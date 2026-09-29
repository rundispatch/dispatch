//! `dispatch serve`: the repo-scoped foreground owner loop for foreign and
//! orphaned attached Work (`docs/plan-0.3-auto-apply-and-attach.md`, parts
//! 6.7, 6.9, 14.4, 14.11). The fixture mirrors `tests/attach_cli.rs`: a Git
//! repository (`root`) with `checks.verify: ['true']` and a fast
//! `coherence.poll_secs: 1`, plus a linked worktree (`workspace`) on a new
//! branch that a test edits directly, playing the role of an already-running
//! agent Dispatch did not launch.
#![cfg(unix)]

use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use assert_cmd::cargo_bin_cmd;
use serde_json::Value;

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// The external-agent fixture for wrapped attach, as in
/// `tests/attach_wrapped.rs`: writes a marker to `src/lib.rs`, announces
/// itself via `$READY`, then blocks on the polled `$GATE` marker file until
/// the test releases it (or forever, when `GATE` is unset, for the adoption
/// scenario where the test kills the wrapper without ever releasing it).
const WRAPPED_AGENT_SCRIPT: &str = r#"#!/bin/sh
trap 'exit 143' TERM
printf 'pub fn f() -> i32 {\n    2\n}\n' > src/lib.rs
: > "$READY"
if [ -n "$GATE" ]; then
    while [ ! -f "$GATE" ]; do sleep 0.1; done
fi
exit "${EXIT_CODE:-0}"
"#;

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    state: PathBuf,
    wrapped_agent_script: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn f() -> i32 {\n    1\n}\n").unwrap();
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

        let wrapped_agent_script = temp.path().join("wrapped-agent.sh");
        executable(&wrapped_agent_script, WRAPPED_AGENT_SCRIPT);

        let state = temp.path().join("state");
        Self {
            root: fs::canonicalize(&root).unwrap(),
            workspace: fs::canonicalize(&workspace).unwrap(),
            state,
            wrapped_agent_script,
            _temp: temp,
        }
    }

    /// `dispatch attach --workspace <workspace> --root <root>
    /// --allow-unsafe-local -- sh <wrapped_agent_script>` (wrapped form), not
    /// yet spawned, stdio piped so the test can observe it.
    fn attach_wrapped_command(&self, workspace: &Path) -> Command {
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
            .arg("--")
            .arg("sh")
            .arg(&self.wrapped_agent_script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Empty before the first `dispatch` invocation has created and migrated
    /// `dispatch.db` (the state directory itself does not exist yet).
    fn all_run_ids(&self) -> HashSet<String> {
        let Ok(db) = rusqlite::Connection::open(self.state.join("dispatch.db")) else {
            return HashSet::new();
        };
        let Ok(mut statement) = db.prepare("SELECT id FROM runs") else {
            return HashSet::new();
        };
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    /// Waits for exactly one run ID to appear beyond `before`, returning it.
    /// Wrapped attach prints nothing while the agent runs, unlike the
    /// foreign form's `ATTACHED <id>` banner.
    fn wait_new_run_id(&self, before: &HashSet<String>) -> String {
        let mut found = None;
        wait_until(Duration::from_secs(30), || {
            let ids = self.all_run_ids();
            let mut diff = ids.difference(before);
            match (diff.next(), diff.next()) {
                (Some(id), None) => {
                    found = Some(id.clone());
                    true
                }
                _ => false,
            }
        });
        found.unwrap()
    }

    /// A second linked worktree of the same repository, on its own branch.
    fn extra_worktree(&self, name: &str) -> PathBuf {
        let path = self._temp.path().join(name);
        git(
            &self.root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                &format!("{name}-branch"),
                path.to_str().unwrap(),
            ],
        );
        fs::canonicalize(&path).unwrap()
    }

    fn dispatch(&self, args: &[&str]) -> assert_cmd::assert::Assert {
        cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&self.state)
            .args(args)
            .assert()
    }

    /// `dispatch attach --workspace <workspace> --root <root>
    /// --allow-unsafe-local <extra...>`, returning the new run's ID.
    fn attach_workspace(&self, workspace: &Path, extra: &[&str]) -> String {
        let mut args = vec![
            "attach",
            "--workspace",
            workspace.to_str().unwrap(),
            "--root",
            self.root.to_str().unwrap(),
            "--allow-unsafe-local",
        ];
        args.extend_from_slice(extra);
        let assert = self.dispatch(&args).success();
        Self::stdout(&assert)
            .lines()
            .find_map(|line| line.strip_prefix("ATTACHED "))
            .expect("attach prints ATTACHED <id>")
            .to_owned()
    }

    fn attach(&self, extra: &[&str]) -> String {
        self.attach_workspace(&self.workspace, extra)
    }

    fn metadata_path(&self, id: &str) -> PathBuf {
        self.state.join("runs").join(id).join("metadata.json")
    }

    fn metadata(&self, id: &str) -> Value {
        serde_json::from_slice(&fs::read(self.metadata_path(id)).unwrap()).unwrap()
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.state.join("dispatch.db")).unwrap()
    }

    fn event_count(&self, id: &str, kind: &str) -> i64 {
        self.db()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_type = ?2",
                rusqlite::params![id, kind],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn stdout(assert: &assert_cmd::assert::Assert) -> String {
        String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
    }

    fn stderr(assert: &assert_cmd::assert::Assert) -> String {
        String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
    }
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

fn wait_until(timeout: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "condition not met in {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A background `dispatch serve` process: stdout lines are streamed to a
/// channel by a reader thread so the test can poll with a deadline instead
/// of blocking forever. Killed and reaped on drop, so a panicking assertion
/// never leaves a `serve` process (and its lock) behind.
struct ServeProcess {
    child: std::process::Child,
    lines: mpsc::Receiver<String>,
}

impl ServeProcess {
    fn spawn(fixture: &Fixture, extra: &[&str]) -> Self {
        let mut command = Command::new(assert_cmd::cargo_bin!("dispatch"));
        command
            .arg("--state-dir")
            .arg(&fixture.state)
            .arg("serve")
            .arg("--root")
            .arg(&fixture.root)
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("failed to start dispatch serve");
        let stdout = child.stdout.take().expect("serve stdout is piped");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self { child, lines: rx }
    }

    fn next_line(&self, timeout: Duration) -> Option<String> {
        self.lines.recv_timeout(timeout).ok()
    }

    /// The first line matching `predicate`, waiting up to `timeout` total.
    fn wait_for(
        &self,
        timeout: Duration,
        mut predicate: impl FnMut(&Value) -> bool,
    ) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let line = self.next_line(remaining)?;
            if let Ok(value) = serde_json::from_str::<Value>(&line)
                && predicate(&value)
            {
                return Some(value);
            }
        }
    }
}

