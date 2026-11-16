//! What the command line writes. A command that only prints ends quietly
//! once its reader is gone, while a hook and the project's owner never die
//! of it and a run never leaves its agent behind; and `status` with no Work
//! still lists the worktrees found, not tracked.
#![cfg(unix)]

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

use serde_json::Value;
use sha2::{Digest, Sha256};

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=T", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(300);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The write end of a pipe whose reader is already gone.
fn closed_stdout() -> std::io::PipeWriter {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    writer
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A process the test started, killed if the test ends first.
struct Owned(Child);

impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A Git project with a fast owner poll and a separate state directory.
struct Project {
    temp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
}

impl Project {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "// baseline\n").unwrap();
        fs::write(
            root.join("dispatch.yml"),
            "execution:\n  timeout_secs: 600\ncoherence:\n  poll_secs: 1\n",
        )
        .unwrap();
        git(&root, &["init", "--quiet", "--initial-branch", "trunk"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
        Self {
            root: fs::canonicalize(&root).unwrap(),
            state: temp.path().join("state"),
            temp,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(assert_cmd::cargo_bin!("dispatch"));
        command
            .arg("--state-dir")
            .arg(&self.state)
            .args(args)
            .current_dir(&self.root);
        command
    }

    fn dispatch(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// `args`, its stdout a pipe nobody reads any more.
    fn unread(&self, args: &[&str]) -> Output {
        self.command(args)
            .stdin(Stdio::null())
            .stdout(closed_stdout())
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    }

    fn key(&self) -> String {
        hex::encode(Sha256::digest(self.root.to_string_lossy().as_bytes()))
    }

    fn watcher_file(&self, suffix: &str) -> PathBuf {
        self.state
            .join("watchers")
            .join(format!("{}.{suffix}", self.key()))
    }
}

/// `dispatch history | head -1` and its kind: help, and a command that only
/// reads and prints, end once its reader is gone as a shell pipeline's
/// writers do, killed by `SIGPIPE` and saying nothing.
#[test]
fn a_command_that_only_prints_ends_quietly_when_its_reader_is_gone() {
    let p = Project::new();
    let run = p.dispatch(&[
        "run",
        "--allow-unsafe-local",
        "--agent",
        "fake-good",
        "--json",
        "Create the fake artifact.",
    ]);
    assert!(run.status.success(), "{}", stderr(&run));
    let result: Value = serde_json::from_slice(&run.stdout).unwrap();
    let id = result["run_id"].as_str().unwrap();

    for args in [
        &["version"][..],
        &["--help"],
        &["history"],
        &["status"],
        &["status", "--json"],
        &["status", "--jsonl"],
        &["status", id],
        &["explain"],
        &["show", id],
        &["diff"],
        &["events", id],
        &["resources"],
        &["check", id],
        &["check", id, "--json"],
        &["watch", "--json"],
    ] {
        let output = p.unread(args);
        assert_eq!(
            output.status.signal(),
            Some(libc::SIGPIPE),
            "{args:?}: {}",
            stderr(&output)
        );
        assert!(output.stderr.is_empty(), "{args:?}: {}", stderr(&output));
    }
}

/// A hook writes to its runtime last; a runtime that stopped listening never
/// gets the hook killed for it.
#[test]
fn a_hook_is_never_killed_by_a_closed_pipe() {
    let p = Project::new();
    let mut hook = p
        .command(&["hook", "claude"])
        .stdin(Stdio::piped())
        .stdout(closed_stdout())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Unreadable input: the hook answers with a message on stdout.
    hook.stdin.take().unwrap().write_all(b"not json").unwrap();
    let output = hook.wait_with_output().unwrap();
    assert_eq!(output.status.signal(), None, "{}", stderr(&output));
}

/// The owner, in the foreground or in the background, may be running checks
/// that only it stops, so a closed pipe's `SIGPIPE` never ends it: it goes
/// on until its project is gone, and then ends by itself.
#[test]
fn the_owner_is_never_killed_by_a_closed_pipe() {
    for background in [false, true] {
        let p = Project::new();
        let mut args = vec!["serve", "--root", p.root.to_str().unwrap()];
        if background {
            args.push("--background");
        }
        let mut owner = Owned(
            p.command(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        // Written by its first tick, after it noted where the project is.
        wait_until("the owner's first tick", || {
            p.watcher_file("live.json").exists()
        });
        // SAFETY: the signal goes to the owner this test started.
        assert_eq!(unsafe { libc::kill(owner.0.id() as i32, libc::SIGPIPE) }, 0);
        fs::rename(&p.root, p.temp.path().join("moved")).unwrap();
        wait_until("the owner to end", || owner.0.try_wait().unwrap().is_some());
        let status = owner.0.wait().unwrap();
        assert!(status.success(), "background {background}: {status:?}");
    }
}

/// `run --jsonl` prints the mid-run watcher's verdict while its agent works.
/// With nobody reading any more, the run ends without leaving the agent
/// running.
#[test]
fn a_run_never_leaves_its_agent_behind_when_its_reader_is_gone() {
    let p = Project::new();
    let at = p.temp.path();
    let agent = at.join("codex");
    executable(
        &agent,
        &format!(
            r#"#!/bin/sh
if [ "$1" = '--version' ]; then echo 'codex fixture'; exit 0; fi
if [ "$1" = 'app-server' ]; then
while IFS= read -r line; do
case "$line" in
*'"id":0'*) printf '%s\n' '{{"id":0,"result":{{"userAgent":"fixture"}}}}' ;;
*'"id":1'*) printf '%s\n' '{{"id":1,"result":{{"account":{{"type":"chatgpt","planType":"plus","email":"fixture@example.invalid"}}}}}}' ;;
*'"id":2'*) printf '%s\n' '{{"id":2,"result":{{"rateLimitsByLimitId":{{}}}}}}'; exit 0 ;;
esac
done
exit 0
fi
printf '%s\n' "$$" > '{at}/agent.pid'
printf '// delivered\n' > src/lib.rs
: > '{at}/started'
while [ ! -f '{at}/gate' ]; do sleep 0.1; done
: > '{at}/released'
printf '{{"type":"result"}}\n'
"#,
            at = at.display()
        ),
    );
    fs::write(
        p.root.join("dispatch.yml"),
        format!(
            "execution:\n  timeout_secs: 600\ncoherence:\n  poll_secs: 1\nharnesses:\n  codex:\n    executable: '{}'\n",
            agent.display()
        ),
    )
    .unwrap();
    git(&p.root, &["commit", "--quiet", "-am", "agent"]);
    fs::create_dir_all(&p.state).unwrap();
    fs::write(
        p.state.join("resources.yml"),
        "version: 1\nallocation_enabled: true\ncapacity:\n  codex_probe: false\nprofiles:\n  - provider: openai\n    funding_source: chatgpt-plus\n    harness: codex\n    model: fixture-model\n    effort: low\n    runtime: local\n    service_mode: standard\n    pool: shared\n    tier: light\n    included: true\n    no_overage_verified: true\n    authorization_revision: 1\n    codex_account: {\"account_sha256\":\"cc6d96611cffa9f02c3626f0b9ee897dc171e2d540a5cae349d4ec316104997b\",\"checked_at\":\"2026-01-01T00:00:00Z\"}\n",
    )
    .unwrap();
    let errors = at.join("run.stderr");
    let mut run = Owned(
        p.command(&[
            "run",
            "--allow-unsafe-local",
            "--jsonl",
            "Add a comment in src/lib.rs",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(fs::File::create(&errors).unwrap())
        .spawn()
        .unwrap(),
    );
    let said = || fs::read_to_string(&errors).unwrap_or_default();
    // Read what came before the agent started, so nothing is held back.
    let mut lines = BufReader::new(run.0.stdout.take().unwrap());
    let mut line = String::new();
    while !line.contains("\"attempt.started\"") {
        line.clear();
        assert!(lines.read_line(&mut line).unwrap() > 0, "{}", said());
    }
    wait_until("the agent", || {
        at.join("started").exists() || run.0.try_wait().unwrap().is_some()
    });
    assert!(at.join("started").exists(), "{}", said());
    let pid: i32 = fs::read_to_string(at.join("agent.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    drop(lines);
    // The same line, edited on both sides: a verdict that is printed.
    fs::write(p.root.join("src/lib.rs"), "// teammate\n").unwrap();
    wait_until("dispatch to end", || run.0.try_wait().unwrap().is_some());
    // An agent Dispatch left running goes on once released.
    fs::write(at.join("gate"), "").unwrap();
    wait_until("the agent to end", || !alive(pid));
    assert!(
        !at.join("released").exists(),
        "the agent outlived dispatch: {}",
        said()
    );
}

/// Worktrees found, not tracked, need no Work to be listed: `status` and
/// `status --json` show them in a project with none. With nothing found
/// either, both say what they always said.
#[test]
fn status_lists_worktrees_found_not_tracked_before_any_work() {
    let p = Project::new();
    let found = p.temp.path().join("found worktree");
    git(
        &p.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "found",
            found.to_str().unwrap(),
        ],
    );
    let found = fs::canonicalize(found).unwrap();
    let status = || String::from_utf8(p.dispatch(&["status"]).stdout).unwrap();
    let json = || String::from_utf8(p.dispatch(&["status", "--json"]).stdout).unwrap();

    let none = format!(
        "No Work in {} yet.\n\nProject: not watched · dispatch start\n",
        p.root.display()
    );
    assert_eq!(status(), none);
    assert_eq!(json(), "null\n");

    fs::create_dir_all(p.state.join("watchers")).unwrap();
    fs::write(
        p.watcher_file("inventory.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "entries": [{"path": found, "first_seen": "2026-11-11T00:00:00Z"}],
        }))
        .unwrap(),
    )
    .unwrap();
    let command = format!("dispatch attach --workspace '{}'", found.display());
    assert_eq!(
        status(),
        format!("{none}\nFound, not tracked\n  {command}\n")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&json()).unwrap(),
        serde_json::json!({"found_not_tracked": [{"path": found, "attach": command}]})
    );
}
