//! Discovery without hooks (`docs/plan-0.4.13.md` §3 B): in a project watched
//! by `dispatch start`, a worktree Claude Code made after the watch began,
//! locked by a session whose process runs, becomes Work by itself. The fake
//! worktrees here carry what Claude Code records (observed with 2.1.296): a
//! linked worktree under `.claude/worktrees/<name>`, `CLAUDE_BASE`, and a
//! `claude session` lock naming a process. A running session is this test
//! process, with its own `ps -o lstart=` in UTC.
#![cfg(unix)]

use std::{
    fs,
    io::Write,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

use serde_json::Value;
use sha2::{Digest, Sha256};

struct Project {
    _temp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
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
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Long enough for several owner ticks (`poll_secs: 1`) and discovery's
/// grace for a new worktree without a lock.
fn ticks() {
    std::thread::sleep(Duration::from_secs(5));
}

/// The lock a running `claude --worktree <name>` session holds.
fn session_lock(name: &str, pid: u32) -> String {
    let output = Command::new("ps")
        .env("TZ", "UTC0")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let start = String::from_utf8(output.stdout).unwrap();
    format!("claude session {name} (pid {pid} start {})", start.trim())
}

/// A session lock whose process has exited.
fn dead_lock(name: &str) -> String {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    let lock = session_lock(name, std::process::id()).replace(
        &format!("pid {}", std::process::id()),
        &format!("pid {pid}"),
    );
    child.wait().unwrap();
    lock
}

impl Project {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn f() -> i32 {\n    1\n}\n").unwrap();
        fs::write(root.join("dispatch.yml"), "coherence:\n  poll_secs: 1\n").unwrap();
        fs::write(root.join(".gitignore"), ".claude/\n").unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
        Self {
            root: fs::canonicalize(&root).unwrap(),
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

    /// `dispatch start`, returning once the owner's first scan is done.
    fn watch(&self) {
        let output = self.dispatch(&["start"]);
        assert!(output.status.success(), "{}", text(&output));
        wait_until("the first scan", || self.inventory().is_some());
    }

    fn watcher_file(&self, suffix: &str) -> PathBuf {
        let key = hex::encode(Sha256::digest(self.root.to_string_lossy().as_bytes()));
        self.state.join("watchers").join(format!("{key}.{suffix}"))
    }

    fn inventory(&self) -> Option<Value> {
        let bytes = fs::read(self.watcher_file("inventory.json")).ok()?;
        Some(serde_json::from_slice(&bytes).unwrap())
    }

    /// The paths the inventory lists, by worktree name.
    fn listed(&self) -> Vec<String> {
        let inventory = self.inventory().expect("an inventory");
        let mut names: Vec<String> = inventory["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                let path = PathBuf::from(entry["path"].as_str().unwrap());
                path.file_name().unwrap().to_string_lossy().into_owned()
            })
            .collect();
        names.sort();
        names
    }

    fn owner_log(&self) -> String {
        fs::read_to_string(self.watcher_file("log")).unwrap_or_default()
    }

    /// Where Claude Code makes worktree `name`.
    fn worktree(&self, name: &str) -> PathBuf {
        self.root.join(".claude/worktrees").join(name)
    }

    /// A linked worktree at `.claude/worktrees/<name>` from `HEAD`, as a
    /// person would make it: no `CLAUDE_BASE`, no lock.
    fn add_worktree(&self, name: &str) -> PathBuf {
        let branch = format!("worktree-{name}");
        let path = format!(".claude/worktrees/{name}");
        git(
            &self.root,
            &["worktree", "add", "--quiet", "-B", &branch, &path],
        );
        fs::canonicalize(self.worktree(name)).unwrap()
    }

    /// The worktree's Git metadata directory, from its `.git` file.
    fn gitdir(&self, worktree: &Path) -> PathBuf {
        let file = fs::read_to_string(worktree.join(".git")).unwrap();
        PathBuf::from(file.trim().strip_prefix("gitdir: ").unwrap())
    }

    /// What Claude Code records in a worktree it made: `CLAUDE_BASE`, and
    /// the lock, if any.
    fn mark_claude(&self, worktree: &Path, lock: Option<&str>) {
        let gitdir = self.gitdir(worktree);
        let head = git(worktree, &["rev-parse", "HEAD"]);
        fs::write(gitdir.join("CLAUDE_BASE"), format!("{head}\n")).unwrap();
        self.lock(worktree, lock);
    }

    fn lock(&self, worktree: &Path, lock: Option<&str>) {
        let locked = self.gitdir(worktree).join("locked");
        match lock {
            Some(reason) => fs::write(locked, reason).unwrap(),
            None => {
                let _ = fs::remove_file(locked);
            }
        }
    }

    /// A worktree Claude Code made, with `lock`.
    fn claude_worktree(&self, name: &str, lock: Option<&str>) -> PathBuf {
        let worktree = self.add_worktree(name);
        self.mark_claude(&worktree, lock);
        worktree
    }

    fn running(&self, name: &str) -> PathBuf {
        self.claude_worktree(name, Some(&session_lock(name, std::process::id())))
    }

    fn runs(&self) -> Vec<Value> {
        let Ok(entries) = fs::read_dir(self.state.join("runs")) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| fs::read(entry.unwrap().path().join("metadata.json")).ok())
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
            .collect()
    }

    fn runs_in(&self, worktree: &Path) -> Vec<Value> {
        let mut runs: Vec<Value> = self
            .runs()
            .into_iter()
            .filter(|run| run["attachment"]["workspace"].as_str() == worktree.to_str())
            .collect();
        runs.sort_by_key(|run| run["id"].as_str().unwrap().to_owned());
        runs
    }

    fn events(&self, id: &str) -> Vec<Value> {
        fs::read_to_string(self.state.join("runs").join(id).join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn hook(&self, event: Value) -> String {
        let mut child = Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&self.state)
            .args(["hook", "claude"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(event.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", text(&output));
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Claude Code's `SessionStart` for a session in `worktree`.
    fn session_start(&self, worktree: &Path, session: &str) -> String {
        self.hook(serde_json::json!({
            "session_id": session, "transcript_path": "/dev/null", "cwd": worktree,
            "hook_event_name": "SessionStart", "source": "startup",
        }))
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = self.dispatch(&["stop"]);
    }
}

#[test]
fn a_worktree_made_while_watched_with_a_running_session_becomes_one_work() {
    let p = Project::new();
    p.watch();
    let base = git(&p.root, &["rev-parse", "HEAD"]);
    // The session committed one change and left another uncommitted, and the
    // project moved meanwhile: S0 is the merge base, neither tip.
    let worktree = p.add_worktree("a");
    fs::write(
        worktree.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    git(&worktree, &["commit", "--quiet", "-am", "agent"]);
    fs::write(worktree.join("notes.txt"), "uncommitted\n").unwrap();
    fs::write(p.root.join("elsewhere.txt"), "the project moved\n").unwrap();
    git(&p.root, &["add", "-A"]);
    git(&p.root, &["commit", "--quiet", "-m", "moved"]);
    let lock = session_lock("a", std::process::id());
    p.mark_claude(&worktree, Some(&lock));

    wait_until("discovered Work", || !p.runs().is_empty());
    let [run] = p.runs().try_into().unwrap();
    let id = run["id"].as_str().unwrap().to_owned();
    let attachment = &run["attachment"];
    assert_eq!(attachment["workspace"], worktree.to_str().unwrap());
    assert_eq!(
        attachment["provenance"],
        serde_json::json!({"discovered": {"commit": base}})
    );
    assert_eq!(attachment["confidence"], "full");
    assert_eq!(attachment["workspace_owner"], "runtime");
    assert_eq!(attachment["agent"], "claude");
    assert_eq!(run["task"], "claude session in a");
    let pid = std::process::id();
    let [session] = attachment["sessions"]
        .as_array()
        .unwrap()
        .clone()
        .try_into()
        .unwrap();
    assert_eq!(session["provider"], "claude");
    assert_eq!(session["session_id"], format!("lock:a@{pid}"));
    assert_eq!(session["source"], "discovered");
    assert_eq!(session["ended_at"], Value::Null);
    // The session began when its process did, as the lock says.
    let started = chrono::DateTime::parse_from_rfc3339(session["started_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    let words = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(
        words(&format!(
            "claude session a (pid {pid} start {})",
            started.format("%a %b %e %H:%M:%S %Y")
        )),
        words(&lock)
    );
    assert_eq!(run["created_at"], session["started_at"]);
    let created = p
        .events(&id)
        .into_iter()
        .find(|event| event["event_type"] == "attach.created")
        .unwrap();
    assert_eq!(created["payload"]["via"], "discovery");
    assert!(p.listed().is_empty());

    // Δ is the session's work against S0: both its changes, not the
    // project's move.
    let finished = p.dispatch(&["finish", &id]);
    assert!(finished.status.success(), "{}", text(&finished));
    let run = p.runs().into_iter().next().unwrap();
    let mut changed: Vec<String> =
        serde_json::from_value(run["candidates"][0]["diff_stats"]["changed_files"].clone())
            .unwrap();
    changed.sort();
    assert_eq!(changed, ["notes.txt", "src/lib.rs"]);

    // Its session still runs, but its Work is finished: never registered
    // again, nor listed.
    ticks();
    assert_eq!(p.runs().len(), 1);
    assert!(p.listed().is_empty());
}

#[test]
fn worktrees_there_at_the_first_scan_are_listed_not_registered_for_the_whole_watch() {
    let p = Project::new();
    let early = p.running("early");
    // A session that ended before the watch began: listed too, never Work.
    p.claude_worktree("early-ended", Some(&dead_lock("early-ended")));
    let base = git(&early, &["rev-parse", "HEAD"]);
    p.watch();
    assert_eq!(p.listed(), ["early", "early-ended"]);
    let inventory = p.inventory().unwrap();
    let entry = inventory["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == early.to_str().unwrap())
        .unwrap();
    assert_eq!(entry["path"], early.to_str().unwrap());
    assert_eq!(entry["base"], base);
    assert_eq!(
        entry["lock"],
        session_lock("early", std::process::id()).as_str()
    );
    let record: Value = serde_json::from_slice(&fs::read(p.watcher_file("json")).unwrap()).unwrap();
    assert_eq!(inventory["watch_started_at"], record["started_at"]);
    assert_eq!(inventory["watch_owner"], record["identity"]);
    ticks();
    assert!(p.runs().is_empty());

    // The owner dies without stopping the watch; the next owner keeps the
    // watch's inventory.
    let pid = record["identity"]["pid"].as_i64().unwrap() as i32;
    // SAFETY: the owner this test started.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    let restarted = || {
        let output = p.dispatch(&["start"]);
        output.status.success() && text(&output).contains("✓ Watching")
    };
    wait_until("a restarted owner", restarted);
    ticks();
    assert!(p.runs().is_empty());
    assert_eq!(p.inventory().unwrap(), inventory);

    // `dispatch stop` ends the watch and its inventory; the next watch takes
    // its own, still without registering anything already there.
    assert!(p.dispatch(&["stop"]).status.success());
    assert!(p.inventory().is_none());
    p.watch();
    assert_eq!(p.listed(), ["early", "early-ended"]);
    assert_ne!(
        p.inventory().unwrap()["watch_started_at"],
        record["started_at"]
    );
    ticks();
    assert!(p.runs().is_empty());
}

#[test]
fn only_a_session_lock_registers_and_human_worktrees_are_never_listed() {
    let p = Project::new();
    p.watch();
    // A `claude -p` run that began and ended between two ticks leaves its
    // lock: Work, its session already over.
    let ended = p.claude_worktree("ended", Some(&dead_lock("ended")));
    p.claude_worktree("other", Some("moving it"));
    let unlocked = p.claude_worktree("unlocked", None);
    // A person's worktree under the same directory, even locked by a live
    // process in Claude's format.
    let human = p.add_worktree("human");
    p.lock(&human, Some(&session_lock("human", std::process::id())));

    wait_until("two listed worktrees", || {
        p.inventory().is_some_and(|_| p.listed().len() == 2)
    });
    assert_eq!(p.listed(), ["other", "unlocked"]);
    wait_until("Work for the ended session", || {
        p.runs_in(&ended).len() == 1
    });
    let [run] = p.runs().try_into().unwrap();
    let attachment = &run["attachment"];
    assert!(
        attachment["provenance"]["discovered"]["commit"].is_string(),
        "{attachment}"
    );
    assert_eq!(attachment["confidence"], "full");
    let [session] = attachment["sessions"]
        .as_array()
        .unwrap()
        .clone()
        .try_into()
        .unwrap();
    assert!(session["ended_at"].is_string(), "{session}");
    assert_eq!(session["end_reason"], "process_gone");
    let status = text(&p.dispatch(&["status", run["id"].as_str().unwrap()]));
    assert!(status.contains("session ended — finish"), "{status}");
    let log = p.owner_log();
    assert_eq!(
        log.matches("unlocked was made by Claude Code").count(),
        1,
        "{log}"
    );
    assert!(log.contains("(moving it)"), "{log}");
    assert!(!log.contains("human"), "{log}");

    // Listed is final: a session that later locks a listed worktree does not
    // make it Work.
    p.lock(
        &unlocked,
        Some(&session_lock("unlocked", std::process::id())),
    );
    ticks();
    assert_eq!(p.runs().len(), 1);
    assert_eq!(p.listed(), ["other", "unlocked"]);
    // Each was noted once, however many ticks saw it.
    assert_eq!(
        p.owner_log()
            .matches("other was made by Claude Code")
            .count(),
        1
    );
}

#[test]
fn a_hook_and_discovery_give_one_work_whichever_sees_the_worktree_first() {
    let p = Project::new();
    p.watch();

    // The hook first: the worktree is not yet recognizable when it registers.
    let hooked = p.add_worktree("hooked");
    let reply = p.session_start(&hooked, "s-hooked");
    assert!(reply.contains("tracking this worktree as Work"), "{reply}");
    p.mark_claude(&hooked, Some(&session_lock("hooked", std::process::id())));

    // Discovery first, then the session's hook joins its Work.
    let found = p.running("found");
    wait_until("discovered Work", || p.runs_in(&found).len() == 1);
    let reply = p.session_start(&found, "s-found");
    assert_eq!(reply.trim(), "");

    ticks();
    let [hooked_run] = p.runs_in(&hooked).try_into().unwrap();
    assert_eq!(
        hooked_run["attachment"]["provenance"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["workspace_at_start"]
    );
    let [found_run] = p.runs_in(&found).try_into().unwrap();
    let sessions: Vec<&str> = found_run["attachment"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["session_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        sessions,
        [
            format!("lock:found@{}", std::process::id()).as_str(),
            "s-found"
        ]
    );
    assert_eq!(p.runs().len(), 2);
    assert!(p.listed().is_empty());
}

/// Removal and recreation at one path. A listed worktree that is removed
/// leaves the inventory, so one made there later is new. Work whose
/// worktree was removed, once finished, leaves the path free for new Work.
/// Work that finished while its worktree stayed keeps it: a later session
/// there is not registered, since its S0 could not be known.
#[test]
fn a_worktree_removed_and_made_again_at_one_path_is_new_only_when_dispatch_saw_it_go() {
    let p = Project::new();
    let path = p.claude_worktree("r", None);
    p.watch();
    assert_eq!(p.listed(), ["r"]);

    git(
        &p.root,
        &["worktree", "remove", "--force", path.to_str().unwrap()],
    );
    wait_until("the removed worktree unlisted", || p.listed().is_empty());
    p.running("r");
    wait_until("Work in the new worktree", || p.runs_in(&path).len() == 1);
    let first = p.runs_in(&path)[0]["id"].as_str().unwrap().to_owned();

    // Claude removes it with nothing changed: the Work ends, and the
    // removal is recorded.
    p.hook(serde_json::json!({
        "hook_event_name": "WorktreeRemove", "session_id": "s", "cwd": p.root,
        "worktree_path": path, "name": "r",
    }));
    git(
        &p.root,
        &["worktree", "remove", "-f", "-f", path.to_str().unwrap()],
    );
    let first_run = || {
        p.runs_in(&path)
            .into_iter()
            .find(|run| run["id"] == first.as_str())
            .unwrap()
    };
    assert_eq!(first_run()["outcome"]["lifecycle"], "finished");
    p.running("r");
    wait_until("Work in the third worktree", || p.runs_in(&path).len() == 2);
    let second = p.runs_in(&path)[1]["id"].as_str().unwrap().to_owned();
    assert_ne!(second, first);

    // Closed while its worktree stays and its session runs: not again.
    let closed = p.dispatch(&["close", &second, "--reason", "unfinished"]);
    assert!(closed.status.success(), "{}", text(&closed));
    ticks();
    assert_eq!(p.runs_in(&path).len(), 2);
    assert!(p.listed().is_empty());
}

/// A project that is a plain directory has no worktrees to look for, and
/// its owner says nothing about them.
#[test]
fn a_plain_directory_is_watched_without_discovery_errors() {
    let p = Project::new();
    fs::remove_dir_all(p.root.join(".git")).unwrap();
    let output = p.dispatch(&["start"]);
    assert!(output.status.success(), "{}", text(&output));
    ticks();
    let log = p.owner_log();
    assert!(!log.contains("worktree"), "{log}");
    assert!(p.inventory().is_none());
}

#[test]
fn a_registration_in_flight_is_left_alone_and_a_failure_is_noted_once_then_retried() {
    let p = Project::new();
    p.watch();

    // Someone holds the workspace's registration lock: discovery waits for
    // it rather than registering beside it.
    let held = p.worktree("held");
    let key = hex::encode(Sha256::digest(held.to_string_lossy().as_bytes()));
    let lock = p.state.join("locks").join(format!("workspace-{key}.lock"));
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let file = fs::File::create(&lock).unwrap();
    // The registration lock is an advisory `flock`, as `OperationLock` takes it.
    // SAFETY: the descriptor stays open while the lock is held.
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let held = p.running("held");
    ticks();
    assert!(p.runs().is_empty());
    assert!(p.listed().is_empty());
    drop(file);
    wait_until("Work once the lock is free", || p.runs_in(&held).len() == 1);

    // A registration that fails is noted once, and tried again each tick.
    let config = fs::read_to_string(p.root.join("dispatch.yml")).unwrap();
    fs::write(p.root.join("dispatch.yml"), "coherence: [\n").unwrap();
    let broken = p.running("broken");
    ticks();
    assert!(p.runs_in(&broken).is_empty());
    let log = p.owner_log();
    assert_eq!(
        log.matches("could not track the Claude Code session in")
            .count(),
        1,
        "{log}"
    );
    fs::write(p.root.join("dispatch.yml"), config).unwrap();
    wait_until("Work once it can register", || {
        p.runs_in(&broken).len() == 1
    });
}

/// Found in the 0.4.13 real-agent trial: a `claude -p --worktree` session was
/// discovered while it ran, then exited before liveness first looked, so
/// nothing ever recorded its process and its session stayed open. Discovery
/// records the running session's process as the Work's agent, so the end is
/// seen however soon it comes.
#[test]
fn a_discovered_session_that_ends_at_once_is_still_seen_to_end() {
    let p = Project::new();
    p.watch();
    let mut agent = Command::new("sleep").arg("30").spawn().unwrap();
    let worktree = p.claude_worktree("quick", Some(&session_lock("quick", agent.id())));
    wait_until("the session's Work", || p.runs_in(&worktree).len() == 1);
    let run = p.runs_in(&worktree).remove(0);
    assert_eq!(
        run["attachment"]["agent_process"]["pid"],
        agent.id(),
        "{run}"
    );
    // Gone before the owner's next tick, as the trial's run was.
    agent.kill().unwrap();
    agent.wait().unwrap();
    wait_until("the session's end", || {
        p.runs_in(&worktree)[0]["attachment"]["sessions"][0]["end_reason"] == "process_gone"
    });
    let run = p.runs_in(&worktree).remove(0);
    assert_eq!(
        run["outcome"]["lifecycle"], "working",
        "ending a session finishes nothing"
    );
}
