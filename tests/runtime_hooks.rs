//! `dispatch hook claude`: Claude Code's session hooks make Work appear by
//! themselves in a watched project. The payloads are the ones Claude Code
//! sends; the worktree sits where `claude --worktree` puts it, inside the
//! checkout under `.claude/worktrees/`.
#![cfg(unix)]

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use serde_json::Value;

struct Project {
    _temp: tempfile::TempDir,
    root: PathBuf,
    worktree: PathBuf,
    state: PathBuf,
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
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
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

impl Project {
    fn new(config: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn f() -> i32 {\n    1\n}\n").unwrap();
        fs::write(
            root.join("dispatch.yml"),
            format!("coherence:\n  poll_secs: 1\n{config}"),
        )
        .unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
        let worktree = root.join(".claude/worktrees/x");
        git(
            &root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "worktree-x",
                ".claude/worktrees/x",
            ],
        );
        Self {
            root: fs::canonicalize(&root).unwrap(),
            worktree: fs::canonicalize(&worktree).unwrap(),
            state: temp.path().join("state"),
            _temp: temp,
        }
    }

    fn dispatch(&self, args: &[&str]) -> Output {
        Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&self.state)
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap()
    }

    fn watch(&self) {
        let output = self.dispatch(&["start"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Run the hook with raw input and return everything it did.
    fn hook_output(&self, input: &[u8]) -> Output {
        let mut child = Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&self.state)
            .args(["hook", "claude"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    fn remove(&self) -> Output {
        self.hook_output(
            serde_json::json!({
                "hook_event_name": "WorktreeRemove", "session_id": "s1", "cwd": self.root,
                "worktree_path": self.worktree, "name": "x",
            })
            .to_string()
            .as_bytes(),
        )
    }

    fn run_dir(&self, id: &str) -> PathBuf {
        self.state.join("runs").join(id)
    }

    /// Run the hook with raw input; the hook itself must always succeed.
    fn hook_raw(&self, input: &[u8]) -> String {
        let mut child = Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&self.state)
            .args(["hook", "claude"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn hook(&self, event: Value) -> String {
        self.hook_raw(event.to_string().as_bytes())
    }

    fn start(&self, session: &str, source: &str, cwd: &Path) -> String {
        self.hook(serde_json::json!({
            "hook_event_name": "SessionStart", "session_id": session, "source": source,
            "cwd": cwd, "transcript_path": "/dev/null", "model": "claude-sonnet-5",
        }))
    }

    fn runs(&self) -> Vec<Value> {
        let Ok(entries) = fs::read_dir(self.state.join("runs")) else {
            return Vec::new();
        };
        entries
            .map(|entry| {
                let path = entry.unwrap().path().join("metadata.json");
                serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
            })
            .collect()
    }

    fn only_run(&self) -> Value {
        let runs = self.runs();
        assert_eq!(runs.len(), 1, "{runs:?}");
        runs.into_iter().next().unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = self.dispatch(&["stop"]);
    }
}

#[test]
fn an_unwatched_project_is_never_touched() {
    let p = Project::new("");
    assert_eq!(p.start("s1", "startup", &p.worktree), "");
    assert!(p.runs().is_empty());
}

#[test]
fn a_session_in_the_checkout_itself_gets_a_notice_and_no_work() {
    let p = Project::new("");
    p.watch();
    let reply: Value = serde_json::from_str(&p.start("s1", "startup", &p.root)).unwrap();
    let notice = reply["systemMessage"].as_str().unwrap();
    assert!(notice.contains("directly in your checkout"), "{notice}");
    assert!(p.runs().is_empty());
}

#[test]
fn a_session_in_its_own_worktree_becomes_work_with_s0_before_its_edits() {
    let p = Project::new("checks:\n  verify: ['true']\n");
    p.watch();
    // The worktree as the session finds it is S0, uncommitted file and all.
    fs::write(p.worktree.join("before.txt"), "there before the session\n").unwrap();
    let reply = p.start("s1", "startup", &p.worktree.join("src"));
    assert!(reply.contains("tracking this worktree"), "{reply}");

    let run = p.only_run();
    let attachment = &run["attachment"];
    assert_eq!(run["mode"], "attached");
    assert_eq!(attachment["workspace"], p.worktree.to_str().unwrap());
    assert_eq!(attachment["integration_root"], p.root.to_str().unwrap());
    assert_eq!(attachment["workspace_owner"], "runtime");
    assert_eq!(attachment["confidence"], "full");
    assert!(attachment["provenance"]["workspace_at_start"]["commit"].is_string());
    assert_eq!(attachment["agent"], "claude");
    assert_eq!(attachment["sessions"][0]["session_id"], "s1");
    assert_eq!(attachment["sessions"][0]["model"], "claude-sonnet-5");
    assert_eq!(run["environment"]["unsafe_local"], false);

    // The session's own edit is Δ; what was there before it is not.
    fs::write(
        p.worktree.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    let id = run["id"].as_str().unwrap();
    let refused = p.dispatch(&["finish", id]);
    assert!(
        !refused.status.success(),
        "checks need a person's authority"
    );
    let finish = p.dispatch(&["finish", id, "--allow-unsafe-local"]);
    assert!(
        finish.status.success(),
        "{}",
        String::from_utf8_lossy(&finish.stderr)
    );
    let patch = fs::read_to_string(p.state.join("runs").join(id).join("delta.patch")).unwrap();
    assert!(
        patch.contains("+    2") && !patch.contains("before.txt"),
        "{patch}"
    );
}

#[test]
fn repeated_and_later_sessions_land_on_one_work() {
    let p = Project::new("");
    p.watch();
    p.start("s1", "startup", &p.worktree);
    p.start("s1", "startup", &p.worktree);
    assert_eq!(p.start("s2", "resume", &p.worktree), "");
    p.hook(serde_json::json!({
        "hook_event_name": "SessionEnd", "session_id": "s1", "reason": "clear",
        "cwd": p.worktree,
    }));

    // The project view reads it as discovered work with no open session.
    let history = text(&p.dispatch(&["history"]));
    assert!(history.contains("discovered claude"), "{history}");
    let run = p.only_run();
    let sessions = run["attachment"]["sessions"].as_array().unwrap();
    let ids: Vec<&str> = sessions
        .iter()
        .map(|s| s["session_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["s1", "s2"]);
    assert_eq!(sessions[0]["end_reason"], "clear");
    assert!(sessions[1]["ended_at"].is_null());
    // An ended session does not end the Work.
    assert_eq!(run["outcome"]["lifecycle"], "working");
}

#[test]
fn a_resume_into_a_worktree_dispatch_has_not_seen_is_partial() {
    let p = Project::new("");
    p.watch();
    p.start("s9", "resume", &p.worktree);
    assert_eq!(p.only_run()["attachment"]["confidence"], "partial");
}

#[test]
fn malformed_hook_input_is_refused_whole_and_never_fails_the_session() {
    let p = Project::new("");
    p.watch();
    for reply in [
        p.start("../escape", "startup", &p.worktree),
        p.hook(serde_json::json!({
            "hook_event_name": "SessionStart", "session_id": "s1", "cwd": "relative/dir",
        })),
        p.hook_raw(&vec![b' '; 70 * 1024]),
        p.hook_raw(b"not json"),
    ] {
        assert!(reply.contains("could not track this session"), "{reply}");
    }
    assert!(p.runs().is_empty());
    // An event Dispatch does not use is silently ignored.
    let other = p.hook(serde_json::json!({
        "hook_event_name": "Stop", "session_id": "s1", "cwd": p.worktree,
    }));
    assert_eq!(other, "");
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn removing_the_worktree_keeps_its_exact_changes_before_the_hook_returns() {
    let p = Project::new("checks:\n  verify: ['test -f src/lib.rs']\n");
    p.watch();
    p.start("s1", "startup", &p.worktree);
    let id = p.only_run()["id"].as_str().unwrap().to_owned();
    fs::write(
        p.worktree.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();

    let output = p.remove();
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        text(&output).contains("kept this worktree's changes"),
        "{}",
        text(&output)
    );
    // Once the hook has returned, the exact Δ and the removal are durable.
    let patch = fs::read_to_string(p.run_dir(&id).join("delta.patch")).unwrap();
    assert!(patch.contains("+    2"), "{patch}");
    let run = p.only_run();
    assert_eq!(run["attachment"]["workspace_removed"]["exact"], true);
    assert_eq!(
        run["outcome"]["lifecycle"], "working",
        "removal does not end the work"
    );
    let status = text(&p.dispatch(&["status", &id]));
    assert!(status.contains("made by: the agent's runtime"), "{status}");
    assert!(status.contains("exact changes are kept"), "{status}");

    // Claude deletes it; the work is still finished and applied from what
    // was kept, its checks run in a workspace rebuilt from S0 and Δ.
    git(
        &p.root,
        &["worktree", "remove", "--force", ".claude/worktrees/x"],
    );
    let finish = p.dispatch(&["finish", &id, "--allow-unsafe-local"]);
    assert!(finish.status.success(), "{}", text(&finish));
    let run = p.only_run();
    assert_eq!(run["outcome"]["work_result"], "ready");
    assert_eq!(run["outcome"]["verification"], "passed");
    let accept = p.dispatch(&["accept", &id]);
    assert!(accept.status.success(), "{}", text(&accept));
    assert_eq!(
        fs::read_to_string(p.root.join("src/lib.rs")).unwrap(),
        "pub fn f() -> i32 {\n    2\n}\n"
    );
}

#[test]
fn a_removed_worktree_with_no_changes_closes() {
    let p = Project::new("");
    p.watch();
    p.start("s1", "startup", &p.worktree);
    let output = p.remove();
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        text(&output).contains("held no changes"),
        "{}",
        text(&output)
    );
    let run = p.only_run();
    assert_eq!(run["outcome"]["lifecycle"], "finished");
    assert_eq!(run["outcome"]["work_result"], "cancelled");
}

#[test]
fn when_the_changes_cannot_be_kept_the_removal_is_stopped() {
    use std::os::unix::fs::PermissionsExt;

    let p = Project::new("");
    p.watch();
    p.start("s1", "startup", &p.worktree);
    let id = p.only_run()["id"].as_str().unwrap().to_owned();
    fs::write(
        p.worktree.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    let run_dir = p.run_dir(&id);
    fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o500)).unwrap();

    let output = p.remove();
    fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!output.status.success(), "{}", text(&output));
    assert!(
        text(&output).contains("stopped the removal"),
        "{}",
        text(&output)
    );
    assert!(p.worktree.join("src/lib.rs").is_file());
    assert!(p.only_run()["attachment"]["workspace_removed"].is_null());
}

#[test]
fn a_worktree_that_vanishes_unannounced_is_noted_and_cannot_be_finished() {
    let p = Project::new("");
    p.watch();
    p.start("s1", "startup", &p.worktree);
    let id = p.only_run()["id"].as_str().unwrap().to_owned();
    fs::write(
        p.worktree.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    // Let the owner follow the edit, then delete the worktree behind its back.
    std::thread::sleep(std::time::Duration::from_secs(3));
    git(
        &p.root,
        &["worktree", "remove", "--force", ".claude/worktrees/x"],
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while p.only_run()["attachment"]["workspace_removed"].is_null() {
        assert!(
            std::time::Instant::now() < deadline,
            "the owner never noticed"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert_eq!(
        p.only_run()["attachment"]["workspace_removed"]["exact"],
        false
    );
    let last = fs::read_to_string(p.run_dir(&id).join("delta-last-seen.patch")).unwrap();
    assert!(last.contains("+    2"), "{last}");
    let finish = p.dispatch(&["finish", &id]);
    assert!(!finish.status.success());
    assert!(
        text(&finish).contains("cannot be finished"),
        "{}",
        text(&finish)
    );
    // Rejecting it is how a person closes it.
    let reject = p.dispatch(&["reject", &id]);
    assert!(reject.status.success(), "{}", text(&reject));
    assert!(text(&reject).contains("Closed"), "{}", text(&reject));
    let run = p.only_run();
    assert_eq!(run["outcome"]["lifecycle"], "finished");
    assert_eq!(run["outcome"]["work_result"], "cancelled");
}

/// Claude Code's `ExitWorktree` tool with `action: remove` deletes the worktree
/// without running `WorktreeRemove`; its `PreToolUse` is the last chance.
#[test]
fn leaving_a_worktree_by_removing_it_keeps_its_exact_changes() {
    let p = Project::new("");
    p.watch();
    p.start("s1", "startup", &p.worktree);
    let id = p.only_run()["id"].as_str().unwrap().to_owned();
    fs::write(
        p.worktree.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    let tool = |name: &str, action: &str| {
        p.hook_output(
            serde_json::json!({
                "hook_event_name": "PreToolUse", "session_id": "s1", "cwd": p.worktree,
                "tool_name": name, "tool_input": {"action": action, "discard_changes": true},
            })
            .to_string()
            .as_bytes(),
        )
    };
    // Other tools, and leaving while keeping the worktree, change nothing.
    for output in [tool("Bash", "remove"), tool("ExitWorktree", "keep")] {
        assert!(output.status.success());
        assert!(output.stdout.is_empty(), "{}", text(&output));
    }
    assert!(p.only_run()["attachment"]["workspace_removed"].is_null());

    let output = tool("ExitWorktree", "remove");
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        text(&output).contains("kept this worktree's changes"),
        "{}",
        text(&output)
    );
    assert_eq!(
        p.only_run()["attachment"]["workspace_removed"]["exact"],
        true
    );
    let patch = fs::read_to_string(p.run_dir(&id).join("delta.patch")).unwrap();
    assert!(patch.contains("+    2"), "{patch}");
}

/// A resumed session reports the checkout before it re-enters its worktree,
/// so only a fresh start is told it works in the checkout.
#[test]
fn a_resumed_session_in_the_checkout_is_not_told_it_is_shared() {
    let p = Project::new("");
    p.watch();
    assert_eq!(p.start("s1", "resume", &p.root), "");
    assert!(p.runs().is_empty());
}

/// A person's `finish --allow-unsafe-local` is the authority the merged-tree
/// checks need at accept: discovered work must not be accepted on the file and
/// symbol analysis alone when the project moved under it.
#[test]
fn accepting_discovered_work_runs_its_checks_on_the_merged_tree() {
    let p = Project::new("checks:\n  verify: ['test ! -f forbidden.txt']\n");
    p.watch();
    p.start("s1", "startup", &p.worktree);
    let id = p.only_run()["id"].as_str().unwrap().to_owned();
    fs::write(
        p.worktree.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    let finish = p.dispatch(&["finish", &id, "--allow-unsafe-local"]);
    assert!(finish.status.success(), "{}", text(&finish));
    assert_eq!(p.only_run()["outcome"]["verification"], "passed");

    // The project moves in a way only the merged tree's checks can see.
    fs::write(p.root.join("forbidden.txt"), "added by someone else\n").unwrap();
    let accept = p.dispatch(&["accept", &id]);
    assert!(
        !accept.status.success(),
        "accepted without its merged-tree checks"
    );
    assert!(
        text(&accept).contains("test ! -f forbidden.txt"),
        "{}",
        text(&accept)
    );
    assert_eq!(
        fs::read_to_string(p.root.join("src/lib.rs")).unwrap(),
        "pub fn f() -> i32 {\n    1\n}\n"
    );
}

/// The project owner holds a run's lock for a moment while it checks the
/// Work; a person's finish waits for it rather than failing.
#[test]
fn finish_waits_for_the_owner_to_let_go_of_the_run() {
    use std::os::fd::AsRawFd;

    let p = Project::new("");
    p.watch();
    p.start("s1", "startup", &p.worktree);
    let id = p.only_run()["id"].as_str().unwrap().to_owned();
    fs::write(
        p.worktree.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(p.run_dir(&id).join(".operation.lock"))
        .unwrap();
    // Held elsewhere for longer than a refusal would take.
    let held = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    assert_eq!(held, 0);
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        drop(lock);
    });
    let finish = p.dispatch(&["finish", &id]);
    release.join().unwrap();
    assert!(finish.status.success(), "{}", text(&finish));
    assert_eq!(p.only_run()["outcome"]["work_result"], "ready");
}

/// With the person's consent for exactly the project's checks, Work a runtime
/// registers may run them; a change to the checks voids that for new Work.
#[test]
fn consent_for_the_projects_checks_gives_discovered_work_its_authority() {
    let p = Project::new("checks:\n  verify: ['true']\n");
    p.watch();
    let state = dispatch::state::State::discover(Some(p.state.clone())).unwrap();
    dispatch::consent::grant(&state, &p.root).unwrap();
    p.start("s1", "startup", &p.worktree);
    let run = p.only_run();
    let id = run["id"].as_str().unwrap().to_owned();
    assert_eq!(run["environment"]["unsafe_local"], true);
    assert_eq!(
        run["attachment"]["capabilities"]["integrate"], false,
        "never auto-apply"
    );
    let db = rusqlite::Connection::open(p.state.join("dispatch.db")).unwrap();
    let by: String = db
        .query_row(
            "SELECT json_extract(payload_json, '$.by') FROM events WHERE run_id = ?1 AND event_type = 'attach.authorized'",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(by, "project consent");

    // The checks change: new Work asks first, and the project says why.
    fs::write(
        p.root.join("dispatch.yml"),
        "coherence:\n  poll_secs: 1\nchecks:\n  verify: ['true', 'sh ./other.sh']\n",
    )
    .unwrap();
    git(
        &p.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "worktree-y",
            ".claude/worktrees/y",
        ],
    );
    let other = fs::canonicalize(p.root.join(".claude/worktrees/y")).unwrap();
    p.start("s2", "startup", &other);
    let second = p
        .runs()
        .into_iter()
        .find(|run| run["id"] != id.as_str())
        .unwrap();
    assert_eq!(second["environment"]["unsafe_local"], false);
    let status = text(&p.dispatch(&["status", &id]));
    assert!(status.contains("check consent no longer holds"), "{status}");
}

impl Project {
    fn consent(&self) {
        let state = dispatch::state::State::discover(Some(self.state.clone())).unwrap();
        dispatch::consent::grant(&state, &self.root).unwrap();
    }

    /// Wait for the owner to change the only run until `ready` holds.
    fn until(&self, what: &str, ready: impl Fn(&Value) -> bool) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let run = self.only_run();
            if ready(&run) {
                return run;
            }
            assert!(std::time::Instant::now() < deadline, "never {what}: {run}");
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
}

#[test]
fn consented_work_is_verified_by_itself_when_its_worktree_is_removed() {
    let p = Project::new("checks:\n  verify: ['test -f src/lib.rs']\n");
    p.consent();
    p.watch();
    p.start("s1", "startup", &p.worktree);
    fs::write(
        p.worktree.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    assert!(p.remove().status.success());
    let run = p.until("verified", |run| run["outcome"]["lifecycle"] == "finished");
    assert_eq!(run["outcome"]["work_result"], "ready", "{run}");
    assert_eq!(run["outcome"]["verification"], "passed", "{run}");
    assert_eq!(run["attachment"]["finish_reason"], "by_consent");
    // Review stays the person's: nothing was applied.
    assert_eq!(run["outcome"]["review"], "pending");
    assert_eq!(run["outcome"]["application"], "not_applied");
    assert_eq!(
        fs::read_to_string(p.root.join("src/lib.rs")).unwrap(),
        "pub fn f() -> i32 {\n    1\n}\n"
    );
}

#[test]
fn failing_checks_leave_consented_work_ready_with_its_failure() {
    let p = Project::new("checks:\n  verify: ['test ! -f src/lib.rs']\n");
    p.consent();
    p.watch();
    p.start("s1", "startup", &p.worktree);
    fs::write(
        p.worktree.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    assert!(p.remove().status.success());
    let run = p.until("verified", |run| run["outcome"]["lifecycle"] == "finished");
    assert_eq!(run["outcome"]["verification"], "failed", "{run}");
}

#[test]
fn consent_voided_after_registration_leaves_the_work_waiting() {
    let p = Project::new("checks:\n  verify: ['true']\n");
    p.consent();
    p.watch();
    p.start("s1", "startup", &p.worktree);
    fs::write(
        p.worktree.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    fs::write(
        p.root.join("dispatch.yml"),
        "coherence:\n  poll_secs: 1\nchecks:\n  verify: ['true', 'sh ./new.sh']\n",
    )
    .unwrap();
    assert!(p.remove().status.success());
    std::thread::sleep(std::time::Duration::from_secs(4));
    let run = p.only_run();
    assert_eq!(run["outcome"]["lifecycle"], "working", "{run}");
    assert_eq!(run["attachment"]["workspace_removed"]["exact"], true);
}

#[test]
fn an_empty_worktree_that_vanishes_unannounced_closes_with_no_changes() {
    let p = Project::new("");
    p.watch();
    p.start("s1", "startup", &p.worktree);
    // Let the owner follow it once, empty, then delete it behind its back.
    std::thread::sleep(std::time::Duration::from_secs(3));
    git(
        &p.root,
        &["worktree", "remove", "--force", ".claude/worktrees/x"],
    );
    let run = p.until("closed", |run| run["outcome"]["lifecycle"] == "finished");
    assert_eq!(run["outcome"]["work_result"], "cancelled", "{run}");
    assert_eq!(run["attachment"]["workspace_removed"]["exact"], false);
}
