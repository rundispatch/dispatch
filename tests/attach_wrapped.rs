//! End-to-end tests for the wrapped attach owner loop
//! (`dispatch attach -- <agent command>`), part 6.7 of
//! `docs/plan-0.3-auto-apply-and-attach.md`. The fixture agent is a shell
//! script that proves stdin is inherited (it reads one line and records it),
//! edits `src/lib.rs` in its cwd, optionally blocks on a release file so the
//! test can move the source or send a signal while it "runs", and exits with
//! a code the test controls.
#![cfg(unix)]

use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    state: PathBuf,
    agent_script: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn f() -> i32 {\n    1\n}\n").unwrap();
        fs::write(root.join(".gitignore"), "build/\n").unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::write(root.join("build/out.bin"), b"original build output\n").unwrap();
        fs::write(
            root.join("dispatch.yml"),
            "checks:\n  verify: ['true']\ncoherence:\n  poll_secs: 1\n",
        )
        .unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);

        let workspace = temp.path().join("wt");
        git(
            &root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "agent-branch",
                workspace.to_str().unwrap(),
            ],
        );

        let agent_script = temp.path().join("agent.sh");
        executable(
            &agent_script,
            r#"#!/bin/sh
trap 'exit 143' TERM
read line
printf '%s' "$line" > "$STDIN_CAPTURE"
printf 'pub fn f() -> i32 {\n    2\n}\n' > src/lib.rs
: > "$READY"
if [ -n "$GATE" ]; then
    while [ ! -f "$GATE" ]; do sleep 0.2; done
fi
exit "${EXIT_CODE:-0}"
"#,
        );

        let state = temp.path().join("state");
        Self {
            _temp: temp,
            root: fs::canonicalize(root).unwrap(),
            workspace: fs::canonicalize(workspace).unwrap(),
            state,
            agent_script,
        }
    }

    /// A plain-directory workspace (no `.git`), for the plain-directory
    /// baseline test. Not a linked worktree, so `--root` is required.
    fn plain_workspace(&self) -> PathBuf {
        let plain = self._temp.path().join("plain");
        fs::create_dir_all(plain.join("src")).unwrap();
        fs::write(plain.join("src/lib.rs"), "pub fn f() -> i32 {\n    1\n}\n").unwrap();
        fs::canonicalize(plain).unwrap()
    }

    /// A `dispatch attach --workspace <workspace> --root <root>
    /// --allow-unsafe-local <extra...> -- sh <agent.sh>` command, not yet
    /// spawned, with stdin/stdout/stderr piped so the test can drive and
    /// observe it.
    fn attach_command(&self, workspace: &Path, extra: &[&str]) -> Command {
        let mut command = Command::new(assert_cmd::cargo_bin!("dispatch"));
        command
            .arg("--state-dir")
            .arg(&self.state)
            .arg("attach")
            .arg("--workspace")
            .arg(workspace)
            .arg("--root")
            .arg(&self.root)
            .arg("--allow-unsafe-local")
            .args(extra)
            .arg("--")
            .arg("sh")
            .arg(&self.agent_script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn metadata_path(&self, id: &str) -> PathBuf {
        self.state.join("runs").join(id).join("metadata.json")
    }

    fn metadata(&self, id: &str) -> Value {
        serde_json::from_slice(&fs::read(self.metadata_path(id)).unwrap()).unwrap()
    }

    fn delta_patch(&self, id: &str) -> String {
        fs::read_to_string(self.state.join("runs").join(id).join("delta.patch")).unwrap()
    }

    /// The run this fixture's state directory holds: every test attaches
    /// exactly once, so there is exactly one run directory.
    fn only_run_id(&self) -> String {
        let mut entries: Vec<String> = fs::read_dir(self.state.join("runs"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one run: {entries:?}");
        entries.remove(0)
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.state.join("dispatch.db")).unwrap()
    }

    fn event_count(&self, kind: &str) -> i64 {
        self.db()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE event_type = ?1",
                [kind],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn dispatch(&self, args: &[&str]) -> Output {
        Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&self.state)
            .args(args)
            .output()
            .unwrap()
    }
}

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn wait_until(mut ready: impl FnMut() -> bool, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(Instant::now() < deadline, "{message}");
        thread::sleep(Duration::from_millis(50));
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// A spawned wrapper process, killed on drop so a failing assertion never
/// leaves an orphaned `dispatch attach` (or its agent) running.
struct OwnedChild(Child);

impl OwnedChild {
    fn write_stdin_line(&mut self, line: &str) {
        let stdin = self.0.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
    }

    fn finish(mut self) -> Output {
        use std::io::Read;
        self.0.stdin.take();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        self.0
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut stdout)
            .unwrap();
        self.0
            .stderr
            .take()
            .unwrap()
            .read_to_end(&mut stderr)
            .unwrap();
        let status = self.0.wait().unwrap();
        Output {
            status,
            stdout,
            stderr,
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn wrapped_attach_runs_the_agent_with_inherited_stdio_and_finishes() {
    let f = Fixture::new();
    let stdin_capture = f._temp.path().join("stdin-capture");
    let ready = f._temp.path().join("ready");

    let mut command = f.attach_command(&f.workspace, &[]);
    command
        .env("STDIN_CAPTURE", &stdin_capture)
        .env("READY", &ready)
        .env_remove("GATE")
        .env("EXIT_CODE", "0");
    let mut child = OwnedChild(command.spawn().unwrap());
    child.write_stdin_line("hello");

    let output = child.finish();
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(fs::read_to_string(&stdin_capture).unwrap(), "hello");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let id = f.only_run_id();
    assert!(
        stdout.starts_with(&format!("Attached work {id} finished")),
        "stdout did not begin with the post-exit line: {stdout:?}"
    );

    let metadata = f.metadata(&id);
    assert_eq!(metadata["outcome"]["work_result"], "ready");
    assert_eq!(metadata["outcome"]["verification"], "passed");
    assert_eq!(
        metadata["attachment"]["finish_reason"],
        serde_json::json!({"process_exit": {"code": 0}})
    );

    let patch = f.delta_patch(&id);
    assert!(patch.contains("+    2"), "{patch}");
}

#[test]
fn wrapped_attach_records_a_verdict_when_the_root_moves() {
    let f = Fixture::new();
    let ready = f._temp.path().join("ready");
    let gate = f._temp.path().join("gate");

    let mut command = f.attach_command(&f.workspace, &[]);
    command
        .env("STDIN_CAPTURE", f._temp.path().join("stdin-capture"))
        .env("READY", &ready)
        .env("GATE", &gate)
        .env("EXIT_CODE", "0");
    let mut child = OwnedChild(command.spawn().unwrap());
    child.write_stdin_line("hello");

    wait_until(
        || ready.exists(),
        "agent never reached its gate (READY marker missing)",
    );

    // The root moves underneath the running agent: the same line is edited
    // to a third, conflicting value.
    fs::write(f.root.join("src/lib.rs"), "pub fn f() -> i32 {\n    3\n}\n").unwrap();

    wait_until(
        || f.event_count("coherence.invalidated") > 0,
        "no coherence.invalidated event was recorded for the moved root",
    );

    fs::write(&gate, "go\n").unwrap();
    let output = child.finish();
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let id = f.only_run_id();
    let check = f.dispatch(&["check", &id]);
    let stdout = String::from_utf8_lossy(&check.stdout);
    assert!(stdout.contains("Coherence: REFRESH"), "{stdout}");
}

#[test]
fn wrapped_attach_with_integrate_auto_applies() {
    let f = Fixture::new();
    let ready = f._temp.path().join("ready");

    let mut command = f.attach_command(&f.workspace, &["--auto-apply"]);
    command
        .env("STDIN_CAPTURE", f._temp.path().join("stdin-capture"))
        .env("READY", &ready)
        .env_remove("GATE")
        .env("EXIT_CODE", "0");
    let mut child = OwnedChild(command.spawn().unwrap());
    child.write_stdin_line("hello");

    let output = child.finish();
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Auto-applied"), "{stdout}");

    let id = f.only_run_id();
    let metadata = f.metadata(&id);
    assert_eq!(metadata["status"], "applied");
    assert_eq!(metadata["outcome"]["application"], "applied");
    assert_eq!(metadata["outcome"]["applied_by"], "auto_apply");
    assert_eq!(metadata["outcome"]["review"], "pending");

    assert_eq!(
        fs::read_to_string(f.root.join("src/lib.rs")).unwrap(),
        "pub fn f() -> i32 {\n    2\n}\n"
    );
}

#[test]
fn wrapped_attach_nonzero_exit_still_finishes() {
    let f = Fixture::new();
    let ready = f._temp.path().join("ready");

    let mut command = f.attach_command(&f.workspace, &[]);
    command
        .env("STDIN_CAPTURE", f._temp.path().join("stdin-capture"))
        .env("READY", &ready)
        .env_remove("GATE")
        .env("EXIT_CODE", "3");
    let mut child = OwnedChild(command.spawn().unwrap());
    child.write_stdin_line("hello");

    let output = child.finish();
    assert_eq!(output.status.code(), Some(3), "{output:?}");

    let id = f.only_run_id();
    let metadata = f.metadata(&id);
    assert_eq!(metadata["status"], "ready_for_evaluation");
    assert_eq!(metadata["candidates"][0]["exit_code"], 3);
}

#[test]
fn wrapped_attach_sigterm_is_forwarded() {
    let f = Fixture::new();
    let ready = f._temp.path().join("ready");
    let gate = f._temp.path().join("gate");

    let mut command = f.attach_command(&f.workspace, &[]);
    command
        .env("STDIN_CAPTURE", f._temp.path().join("stdin-capture"))
        .env("READY", &ready)
        .env("GATE", &gate)
        .env("EXIT_CODE", "0");
    let mut child = OwnedChild(command.spawn().unwrap());
    let wrapper_pid = child.0.id() as i32;
    child.write_stdin_line("hello");

    wait_until(
        || ready.exists(),
        "agent never reached its gate (READY marker missing)",
    );

    // SAFETY: `wrapper_pid` is the `dispatch attach` process this test just
    // spawned and still owns.
    let killed = unsafe { libc::kill(wrapper_pid, libc::SIGTERM) };
    assert_eq!(killed, 0, "failed to signal the wrapper");

    let output = child.finish();
    assert_eq!(
        output.status.code(),
        Some(143),
        "wrapper did not exit with the forwarded agent's exit code: {output:?}"
    );

    let id = f.only_run_id();
    let metadata = f.metadata(&id);
    assert_eq!(
        metadata["attachment"]["finish_reason"],
        serde_json::json!({"process_exit": {"code": 143}})
    );

    let agent_pid = metadata["attachment"]["agent_process"]["pid"]
        .as_u64()
        .expect("agent_process.pid recorded") as i32;
    wait_until(
        || !alive(agent_pid),
        "the agent process was not reaped after the wrapper forwarded SIGTERM",
    );
}

#[test]
fn wrapped_attach_of_a_plain_directory_snapshots_the_workspace() {
    let f = Fixture::new();
    let plain = f.plain_workspace();
    let ready = f._temp.path().join("ready");

    let mut command = f.attach_command(&plain, &[]);
    command
        .env("STDIN_CAPTURE", f._temp.path().join("stdin-capture"))
        .env("READY", &ready)
        .env_remove("GATE")
        .env("EXIT_CODE", "0");
    let mut child = OwnedChild(command.spawn().unwrap());
    child.write_stdin_line("hello");

    let output = child.finish();
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let id = f.only_run_id();
    let metadata = f.metadata(&id);
    assert_eq!(
        metadata["attachment"]["provenance"],
        serde_json::json!("snapshot_at_attach")
    );
    assert_eq!(metadata["attachment"]["confidence"], "partial");
    assert_eq!(metadata["attachment"]["repo_key"], Value::Null);
}

impl Fixture {
    /// Wrapped attach started in the checkout itself: Dispatch makes the
    /// workspace. Returns the run and the wrapper's output.
    fn attach_from_the_checkout(&self, workspace: &Path) -> (String, Output) {
        let mut command = self.attach_command(workspace, &[]);
        command
            .env("STDIN_CAPTURE", self._temp.path().join("stdin-capture"))
            .env("READY", self._temp.path().join("ready"))
            .env_remove("GATE")
            .env("EXIT_CODE", "0");
        let mut child = OwnedChild(command.spawn().unwrap());
        child.write_stdin_line("hello");
        let output = child.finish();
        assert!(
            output.status.success(),
            "stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        (self.only_run_id(), output)
    }
}

#[test]
fn wrapped_attach_from_the_checkout_works_in_a_workspace_dispatch_makes() {
    let f = Fixture::new();
    // Uncommitted work in the checkout is part of S0.
    fs::write(f.root.join("notes.txt"), "mine, not committed\n").unwrap();
    let (id, output) = f.attach_from_the_checkout(&f.root);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("your checkout is not touched"), "{stderr}");

    let metadata = f.metadata(&id);
    let attachment = &metadata["attachment"];
    assert_eq!(attachment["workspace_owner"], "dispatch");
    let branch = format!("dispatch/{}", id.to_lowercase());
    assert_eq!(attachment["managed"]["branch"], branch.as_str());
    let workspace = PathBuf::from(attachment["workspace"].as_str().unwrap());
    assert_eq!(
        workspace,
        fs::canonicalize(f.state.join("workspaces"))
            .unwrap()
            .join(&id)
    );
    assert!(!workspace.starts_with(&f.root));
    assert_eq!(attachment["confidence"], "full");
    assert!(attachment["provenance"]["workspace_at_start"]["commit"].is_string());

    // The agent worked in the workspace, which began as the checkout's world.
    assert_eq!(
        fs::read_to_string(workspace.join("notes.txt")).unwrap(),
        "mine, not committed\n"
    );
    assert_eq!(
        fs::read_to_string(f.root.join("src/lib.rs")).unwrap(),
        "pub fn f() -> i32 {\n    1\n}\n"
    );
    let patch = f.delta_patch(&id);
    assert!(
        patch.contains("+    2") && !patch.contains("notes.txt"),
        "{patch}"
    );
    assert_eq!(metadata["outcome"]["work_result"], "ready");

    let status = String::from_utf8_lossy(&f.dispatch(&["status", &id]).stdout).into_owned();
    assert!(status.contains("made by: Dispatch"), "{status}");
    assert!(status.contains(&format!("branch: {branch}")), "{status}");
    let history = String::from_utf8_lossy(&f.dispatch(&["history"]).stdout).into_owned();
    assert!(history.contains("isolated"), "{history}");

    // Accepting applies Δ to the checkout; the workspace is then released.
    let accept = f.dispatch(&["accept", &id]);
    assert!(
        accept.status.success(),
        "{}",
        String::from_utf8_lossy(&accept.stderr)
    );
    assert_eq!(
        fs::read_to_string(f.root.join("src/lib.rs")).unwrap(),
        "pub fn f() -> i32 {\n    2\n}\n"
    );
    assert!(!workspace.exists());
    assert!(!git(&f.root, &["branch", "--list", &branch]).contains("dispatch/"));
    assert_eq!(f.metadata(&id)["attachment"]["managed"]["removed"], true);
    assert_eq!(f.event_count("workspace.released"), 1);
}

#[test]
fn a_rejected_workspace_dispatch_made_is_kept() {
    let f = Fixture::new();
    let (id, _) = f.attach_from_the_checkout(&f.root);
    let workspace = PathBuf::from(f.metadata(&id)["attachment"]["workspace"].as_str().unwrap());
    let reject = f.dispatch(&["reject", &id]);
    assert!(
        reject.status.success(),
        "{}",
        String::from_utf8_lossy(&reject.stderr)
    );
    let stdout = String::from_utf8_lossy(&reject.stdout);
    assert!(
        stdout.contains(&format!("kept at {}", workspace.display())),
        "{stdout}"
    );
    assert!(workspace.join("src/lib.rs").is_file());
    assert_eq!(f.event_count("workspace.released"), 0);
    let history = String::from_utf8_lossy(&f.dispatch(&["history"]).stdout).into_owned();
    assert!(history.contains("rejected"), "{history}");
}

#[test]
fn wrapped_attach_from_a_plain_directory_works_in_a_private_copy() {
    let f = Fixture::new();
    let plain = f.plain_workspace();
    let mut command = Command::new(assert_cmd::cargo_bin!("dispatch"));
    command
        .arg("--state-dir")
        .arg(&f.state)
        .args(["attach", "--workspace"])
        .arg(&plain)
        .arg("--root")
        .arg(&plain)
        .args(["--", "sh"])
        .arg(&f.agent_script)
        .env("STDIN_CAPTURE", f._temp.path().join("stdin-capture"))
        .env("READY", f._temp.path().join("ready"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = OwnedChild(command.spawn().unwrap());
    child.write_stdin_line("hello");
    let output = child.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let id = f.only_run_id();
    let attachment = &f.metadata(&id)["attachment"];
    assert_eq!(attachment["workspace_owner"], "dispatch");
    assert!(attachment["managed"]["branch"].is_null());
    assert_eq!(
        fs::read_to_string(plain.join("src/lib.rs")).unwrap(),
        "pub fn f() -> i32 {\n    1\n}\n"
    );
    assert!(f.delta_patch(&id).contains("+    2"));
}

/// Nothing is removed by a timer: `dispatch clean` lists the workspaces of
/// Work that is over and removes them only when the person says so.
#[test]
fn clean_removes_only_listed_workspaces_of_work_that_is_over() {
    let f = Fixture::new();
    let (rejected, _) = f.attach_from_the_checkout(&f.root);
    assert!(f.dispatch(&["reject", &rejected]).status.success());
    // A second one waits for review: never cleanable.
    let before: std::collections::HashSet<String> = fs::read_dir(f.state.join("runs"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    let mut command = f.attach_command(&f.root, &[]);
    command
        .env("STDIN_CAPTURE", f._temp.path().join("stdin-capture-2"))
        .env("READY", f._temp.path().join("ready-2"))
        .env_remove("GATE")
        .env("EXIT_CODE", "0");
    let mut child = OwnedChild(command.spawn().unwrap());
    child.write_stdin_line("hello");
    assert!(child.finish().status.success());
    let pending = fs::read_dir(f.state.join("runs"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .find(|id| !before.contains(id))
        .unwrap();
    let workspace =
        |id: &str| PathBuf::from(f.metadata(id)["attachment"]["workspace"].as_str().unwrap());
    let text = |output: &Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };

    let dry = f.dispatch(&["clean", "--dry-run"]);
    assert!(dry.status.success(), "{}", text(&dry));
    assert!(
        text(&dry).contains(&workspace(&rejected).display().to_string()),
        "{}",
        text(&dry)
    );
    assert!(
        !text(&dry).contains(&workspace(&pending).display().to_string()),
        "{}",
        text(&dry)
    );
    assert!(workspace(&rejected).exists());

    let unasked = f.dispatch(&["clean"]);
    assert!(!unasked.status.success());
    assert!(text(&unasked).contains("--yes"), "{}", text(&unasked));
    assert!(workspace(&rejected).exists());

    let cleaned = f.dispatch(&["clean", "--yes"]);
    assert!(cleaned.status.success(), "{}", text(&cleaned));
    assert!(!workspace(&rejected).exists());
    assert!(
        workspace(&pending).exists(),
        "work waiting for review keeps its workspace"
    );
    assert_eq!(
        f.metadata(&rejected)["attachment"]["managed"]["removed"],
        true
    );
    let reason: String = f
        .db()
        .query_row(
            "SELECT json_extract(payload_json, '$.reason') FROM events WHERE run_id = ?1 AND event_type = 'workspace.released'",
            [&rejected],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reason, "cleaned");
    let again = f.dispatch(&["clean", "--yes"]);
    assert!(
        text(&again).contains("Nothing to clean"),
        "{}",
        text(&again)
    );
}