impl Drop for ServeProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A spawned wrapped-attach process, killed on drop so a failing assertion
/// never leaves an orphaned `dispatch attach` running.
struct OwnedChild(Child);

impl OwnedChild {
    fn kill_and_wait(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn second_serve_on_the_same_root_is_refused() {
    let fixture = Fixture::new();

    let first = ServeProcess::spawn(&fixture, &["--json"]);
    let world = first.wait_for(Duration::from_secs(10), |value| value["type"] == "world");
    assert!(world.is_some(), "serve did not print a world line in time");

    let second = fixture
        .dispatch(&["serve", "--root", fixture.root.to_str().unwrap(), "--json"])
        .failure();
    assert!(
        Fixture::stderr(&second).contains("already serving this root"),
        "{}",
        Fixture::stderr(&second)
    );

    drop(first);

    let third = ServeProcess::spawn(&fixture, &["--json"]);
    let world = third.wait_for(Duration::from_secs(10), |value| value["type"] == "world");
    assert!(world.is_some(), "a third serve did not start cleanly");
}

#[test]
fn serve_reevaluates_foreign_work_when_the_root_moves() {
    let fixture = Fixture::new();
    let id = fixture.attach(&[]);
    fs::write(
        fixture.workspace.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();

    let serve = ServeProcess::spawn(&fixture, &["--json"]);
    serve.wait_for(Duration::from_secs(10), |value| value["type"] == "world");

    // The root moves underneath the active attached work, editing the same
    // line to a third, conflicting value.
    fs::write(
        fixture.root.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    3\n}\n",
    )
    .unwrap();

    wait_until(Duration::from_secs(30), || {
        fixture.event_count(&id, "coherence.invalidated") > 0
    });
}

fn stored_decision(fixture: &Fixture, id: &str) -> Value {
    fixture.metadata(id)["coherence"]["validity"]["decision"].clone()
}

/// Foreign work edits line 2 of `src/lib.rs`; the root edits the same line
/// to another value, so the work goes stale.
fn conflict_in_root(fixture: &Fixture) {
    fs::write(
        fixture.root.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    3\n}\n",
    )
    .unwrap();
}

fn restore_root(fixture: &Fixture) {
    git(&fixture.root, &["checkout", "--", "src/lib.rs"]);
}

fn stale_foreign_work(fixture: &Fixture) -> String {
    let id = fixture.attach(&[]);
    fs::write(
        fixture.workspace.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    id
}

#[test]
fn a_verdict_that_changes_back_within_a_minute_is_still_recorded() {
    let fixture = Fixture::new();
    let id = stale_foreign_work(&fixture);
    let serve = ServeProcess::spawn(&fixture, &["--json"]);
    serve.wait_for(Duration::from_secs(10), |value| value["type"] == "world");
    conflict_in_root(&fixture);
    wait_until(Duration::from_secs(30), || {
        stored_decision(&fixture, &id) == "refresh"
    });
    // The root moves back at once and then stays put: the owner must record
    // CONTINUE without waiting for yet another move.
    restore_root(&fixture);
    wait_until(Duration::from_secs(30), || {
        stored_decision(&fixture, &id) == "continue"
    });
}

#[test]
fn a_restarted_owner_records_a_change_the_old_one_never_saw() {
    let fixture = Fixture::new();
    let id = stale_foreign_work(&fixture);
    let serve = ServeProcess::spawn(&fixture, &["--json"]);
    serve.wait_for(Duration::from_secs(10), |value| value["type"] == "world");
    conflict_in_root(&fixture);
    wait_until(Duration::from_secs(30), || {
        stored_decision(&fixture, &id) == "refresh"
    });
    drop(serve);

    restore_root(&fixture);
    let serve = ServeProcess::spawn(&fixture, &["--json"]);
    serve.wait_for(Duration::from_secs(10), |value| value["type"] == "world");
    wait_until(Duration::from_secs(30), || {
        stored_decision(&fixture, &id) == "continue"
    });
}

#[test]
fn serve_follows_foreign_work_that_edits_into_a_change_already_made() {
    let fixture = Fixture::new();
    let id = fixture.attach(&[]);
    let serve = ServeProcess::spawn(&fixture, &["--json"]);
    serve.wait_for(Duration::from_secs(10), |value| value["type"] == "world");
    // The root moves first, while the work has not touched the code yet.
    conflict_in_root(&fixture);
    serve.wait_for(Duration::from_secs(10), |value| value["type"] == "world");
    // Then the work edits the same line, and the root stays put.
    fs::write(
        fixture.workspace.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    wait_until(Duration::from_secs(30), || {
        stored_decision(&fixture, &id) == "refresh"
    });
}

#[test]
fn serve_auto_applies_ready_foreign_work_with_integrate() {
    let fixture = Fixture::new();

    let id = fixture.attach(&["--auto-apply"]);
    fs::write(
        fixture.workspace.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    fixture.dispatch(&["finish", &id]).success();

    let _serve = ServeProcess::spawn(&fixture, &["--json"]);
    wait_until(Duration::from_secs(30), || {
        let metadata = fixture.metadata(&id);
        metadata["outcome"]["application"] == "applied"
            && metadata["outcome"]["applied_by"] == "auto_apply"
    });

    // A second Work, attached without --auto-apply and finished, is not
    // applied after two ticks (poll_secs is 1s).
    let workspace2 = fixture.extra_worktree("wt2");
    let id2 = fixture.attach_workspace(&workspace2, &[]);
    fs::write(
        workspace2.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    4\n}\n",
    )
    .unwrap();
    fixture.dispatch(&["finish", &id2]).success();

    std::thread::sleep(Duration::from_millis(2_500));
    let metadata2 = fixture.metadata(&id2);
    assert_eq!(metadata2["outcome"]["application"], "not_applied");
}

/// A wrapped attach (part 6.7/S4) records its own process as `attachment.owner`
/// with `owner_state: Live`. Killing the wrapper (not the agent it spawned)
/// leaves the agent running, orphaned, while the stored owner is now dead:
/// `serve` must adopt that Work exactly once (`attach.adopted`,
/// `owner_state: adopted`), and must apply or finish nothing, since adoption
/// is observation only (part 6.9's "Active Work after `serve` restart" row).
#[test]
fn serve_adopts_work_whose_owner_is_gone() {
    let fixture = Fixture::new();
    let ready = fixture._temp.path().join("ready");
    // Never created: the agent blocks on it forever, so it is still running,
    // orphaned, once the wrapper is killed below.
    let gate = fixture._temp.path().join("gate-never-released");
    let before = fixture.all_run_ids();

    let mut wrapped = OwnedChild(
        fixture
            .attach_wrapped_command(&fixture.workspace)
            .env("READY", &ready)
            .env("GATE", &gate)
            .spawn()
            .expect("failed to start dispatch attach"),
    );
    let wrapper_pid = wrapped.0.id() as i32;
    wait_until(Duration::from_secs(30), || ready.exists());
    let id = fixture.wait_new_run_id(&before);

    // The wrapper records `attachment.agent_process` and syncs it to disk
    // right after spawning the child, before the child could plausibly reach
    // its own READY marker; under heavy parallel load the wrapper's own
    // commit can still lag the child briefly, so poll rather than assume.
    let mut agent_pid = None;
    wait_until(Duration::from_secs(30), || {
        agent_pid = fixture.metadata(&id)["attachment"]["agent_process"]["pid"].as_u64();
        agent_pid.is_some()
    });
    let agent_pid = agent_pid.unwrap() as i32;

    // Kill the wrapper only; the agent it spawned, never signaled, stays
    // alive and blocked forever on its unset GATE.
    assert_eq!(unsafe { libc::kill(wrapper_pid, libc::SIGKILL) }, 0);
    wrapped.kill_and_wait();
    assert!(alive(agent_pid), "the orphaned agent must still be running");

    let serve = ServeProcess::spawn(&fixture, &["--json"]);
    wait_until(Duration::from_secs(30), || {
        fixture.event_count(&id, "attach.adopted") > 0
    });
    assert_eq!(fixture.event_count(&id, "attach.adopted"), 1);

    let metadata = fixture.metadata(&id);
    assert_eq!(metadata["attachment"]["owner_state"], "adopted");
    assert_eq!(metadata["outcome"]["lifecycle"], "working");
    assert_eq!(fixture.event_count(&id, "result.applied"), 0);
    assert_eq!(fixture.event_count(&id, "run.finished"), 0);
    drop(serve);

    // Cleanup: the orphaned agent would otherwise block on its gate forever.
    unsafe { libc::kill(agent_pid, libc::SIGKILL) };
    wait_until(Duration::from_secs(10), || !alive(agent_pid));
}

#[test]
fn serve_view_lists_native_and_attached_runs() {
    let fixture = Fixture::new();

    let native_id = {
        let assert = fixture
            .dispatch(&[
                "run",
                fixture.root.to_str().unwrap(),
                "--allow-unsafe-local",
                "--task",
                "Create the fake artifact.",
                "--agent",
                "fake-good",
            ])
            .success();
        Fixture::stdout(&assert)
            .lines()
            .find_map(|line| line.strip_prefix("RUN "))
            .expect("run prints RUN <id>")
            .trim()
            .to_owned()
    };
    let attached_id = fixture.attach(&[]);

    let serve = ServeProcess::spawn(&fixture, &["--json"]);
    let mut seen = std::collections::HashMap::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while seen.len() < 2 && Instant::now() < deadline {
        let Some(line) = serve.next_line(Duration::from_secs(1)) else {
            continue;
        };
        if let Ok(value) = serde_json::from_str::<Value>(&line)
            && value["type"] == "work"
            && let Some(run_id) = value["run_id"].as_str()
        {
            seen.insert(run_id.to_owned(), value.clone());
        }
    }
    // Each row says where the work came from, which agent did it and what it
    // began against; a native row names its agent, never "dispatch".
    let native = seen.get(&native_id).expect("native row");
    assert_eq!(native["origin"], "native", "{native}");
    assert_eq!(native["agent"], "fake-good", "{native}");
    assert!(
        native["s0"].as_str().unwrap().starts_with("snapshot "),
        "{native}"
    );
    assert_eq!(native["review"], "pending", "{native}");
    let attached = seen.get(&attached_id).expect("attached row");
    assert_eq!(attached["origin"], "attached", "{attached}");
    assert!(
        attached["s0"].as_str().unwrap().starts_with("merge-base "),
        "{attached}"
    );
    assert!(attached["verification"].is_string(), "{attached}");
}

/// A `dispatch -vv serve --background` owner whose `tick:` diagnostics are
/// streamed to a channel; killed on drop.
struct Diagnosed {
    child: Child,
    ticks: mpsc::Receiver<String>,
}

impl Diagnosed {
    fn spawn(fixture: &Fixture) -> Self {
        let mut child = Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&fixture.state)
            .args(["-vv", "serve", "--background", "--root"])
            .arg(&fixture.root)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(tick) = line.split_once("tick: ").map(|(_, tick)| tick.to_owned())
                    && tx.send(tick).is_err()
                {
                    break;
                }
            }
        });
        Self { child, ticks: rx }
    }

    fn next(&self) -> String {
        self.ticks
            .recv_timeout(Duration::from_secs(30))
            .expect("a tick line")
    }
}

