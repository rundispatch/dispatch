//! Registering a runtime session without silent gaps (`docs/plan-0.4.10.md`
//! §3): every `SessionStart` in its own worktree of a watched project ends
//! registered, pending then published, or untracked with its notice, and
//! `runs/` only ever holds complete Work. Faults are injected in debug builds
//! with `DISPATCH_REGISTRATION_FAULT`; the deadline is shortened with
//! `DISPATCH_REGISTRATION_BUDGET_MS`.
#![cfg(unix)]

use std::{
    fs,
    io::Write,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

use dispatch::orchestrator::registration;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The owner's budget in these tests: a registration older than this, with
/// nobody holding it, is the owner's to resolve.
const OWNER_BUDGET_MS: &str = "1500";

/// A hook's budget where the deadline is not under test: only a hang reaches
/// it, so the session registers however slow the machine is.
const NO_DEADLINE_MS: &str = "600000";

/// `SessionStart`'s hook timeout as `dispatch setup` installs it: the reply
/// must come before Claude Code gives up on the hook.
const HOOK_TIMEOUT: Duration = Duration::from_secs(60);

/// A hook's deadline comes a budget after it starts, so past the deadline it
/// has this long to reply; waiting a quarter of the budget for a publish under
/// way must fit.
const AFTER_DEADLINE: Duration = HOOK_TIMEOUT.saturating_sub(registration::BUDGET);
const _: () = assert!(registration::BUDGET.as_millis() / 4 < AFTER_DEADLINE.as_millis());

/// How long ago `path` was last written.
fn since_written(path: &Path) -> Duration {
    fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .elapsed()
        .unwrap()
}

struct Project {
    _temp: tempfile::TempDir,
    root: PathBuf,
    worktree: PathBuf,
    state: PathBuf,
    /// Held, the project counts as watched with no owner running.
    watched: Option<fs::File>,
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
    /// A project watched by nobody but this test, so that only `owner`
    /// reconciles, when the test says.
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn f() -> i32 {\n    1\n}\n").unwrap();
        fs::write(root.join("dispatch.yml"), "coherence:\n  poll_secs: 1\n").unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
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
        let mut project = Self {
            root: fs::canonicalize(&root).unwrap(),
            worktree: fs::canonicalize(root.join(".claude/worktrees/x")).unwrap(),
            state: temp.path().join("state"),
            watched: None,
            _temp: temp,
        };
        project.hold_watch();
        project
    }

    fn serve_lock(&self) -> PathBuf {
        let key = hex::encode(Sha256::digest(self.root.to_string_lossy().as_bytes()));
        self.state.join("locks").join(format!("serve-{key}.lock"))
    }

    /// Let go of the project's serve lock. A process another test forked
    /// meanwhile holds it with us until it execs, which a loaded machine
    /// delays: wait until nobody holds it.
    fn release_watch(&mut self) {
        self.watched = None;
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let probe = fs::File::open(self.serve_lock()).unwrap();
            // SAFETY: a non-blocking probe on a descriptor this test owns.
            if unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return;
            }
            assert!(Instant::now() < deadline, "the serve lock stayed held");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Hold the project's serve lock, as an owner would.
    fn hold_watch(&mut self) {
        let path = self.serve_lock();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .unwrap();
        // SAFETY: the descriptor stays open while the lock is held.
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        self.watched = Some(file);
    }

    fn dispatch(&self, args: &[&str]) -> Output {
        Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&self.state)
            .args(args)
            .current_dir(&self.root)
            .env("DISPATCH_REGISTRATION_BUDGET_MS", OWNER_BUDGET_MS)
            .env_remove("DISPATCH_REGISTRATION_FAULT")
            .output()
            .unwrap()
    }

    /// Run the hook with `env`, and return what it did.
    fn hook(&self, event: Value, env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(assert_cmd::cargo_bin!("dispatch"));
        command
            .arg("--state-dir")
            .arg(&self.state)
            .args(["hook", "claude"])
            .env_remove("DISPATCH_REGISTRATION_FAULT")
            .env("DISPATCH_REGISTRATION_BUDGET_MS", NO_DEADLINE_MS)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(event.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn start(&self, session: &str, source: &str, env: &[(&str, &str)]) -> Output {
        self.hook(
            serde_json::json!({
                "hook_event_name": "SessionStart", "session_id": session, "source": source,
                "cwd": self.worktree, "transcript_path": "/dev/null", "model": "claude-sonnet-5",
            }),
            env,
        )
    }

    /// The reply of a hook that succeeded.
    fn notice(output: &Output) -> String {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return String::new();
        }
        let reply: Value = serde_json::from_str(&stdout).unwrap();
        reply["systemMessage"].as_str().unwrap().to_owned()
    }

    /// A session that started and was told its registration is pending: its
    /// worker stalled after `registration.json`, and the deadline came then,
    /// however slow the machine was to get there.
    fn pending(&self) -> String {
        let notice = Self::notice(&self.start(
            "s1",
            "startup",
            &[(
                "DISPATCH_REGISTRATION_FAULT",
                "after_registration_json:deadline",
            )],
        ));
        let [(id, files)] = self.registrations().try_into().unwrap();
        assert!(
            since_written(&self.registration_dir(&id).join("registration.json")) < AFTER_DEADLINE
        );
        assert_eq!(notice, registration::notice_pending(&id));
        assert!(files.contains(&"registration.json".to_owned()), "{files:?}");
        assert_eq!(self.decision(&id).as_deref(), Some("pending"));
        assert!(self.runs().is_empty());
        id
    }

    fn registration_dir(&self, id: &str) -> PathBuf {
        self.state.join("registrations").join(id)
    }

    /// Each registration directory, with the names in it.
    fn registrations(&self) -> Vec<(String, Vec<String>)> {
        let Ok(entries) = fs::read_dir(self.state.join("registrations")) else {
            return Vec::new();
        };
        let mut found: Vec<_> = entries
            .map(|entry| {
                let entry = entry.unwrap();
                let mut names: Vec<_> = fs::read_dir(entry.path())
                    .unwrap()
                    .map(|inner| inner.unwrap().file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                (entry.file_name().to_string_lossy().into_owned(), names)
            })
            .collect();
        found.sort();
        found
    }

    fn decision(&self, id: &str) -> Option<String> {
        let bytes = fs::read(self.registration_dir(id).join("decision")).ok()?;
        Some(serde_json::from_slice::<String>(&bytes).unwrap())
    }

    /// Every run in `runs/`, each of which must be complete Work: its
    /// metadata, its baseline and its database record.
    fn runs(&self) -> Vec<Value> {
        let Ok(entries) = fs::read_dir(self.state.join("runs")) else {
            return Vec::new();
        };
        entries
            .map(|entry| {
                let dir = entry.unwrap().path();
                let metadata = dir.join("metadata.json");
                assert!(metadata.is_file(), "incomplete Work in runs/: {dir:?}");
                let run: Value = serde_json::from_slice(&fs::read(metadata).unwrap()).unwrap();
                assert!(dir.join("baseline").is_dir(), "{dir:?} has no baseline");
                assert_eq!(
                    run["baseline_path"].as_str().map(PathBuf::from),
                    Some(dir.join("baseline"))
                );
                assert_eq!(self.recorded(run["id"].as_str().unwrap()), 1);
                run
            })
            .collect()
    }

    fn only_run(&self) -> Value {
        let runs = self.runs();
        assert_eq!(runs.len(), 1, "{runs:?}");
        runs.into_iter().next().unwrap()
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.state.join("dispatch.db")).unwrap()
    }

    /// How many records the database holds for `id`, or for every run.
    fn recorded(&self, id: &str) -> i64 {
        if !self.state.join("dispatch.db").is_file() {
            return 0;
        }
        self.db()
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE id = ?1 OR ?1 = ''",
                [id],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// The payloads of `id`'s events of `kind`.
    fn events(&self, id: &str, kind: &str) -> Vec<Value> {
        let db = self.db();
        let mut statement = db
            .prepare("SELECT payload_json FROM events WHERE run_id = ?1 AND event_type = ?2")
            .unwrap();
        statement
            .query_map([id, kind], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
            .collect()
    }

    /// Run the project owner until `done` holds, then stop it, and return its
    /// log.
    fn owner(&mut self, what: &str, done: impl Fn(&Self) -> bool) -> String {
        self.release_watch();
        let output = self.dispatch(&["start"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let deadline = Instant::now() + Duration::from_secs(300);
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "the owner never {what}: {:?}\n{}",
                self.registrations(),
                self.log()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(self.dispatch(&["stop"]).status.success());
        self.hold_watch();
        self.log()
    }

    fn log(&self) -> String {
        let key = hex::encode(Sha256::digest(self.root.to_string_lossy().as_bytes()));
        fs::read_to_string(self.state.join("watchers").join(format!("{key}.log")))
            .unwrap_or_default()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = self.dispatch(&["stop"]);
    }
}

/// The hook process ends at `boundary` with no reply and no cleanup.
fn killed_at(boundary: &str) -> (Project, String) {
    let p = Project::new();
    let output = p.start(
        "s1",
        "startup",
        &[("DISPATCH_REGISTRATION_FAULT", boundary)],
    );
    assert!(!output.status.success(), "the fault must end the hook");
    assert!(output.stdout.is_empty(), "no reply at all");
    assert!(
        p.runs().is_empty(),
        "runs/ only ever receives complete Work"
    );
    let [(id, _)] = p.registrations().try_into().unwrap();
    (p, id)
}

/// The owner published the registration `id` as complete Work, once.
fn published_once(p: &Project, id: &str, via: &str) -> Value {
    let run = p.only_run();
    assert_eq!(run["id"], id);
    assert!(p.registrations().is_empty(), "{:?}", p.registrations());
    assert_eq!(p.recorded(""), 1, "no duplicate Work");
    assert_eq!(p.events(id, "run.created").len(), 1);
    let created = p.events(id, "attach.created");
    assert_eq!(created.len(), 1, "{created:?}");
    assert_eq!(created[0]["via"], via);
    assert!(created[0]["registration_ms"].as_u64().is_some());
    assert_eq!(run["attachment"]["sessions"][0]["session_id"], "s1");
    assert_eq!(run["attachment"]["workspace"], p.worktree.to_str().unwrap());
    run
}

/// S0 the run was built from is the one `registration.json` recorded.
fn s0_of(p: &Project, id: &str) -> String {
    let registration: Value = serde_json::from_slice(
        &fs::read(p.registration_dir(id).join("registration.json")).unwrap(),
    )
    .unwrap();
    registration["s0_commit"].as_str().unwrap().to_owned()
}

#[test]
fn a_session_registers_within_its_budget_and_says_so() {
    let p = Project::new();
    let notice = Project::notice(&p.start("s1", "startup", &[]));
    let run = p.only_run();
    let id = run["id"].as_str().unwrap();
    assert_eq!(
        notice,
        format!(
            "Dispatch is tracking this worktree as Work {}; see it with dispatch watch.",
            id
        )
    );
    published_once(&p, id, "hook");
    assert!(fs::read_to_string(p.state.join("runs").join(id).join("decision")).is_ok());
}

#[test]
fn killed_after_the_workspace_lock_nothing_is_captured_and_the_owner_removes_it() {
    let (mut p, id) = killed_at("after_lock");
    assert_eq!(p.registrations()[0].1, ["registration.lock"]);
    // Younger than the budget, a registration may still be running.
    p.release_watch();
    let output = Command::new(assert_cmd::cargo_bin!("dispatch"))
        .arg("--state-dir")
        .arg(&p.state)
        .args(["start"])
        .current_dir(&p.root)
        .env("DISPATCH_REGISTRATION_BUDGET_MS", "60000")
        .output()
        .unwrap();
    assert!(output.status.success());
    std::thread::sleep(Duration::from_millis(2500));
    assert!(p.dispatch(&["stop"]).status.success());
    p.hold_watch();
    assert_eq!(p.registrations().len(), 1, "left alone while young");

    let log = p.owner("removed it", |p| p.registrations().is_empty());
    assert!(
        log.contains(&format!(
            "registration {id}: no starting state was captured"
        )),
        "{log}"
    );
    assert!(p.runs().is_empty());
    assert_eq!(p.recorded(""), 0);
}

#[test]
fn killed_after_s0_without_registration_json_is_never_published() {
    let (mut p, id) = killed_at("after_s0_commit");
    assert_eq!(p.registrations()[0].1, ["registration.lock"]);
    let log = p.owner("removed it", |p| p.registrations().is_empty());
    assert!(log.contains(&format!("registration {id}")), "{log}");
    assert!(p.runs().is_empty());
    assert_eq!(p.recorded(""), 0);
}

#[test]
fn killed_after_registration_json_the_owner_publishes_it_from_s0() {
    let (mut p, id) = killed_at("after_registration_json");
    let files = &p.registrations()[0].1;
    assert!(files.contains(&"registration.json".to_owned()), "{files:?}");
    assert!(!files.contains(&"decision".to_owned()), "{files:?}");
    let s0 = s0_of(&p, &id);
    let log = p.owner("published it", |p| !p.runs().is_empty());
    let run = published_once(&p, &id, "owner");
    assert_eq!(
        run["attachment"]["provenance"]["workspace_at_start"]["commit"],
        s0.as_str()
    );
    assert!(
        log.contains(&format!("registration {id}: published")),
        "{log}"
    );
}

#[test]
fn killed_after_the_decision_the_owner_publishes_what_was_built() {
    let (mut p, id) = killed_at("after_decision");
    assert_eq!(p.decision(&id).as_deref(), Some("publishing"));
    assert!(p.registration_dir(&id).join("baseline").is_dir());
    assert_eq!(p.recorded(""), 0);
    let s0 = s0_of(&p, &id);
    p.owner("published it", |p| !p.runs().is_empty());
    let run = published_once(&p, &id, "owner");
    assert_eq!(
        run["attachment"]["provenance"]["workspace_at_start"]["commit"],
        s0.as_str()
    );
}

#[test]
fn killed_after_the_database_insert_the_owner_completes_the_same_work() {
    let (mut p, id) = killed_at("after_db_insert");
    assert_eq!(p.recorded(&id), 1, "recorded, not yet renamed");
    assert!(p.events(&id, "attach.created").is_empty());
    assert!(!p.state.join("runs").join(&id).exists());
    p.owner("published it", |p| !p.runs().is_empty());
    published_once(&p, &id, "owner");
}

#[test]
fn past_the_deadline_without_durable_s0_the_session_is_told_it_is_untracked() {
    let mut p = Project::new();
    let began = Instant::now();
    let notice = Project::notice(&p.start(
        "s1",
        "startup",
        &[
            ("DISPATCH_REGISTRATION_FAULT", "after_s0_commit:sleep"),
            ("DISPATCH_REGISTRATION_BUDGET_MS", "2000"),
        ],
    ));
    assert!(began.elapsed() < HOOK_TIMEOUT);
    assert_eq!(notice, registration::NOTICE_UNTRACKED);
    let [(id, files)] = p.registrations().try_into().unwrap();
    assert_eq!(files, ["decision", "registration.lock"]);
    assert_eq!(p.decision(&id).as_deref(), Some("untracked"));

    let log = p.owner("removed it", |p| p.registrations().is_empty());
    assert!(
        log.contains(&format!("registration {id}: untracked")),
        "{log}"
    );
    // Nothing publishes it later, whatever comes next for the workspace.
    p.hook(
        serde_json::json!({
            "hook_event_name": "SessionEnd", "session_id": "s1", "cwd": p.worktree,
            "reason": "other",
        }),
        &[],
    );
    assert!(p.runs().is_empty());
    assert_eq!(p.recorded(""), 0);
}

#[test]
fn past_the_deadline_with_durable_s0_the_session_is_pending_and_the_owner_publishes_it() {
    let mut p = Project::new();
    let id = p.pending();
    let log = p.owner("published it", |p| !p.runs().is_empty());
    published_once(&p, &id, "owner");
    assert!(
        log.contains(&format!("registration {id}: published")),
        "{log}"
    );
}

#[test]
fn a_repeated_session_start_while_pending_lands_on_the_one_work() {
    let p = Project::new();
    let id = p.pending();
    let notice = Project::notice(&p.start("s2", "resume", &[]));
    assert_eq!(notice, "", "the session joins existing Work");
    let run = published_once(&p, &id, "hook");
    let sessions: Vec<_> = run["attachment"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["session_id"].as_str().unwrap())
        .collect();
    assert_eq!(sessions, ["s1", "s2"]);
}

/// Claude Code reports a worktree session's end from the main checkout. Arriving
/// while the registration is pending, the end publishes it first, matched by
/// session, then records the end on it.
#[test]
fn a_session_end_from_the_checkout_while_pending_publishes_then_records_it() {
    let p = Project::new();
    let id = p.pending();
    let notice = Project::notice(&p.hook(
        serde_json::json!({
            "session_id": "s1", "transcript_path": "/dev/null", "cwd": p.root,
            "hook_event_name": "SessionEnd", "reason": "prompt_input_exit",
        }),
        &[],
    ));
    assert_eq!(notice, "");
    let run = published_once(&p, &id, "hook");
    assert_eq!(
        run["attachment"]["sessions"][0]["end_reason"],
        "prompt_input_exit"
    );
    assert_eq!(run["outcome"]["lifecycle"], "working");
    assert_eq!(p.events(&id, "runtime.session_ended").len(), 1);
}

#[test]
fn removing_the_worktree_while_pending_publishes_then_keeps_the_exact_changes() {
    let p = Project::new();
    let id = p.pending();
    fs::write(
        p.worktree.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    let notice = Project::notice(&p.hook(
        serde_json::json!({
            "hook_event_name": "WorktreeRemove", "session_id": "s1", "cwd": p.root,
            "worktree_path": p.worktree, "name": "x",
        }),
        &[],
    ));
    assert!(
        notice.contains(&format!("kept this worktree's changes as Work {id}")),
        "{notice}"
    );
    let run = published_once(&p, &id, "hook");
    assert_eq!(run["attachment"]["workspace_removed"]["exact"], true);
    let delta = fs::read_to_string(p.state.join("runs").join(&id).join("delta.patch")).unwrap();
    assert!(delta.contains("+    2"), "{delta}");
}

#[test]
fn an_owner_restart_while_pending_publishes_it_once() {
    let mut p = Project::new();
    let id = p.pending();
    p.owner("published it", |p| !p.runs().is_empty());
    // A second owner, ticking over the same state, finds nothing to do.
    let began = Instant::now();
    p.owner("ticked again", |_| began.elapsed() > Duration::from_secs(3));
    published_once(&p, &id, "owner");
}

#[test]
fn a_deadline_that_finds_the_publish_under_way_waits_for_it_then_says_pending() {
    let mut p = Project::new();
    let budget = registration::BUDGET.as_millis().to_string();
    let notice = Project::notice(&p.start(
        "s1",
        "startup",
        &[
            ("DISPATCH_REGISTRATION_FAULT", "after_decision:deadline"),
            ("DISPATCH_REGISTRATION_BUDGET_MS", &budget),
        ],
    ));
    let [(id, _)] = p.registrations().try_into().unwrap();
    // The deadline came as the publish began; a quarter of the budget waiting
    // for it, then the reply.
    let waited = since_written(&p.registration_dir(&id).join("decision"));
    assert!(waited >= registration::BUDGET / 4, "{waited:?}");
    assert!(waited < AFTER_DEADLINE, "{waited:?}");
    assert_eq!(notice, registration::notice_pending(&id));
    assert_eq!(p.decision(&id).as_deref(), Some("publishing"));
    assert!(p.runs().is_empty());
    p.owner("published it", |p| !p.runs().is_empty());
    published_once(&p, &id, "owner");
}

#[test]
fn foreign_attach_publishes_whole_and_leaves_no_registration() {
    let p = Project::new();
    let output = p.dispatch(&[
        "attach",
        "--workspace",
        p.worktree.to_str().unwrap(),
        "--task",
        "t",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    p.only_run();
    assert!(p.registrations().is_empty());
}