impl Drop for Diagnosed {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn an_idle_tick_evaluates_nothing() {
    let fixture = Fixture::new();
    let _id = stale_foreign_work(&fixture);
    let owner = Diagnosed::spawn(&fixture);
    let first = owner.next();
    assert!(first.contains("1 evaluated"), "{first}");
    for _ in 0..3 {
        let idle = owner.next();
        assert!(idle.contains("1 followed"), "{idle}");
        assert!(idle.contains("0 evaluated"), "{idle}");
        assert!(idle.contains("0 rechecked"), "{idle}");
    }
}

/// Where the owner's time goes as a project grows: a Git root of 2,000 files
/// with 1, 5 and 20 foreign attached Work items, idle and after the root
/// moves. Prints its measurements; asserts nothing about time.
/// `cargo test --test serve serve_tick_cost -- --ignored --nocapture`
#[test]
#[ignore]
fn serve_tick_cost() {
    for items in [1, 5, 20] {
        let fixture = Fixture::new();
        for n in 0..2000 {
            let dir = fixture.root.join(format!("pkg{}", n / 100));
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join(format!("m{n}.rs")),
                format!("pub fn f{n}() {{}}\n"),
            )
            .unwrap();
        }
        git(&fixture.root, &["add", "-A"]);
        git(&fixture.root, &["commit", "--quiet", "-m", "grow"]);
        for item in 0..items {
            let workspace = fixture.extra_worktree(&format!("w{item}"));
            fixture.attach_workspace(&workspace, &[]);
            fs::write(
                workspace.join(format!("pkg0/m{item}.rs")),
                "pub fn changed() {}\n",
            )
            .unwrap();
        }
        let owner = Diagnosed::spawn(&fixture);
        let first = owner.next();
        let idle: Vec<String> = (0..3).map(|_| owner.next()).collect();
        fs::write(fixture.root.join("pkg1/m100.rs"), "pub fn moved() {}\n").unwrap();
        let moved = owner.next();
        println!("{items} items, first: {first}");
        for line in idle {
            println!("{items} items, idle:  {line}");
        }
        println!("{items} items, moved: {moved}");
    }
}

/// What comparing Work costs the owner: `n` attached Work items on the
/// 2,000-file fixture, each changing its own module and calling one shared
/// function whose signature every fifth item changes. Prints the owner's tick
/// lines: the first derives every footprint, idle ticks reuse them, and a tick
/// after one item edits derives only that one again.
/// `cargo test --test serve interactions_cost -- --ignored --nocapture`
#[test]
#[ignore]
fn interactions_cost() {
    let sizes: Vec<usize> = std::env::var("DISPATCH_COST_ITEMS")
        .map(|items| items.split(',').map(|n| n.parse().unwrap()).collect())
        .unwrap_or_else(|_| vec![1, 5, 20, 50]);
    for items in sizes {
        let fixture = Fixture::new();
        for n in 0..2000 {
            let dir = fixture.root.join(format!("pkg{}", n / 100));
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join(format!("m{n}.rs")),
                format!("pub fn f{n}(x: u32) -> u32 {{\n    x + {n}\n}}\n"),
            )
            .unwrap();
        }
        git(&fixture.root, &["add", "-A"]);
        git(&fixture.root, &["commit", "--quiet", "-m", "grow"]);
        let mut first_workspace = None;
        for item in 0..items {
            let workspace = fixture.extra_worktree(&format!("w{item}"));
            fixture.attach_workspace(&workspace, &[]);
            fs::write(
                workspace.join(format!("pkg1/m{}.rs", 100 + item)),
                format!(
                    "pub fn f{}(x: u32) -> u32 {{\n    f1999(x) + 1\n}}\n",
                    100 + item
                ),
            )
            .unwrap();
            if item % 5 == 0 {
                fs::write(
                    workspace.join("pkg19/m1999.rs"),
                    "pub fn f1999(x: u32, y: u32) -> u32 {\n    x + y\n}\n",
                )
                .unwrap();
            }
            first_workspace.get_or_insert(workspace);
        }
        let owner = Diagnosed::spawn(&fixture);
        // The first tick derives everything, which at 50 items takes longer
        // than the usual wait for a tick.
        let first = owner.ticks.recv_timeout(Duration::from_secs(300)).unwrap();
        let idle: Vec<String> = (0..2).map(|_| owner.next()).collect();
        fs::write(
            first_workspace.unwrap().join("pkg2/m200.rs"),
            "pub fn f200(x: u32) -> u32 {\n    x\n}\n",
        )
        .unwrap();
        let edited: Vec<String> = (0..2).map(|_| owner.next()).collect();
        println!("{items} items, first:  {first}");
        for line in idle {
            println!("{items} items, idle:   {line}");
        }
        for line in edited {
            println!("{items} items, edited: {line}");
        }
    }
}
