//! Catch-up in place, through `dispatch hook claude` as Claude Code runs it:
//! the `Stop` hook that keeps a turn going with an ask, the waiter that wakes
//! an idle session with one (`hook claude-wait`), and, end to end, two
//! stand-in Claude Code sessions where one lands a change to a signature the
//! other uses, which is asked once, merges, and continues. Payloads are the P0
//! probes', paths replaced. Every waiter a test starts is killed when the test
//! ends, however it ends.
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

const LIB: &str = "pub fn scale(n: i32) -> i32 {\n    n * 2\n}\n";
const ASK: &str = "Dispatch: Work 01AAAAAA (Claude Code) landed a change this Work relies on.";

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

fn key(path: &Path) -> String {
    hex::encode(Sha256::digest(path.to_str().unwrap().as_bytes()))
}

/// A JSON object with `fields` in this order, as the runtimes send them.
fn payload(fields: &[(&str, Value)]) -> String {
    let fields: Vec<String> = fields
        .iter()
        .map(|(name, value)| format!("{}: {value}", Value::from(*name)))
        .collect();
    format!("{{{}}}", fields.join(", "))
}

/// `Stop` as Claude Code 2.1.296 sends it, in its order: `stop_hook_active`
/// before the last message, however long that is.
fn stop(session: &str, cwd: &Path, active: bool, last: &str) -> String {
    payload(&[
        ("session_id", session.into()),
        (
            "transcript_path",
            format!("/home/u/.claude/projects/p/{session}.jsonl").into(),
        ),
        ("cwd", cwd.to_str().unwrap().into()),
        ("scratchpad_dir", "/tmp/scratch".into()),
        ("prompt_id", "89faab27-c846-4882-8c28-7ab6b3b34855".into()),
        ("permission_mode", "auto".into()),
        ("effort", serde_json::json!({"level": "xhigh"})),
        ("hook_event_name", "Stop".into()),
        ("stop_hook_active", active.into()),
        ("last_assistant_message", last.into()),
        ("background_tasks", serde_json::json!([])),
        ("session_crons", serde_json::json!([])),
    ])
}

struct Project {
    _temp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    watched: Option<fs::File>,
}

impl Project {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), LIB).unwrap();
        fs::write(root.join("dispatch.yml"), "coherence:\n  poll_secs: 1\n").unwrap();
        fs::write(root.join(".gitignore"), ".claude/\n").unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
        let mut project = Self {
            root: fs::canonicalize(&root).unwrap(),
            state: temp.path().join("state"),
            watched: None,
            _temp: temp,
        };
        project.hold_watch();
        project
    }

    fn serve_lock(&self) -> PathBuf {
        let name = format!("serve-{}.lock", key(&self.root));
        self.state.join("locks").join(name)
    }

    /// Hold the project's serve lock, as an owner would.
    fn hold_watch(&mut self) {
        fs::create_dir_all(self.state.join("locks")).unwrap();
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.serve_lock())
            .unwrap();
        // SAFETY: the descriptor stays open while the lock is held.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(locked, 0);
        self.watched = Some(file);
    }

    /// Run the owner: let go of the serve lock, wait until a process
    /// forked meanwhile no longer holds it with us, and start it.
    fn start_owner(&mut self) {
        self.watched = None;
        until("the serve lock is free", || {
            let probe = fs::File::open(self.serve_lock()).unwrap();
            // SAFETY: a non-blocking probe on a descriptor this test owns.
            unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
        });
        self.ok(&["start"]);
    }

    fn stop_owner(&mut self) {
        self.ok(&["stop"]);
        self.hold_watch();
    }

    fn dispatch(&self) -> Command {
        let mut command = Command::new(assert_cmd::cargo_bin!("dispatch"));
        command
            .arg("--state-dir")
            .arg(&self.state)
            .current_dir(&self.root);
        command
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.dispatch().args(args).output().unwrap();
        assert!(output.status.success(), "dispatch {args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// `dispatch hook claude` with `input`, and what it printed.
    fn claude(&self, input: &str) -> Value {
        let mut child = self
            .dispatch()
            .args(["hook", "claude"])
            .env("DISPATCH_REGISTRATION_BUDGET_MS", "600000")
            .env_remove("DISPATCH_REGISTRATION_FAULT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output: Output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        match String::from_utf8_lossy(&output.stdout).trim() {
            "" => Value::Null,
            reply => serde_json::from_str(reply).unwrap(),
        }
    }

    /// `dispatch hook claude-wait`, the waiter, after `Stop` in `cwd`.
    fn waiter(&self, session: &str, cwd: &Path, most: u64) -> Waiter {
        self.hook(
            "claude-wait",
            &["--max-wait", &most.to_string()],
            session,
            cwd,
        )
    }

    /// `dispatch hook <runtime> <options>`, given `Stop` in `cwd`.
    fn hook(&self, runtime: &str, options: &[&str], session: &str, cwd: &Path) -> Waiter {
        let mut child = self
            .dispatch()
            .args(["hook", runtime])
            .args(options)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = stop(session, cwd, false, "done");
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.as_bytes()).unwrap();
        drop(stdin);
        Waiter(Some(child))
    }

    /// The waiter `session` registered as its session's newest.
    fn registered(&self, session: &str) -> PathBuf {
        self.state.join("waiters").join(session)
    }

    /// A worktree the way `claude --worktree` makes one, with no session.
    fn worktree(&self, name: &str) -> PathBuf {
        let worktree = format!(".claude/worktrees/{name}");
        git(
            &self.root,
            &["worktree", "add", "--quiet", "-b", name, &worktree],
        );
        fs::canonicalize(self.root.join(worktree)).unwrap()
    }

    /// Mark `worktree` as holding Work the owner follows for catch-up, as
    /// the owner does for live Claude Code Work.
    fn follow(&self, worktree: &Path) {
        let record = self
            .state
            .join("catchup/work")
            .join(format!("{}.json", key(worktree)));
        fs::create_dir_all(record.parent().unwrap()).unwrap();
        let text = serde_json::json!({"version": 1, "run_id": "r", "asked": []});
        fs::write(record, text.to_string()).unwrap();
    }

    /// Leave an ask to catch up for `worktree`, as the owner does.
    fn ask(&self, worktree: &Path) {
        let catchup = self.state.join("catchup");
        fs::create_dir_all(catchup.join("delivered")).unwrap();
        let ask = serde_json::json!({"version": 1, "run_id": "r", "landings": [], "text": ASK});
        fs::write(
            catchup.join(format!("{}.json", key(worktree))),
            ask.to_string(),
        )
        .unwrap();
    }

    fn delivered(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.state.join("catchup/delivered"))
            .into_iter()
            .flatten()
            .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
            .collect();
        names.sort();
        names
    }

    /// A worktree whose Claude Code session started, and its Work.
    fn session(&self, name: &str) -> (PathBuf, String) {
        let worktree = self.worktree(name);
        let reply = self.claude(&payload(&[
            ("session_id", name.into()),
            (
                "transcript_path",
                format!("/home/u/.claude/projects/p/{name}.jsonl").into(),
            ),
            ("cwd", worktree.to_str().unwrap().into()),
            ("hook_event_name", "SessionStart".into()),
            ("source", "startup".into()),
            ("model", "claude-opus-5-5".into()),
        ]));
        let id = reply["systemMessage"]
            .as_str()
            .and_then(|notice| notice.strip_prefix("Dispatch is tracking this worktree as Work "))
            .and_then(|rest| rest.split(';').next())
            .unwrap_or_else(|| panic!("not registered: {reply}"))
            .to_owned();
        (worktree, id)
    }

    fn events(&self, id: &str, kind: &str) -> Vec<Value> {
        let db = rusqlite::Connection::open(self.state.join("dispatch.db")).unwrap();
        let mut statement = db
            .prepare("SELECT payload_json FROM events WHERE run_id = ?1 AND event_type = ?2")
            .unwrap();
        statement
            .query_map([id, kind], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
            .collect()
    }

    fn run(&self, id: &str) -> Value {
        let path = self.state.join("runs").join(id).join("metadata.json");
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    /// The owner's interaction view, once it has written one.
    fn view(&self) -> Option<Value> {
        fs::read_dir(self.state.join("watchers"))
            .ok()?
            .filter_map(|entry| Some(entry.ok()?.path()))
            .find(|path| path.to_string_lossy().ends_with(".interactions.json"))
            .and_then(|path| serde_json::from_slice(&fs::read(path).ok()?).ok())
    }

    /// Whether the owner's view lists `id` with the patch it finished with.
    fn lists_frozen(&self, id: &str) -> bool {
        let patch = fs::read(self.run(id)["candidates"][0]["diff_path"].as_str().unwrap()).unwrap();
        let digest = hex::encode(Sha256::digest(patch));
        self.view().is_some_and(|view| {
            view["participants"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["run_id"] == id && p["delta_sha256"] == digest.as_str())
        })
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = self.dispatch().arg("stop").output();
    }
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(300);
    while !done() {
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A waiter this test started, killed when the test ends if it has not
/// left by itself.
struct Waiter(Option<std::process::Child>);

impl Waiter {
    /// How it left, once it does.
    fn left(mut self) -> Output {
        let child = self.0.as_mut().unwrap();
        until("the waiter left", || child.try_wait().unwrap().is_some());
        self.0.take().unwrap().wait_with_output().unwrap()
    }

    fn waiting(&mut self) -> bool {
        self.0.as_mut().unwrap().try_wait().unwrap().is_none()
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn quiet(output: &Output) -> bool {
    output.status.code() == Some(0) && output.stdout.is_empty() && output.stderr.is_empty()
}

#[test]
fn the_waiter_wakes_its_session_with_an_ask_that_comes_while_it_waits() {
    let p = Project::new();
    let worktree = p.worktree("w");
    p.follow(&worktree);
    let waiter = p.waiter("s1", &worktree.join("src"), 3_600);
    until("the waiter waits", || p.registered("s1").exists());
    p.ask(&worktree);
    let output = waiter.left();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stderr), format!("{ASK}\n"));
    assert!(output.stdout.is_empty(), "nothing on stdout");
    let delivered = p.delivered();
    assert!(
        delivered.len() == 1 && delivered[0].ends_with(".wake.json"),
        "{delivered:?}"
    );
}

#[test]
fn a_waiter_steps_aside_for_a_newer_one_of_its_session_alone() {
    let p = Project::new();
    let (worktree, elsewhere) = (p.worktree("w"), p.worktree("w2"));
    p.follow(&worktree);
    p.follow(&elsewhere);
    let first = p.waiter("s1", &worktree, 3_600);
    until("the first waits", || p.registered("s1").exists());
    let mut other = p.waiter("s2", &elsewhere, 3_600);
    until("another session's waits", || p.registered("s2").exists());
    let before = fs::metadata(p.registered("s1"))
        .unwrap()
        .modified()
        .unwrap();
    let mut newer = p.waiter("s1", &worktree, 3_600);
    until("the newer waits", || {
        fs::metadata(p.registered("s1")).is_ok_and(|m| m.modified().unwrap() != before)
    });
    assert!(quiet(&first.left()));
    assert!(newer.waiting() && other.waiting());
    // The newest delivers; the other session's waits on.
    p.ask(&worktree);
    assert_eq!(newer.left().status.code(), Some(2));
    assert!(other.waiting());
}

/// A process this test did not start, killed when the test ends unless it
/// was seen to be gone.
struct Orphan(u32, bool);

impl Orphan {
    fn alive(&self) -> bool {
        // SAFETY: signal zero only asks whether the process exists.
        unsafe { libc::kill(self.0 as i32, 0) == 0 }
    }
}

impl Drop for Orphan {
    fn drop(&mut self) {
        if !self.1 {
            // SAFETY: the pid was this test's waiter, not yet seen gone.
            unsafe { libc::kill(self.0 as i32, libc::SIGKILL) };
        }
    }
}

#[test]
fn a_waiter_leaves_when_the_session_that_started_it_is_gone() {
    let p = Project::new();
    let worktree = p.worktree("w");
    p.follow(&worktree);
    let input = p._temp.path().join("stop.json");
    fs::write(&input, stop("s1", &worktree, false, "done")).unwrap();
    // A stand-in session: a shell that starts the waiter, waits until it
    // waits, five minutes at most, and ends.
    let output = Command::new("sh")
        .arg("-c")
        .arg(
            r#""$@" < "$INPUT" > /dev/null 2>&1 & echo $!
            i=0
            while [ ! -e "$REGISTERED" ]; do
                i=$((i+1)); [ "$i" -le 3000 ] || exit 1; sleep 0.1
            done"#,
        )
        .arg("sh")
        .arg(assert_cmd::cargo_bin!("dispatch"))
        .arg("--state-dir")
        .arg(&p.state)
        .args(["hook", "claude-wait", "--max-wait", "3600"])
        .env("INPUT", &input)
        .env("REGISTERED", p.registered("s1"))
        .output()
        .unwrap();
    let pid = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap();
    let mut orphan = Orphan(pid, false);
    assert!(
        output.status.success(),
        "the waiter never waited: {output:?}"
    );
    until("the waiter left with its session", || !orphan.alive());
    orphan.1 = true;
    // It left without taking an ask that came later.
    p.ask(&worktree);
    assert!(p.delivered().is_empty());
}

#[test]
fn a_waiter_leaves_at_once_with_nothing_to_wait_for_and_after_its_longest_wait() {
    let mut p = Project::new();
    let worktree = p.worktree("w");
    p.ask(&worktree);
    // The owner keeps no record for this workspace: no Work to ask.
    assert!(quiet(&p.waiter("s1", &worktree, 3_600).left()));
    // The checkout itself never has Work.
    p.follow(&worktree);
    assert!(quiet(&p.waiter("s1", &p.root, 3_600).left()));
    assert!(!p.registered("s1").exists(), "nothing waited");
    // A project nobody watches.
    p.watched = None;
    assert!(quiet(&p.waiter("s1", &worktree, 3_600).left()));
    assert!(!p.registered("s1").exists());
    p.hold_watch();
    // Everything to wait for, but only two seconds to.
    fs::remove_file(
        p.state
            .join("catchup")
            .join(format!("{}.json", key(&worktree))),
    )
    .unwrap();
    let started = Instant::now();
    assert!(quiet(&p.waiter("s1", &worktree, 2).left()));
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert!(p.registered("s1").exists(), "it waited");
}

/// A Dispatch that does not know a hook's runtime, as an older one does not
/// know `claude-wait`, refuses it with status 1: Claude Code wakes a session
/// only for a background hook's status 2.
#[test]
fn a_runtime_this_dispatch_does_not_know_wakes_no_session() {
    let p = Project::new();
    let worktree = p.worktree("w");
    p.follow(&worktree);
    p.ask(&worktree);
    let output = p.hook("claude-later", &[], "s1", &worktree).left();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown agent runtime: claude-later"),
        "{stderr}"
    );
    assert!(output.stdout.is_empty());
    assert!(p.delivered().is_empty(), "the ask is still there");
}

#[test]
fn a_stop_of_any_size_delivers_the_ask() {
    let p = Project::new();
    let worktree = p.worktree("w");
    p.ask(&worktree);
    let huge = stop("s1", &worktree, false, &"x".repeat(1024 * 1024));
    assert_eq!(
        p.claude(&huge),
        serde_json::json!({"decision": "block", "reason": ASK})
    );
    // Nothing more to ask: as before, too large to follow.
    let reply = p.claude(&huge);
    let message = reply["systemMessage"].as_str().unwrap_or_default();
    assert!(message.contains("could not track this session"), "{reply}");
}

#[test]
fn a_live_agent_is_asked_once_to_catch_up_merges_and_its_work_continues() {
    let mut p = Project::new();
    let (wa, a) = p.session("a");
    let (wb, b) = p.session("b");
    // A changes scale's signature; B adds a use of it.
    let changed = LIB.replace("scale(n: i32)", "scale(n: i32, by: i32)");
    fs::write(wa.join("src/lib.rs"), changed.replace("n * 2", "n * by")).unwrap();
    let report = "pub fn report() -> i32 {\n    crate::scale(3)\n}\n";
    fs::write(wb.join("src/report.rs"), report).unwrap();

    p.start_owner();
    until("the owner sees B use what A changes", || {
        p.view().is_some_and(|view| {
            view["edges"].as_array().unwrap().iter().any(|edge| {
                edge["interactions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|i| i["rule"] == "uses")
            })
        })
    });
    p.ok(&["finish", &a]);
    until("the owner lists A's frozen patch", || p.lists_frozen(&a));
    p.ok(&["accept", &a]);
    until("B is asked", || !p.events(&b, "catchup.written").is_empty());

    // The turn B ends is kept going, once, with the ask.
    let reply = p.claude(&stop("b", &wb, false, "Done."));
    assert_eq!(reply["decision"], "block", "{reply}");
    let reason = reply["reason"].as_str().unwrap();
    let sources = p
        .state
        .join("projects")
        .join(key(&p.root))
        .join("sources.json");
    let sources: Value = serde_json::from_slice(&fs::read(sources).unwrap()).unwrap();
    let commit = sources[0]["commit"].as_str().unwrap();
    let lines: Vec<&str> = reason.lines().collect();
    assert_eq!(lines.len(), 4, "{reason}");
    let short = &a[..8];
    assert_eq!(
        lines[0],
        format!("Dispatch: Work {short} (Claude Code) landed a change this Work relies on.")
    );
    assert_eq!(
        lines[1],
        format!(
            "- scale (src/lib.rs), which this Work uses: {short} changed its signature, \
             pub fn scale(n: i32) -> i32 → pub fn scale(n: i32, by: i32) -> i32"
        )
    );
    assert_eq!(
        lines[2],
        format!(
            "These changes are in refs/dispatch/source (commit {}).",
            &commit[..8]
        )
    );
    // The command names the whole commit: Claude Code refuses to run one
    // that names the ref in a worktree.
    assert_eq!(
        lines[3],
        format!(
            "To catch up: commit your work, run git merge {commit}, update what uses them, and \
             run the project's checks. Dispatch asks once per landing."
        )
    );
    assert_eq!(p.claude(&stop("b", &wb, true, "Merged.")), Value::Null);
    assert_eq!(p.claude(&stop("b", &wb, false, "Done.")), Value::Null);

    // B does as asked: commits, merges the commit it was given, and updates
    // its use of scale.
    let given = lines[3]
        .split_once("run git merge ")
        .and_then(|(_, rest)| rest.split_once(','))
        .map(|(commit, _)| commit)
        .unwrap();
    git(&wb, &["add", "-A"]);
    git(&wb, &["commit", "--quiet", "-m", "report"]);
    git(&wb, &["merge", "--quiet", "--no-edit", given]);
    fs::write(
        wb.join("src/report.rs"),
        report.replace("scale(3)", "scale(3, 2)"),
    )
    .unwrap();
    git(
        &wb,
        &["commit", "--quiet", "-am", "report with the new scale"],
    );
    until("B's S0 moved to the landing", || {
        p.events(&b, "attach.s0_moved")
            .iter()
            .any(|moved| moved["landed_run"] == a.as_str())
    });
    until("B continues", || {
        p.run(&b)["coherence"]["validity"]["decision"] == "continue"
    });
    until("the ask is recorded as delivered", || {
        !p.events(&b, "catchup.delivered").is_empty()
    });
    p.stop_owner();
    let delivered = p.events(&b, "catchup.delivered");
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0]["via"], "stop");
    assert_eq!(delivered[0]["landings"], serde_json::json!([a]));
    assert_eq!(p.events(&b, "catchup.written").len(), 1, "once per landing");
    assert!(p.events(&b, "catchup.expired").is_empty());
}

/// A stale result whose session ended in its worktree, and whose S0 moved
/// once: `check` says to resume the session there and merge the newest
/// recorded source, named by its whole commit. Done so, the session goes on
/// as new Work from the same S0, the result closes as superseded, and the
/// merge moves the new Work's S0: it continues, with no agent run again.
#[test]
fn a_stale_result_is_resumed_in_its_worktree_merges_and_continues() {
    let mut p = Project::new();
    let (wa, a) = p.session("a");
    let (wb, b) = p.session("b");
    let (wc, c) = p.session("c");
    let changed = LIB.replace("scale(n: i32)", "scale(n: i32, by: i32)");
    fs::write(wa.join("src/lib.rs"), changed.replace("n * 2", "n * by")).unwrap();
    let report = "pub fn report() -> i32 {\n    crate::scale(3)\n}\n";
    fs::write(wb.join("src/report.rs"), report).unwrap();
    fs::write(wc.join("src/notes.rs"), "pub const NOTE: i32 = 1;\n").unwrap();
    p.start_owner();
    let recorded = |landing: usize| {
        let sources = p
            .state
            .join("projects")
            .join(key(&p.root))
            .join("sources.json");
        let sources: Value = serde_json::from_slice(&fs::read(sources).unwrap()).unwrap();
        sources[landing]["commit"].as_str().unwrap().to_owned()
    };

    // C lands first, and B merges it: B's S0 moves once.
    p.ok(&["finish", &c]);
    p.ok(&["accept", &c]);
    git(&wb, &["add", "-A"]);
    git(&wb, &["commit", "--quiet", "-m", "report"]);
    git(&wb, &["merge", "--quiet", "--no-edit", &recorded(0)]);
    until("B's S0 moved to C's landing", || {
        p.events(&b, "attach.s0_moved")
            .iter()
            .any(|moved| moved["landed_run"] == c.as_str())
    });
    // Then A lands a change to scale's signature, which B uses.
    p.ok(&["finish", &a]);
    p.ok(&["accept", &a]);

    // B's session ends, and its result is frozen stale.
    let end = payload(&[
        ("session_id", "b".into()),
        (
            "transcript_path",
            "/home/u/.claude/projects/p/b.jsonl".into(),
        ),
        ("cwd", wb.to_str().unwrap().into()),
        ("hook_event_name", "SessionEnd".into()),
        ("reason", "prompt_input_exit".into()),
    ]);
    assert_eq!(p.claude(&end), Value::Null);
    until("B's session is recorded as ended", || {
        !p.run(&b)["attachment"]["sessions"][0]["ended_at"].is_null()
    });
    p.ok(&["finish", &b]);
    let check = p.ok(&["check", &b]);
    let commit = recorded(1);
    assert!(check.contains("Coherence: REFRESH"), "{check}");
    let next = check
        .lines()
        .find(|line| line.starts_with("Next: "))
        .unwrap();
    assert_eq!(
        next,
        format!(
            "Next: resume its session in {}, then commit there and run git merge {commit}; the \
             session goes on as new Work from this Work's S0, and this Work closes as \
             superseded; Dispatch then moves that S0 and checks it again  (or dispatch reject {b})",
            wb.display()
        )
    );

    // Its session resumes there, and does as the step says.
    let resumed = p.claude(&payload(&[
        ("session_id", "b".into()),
        (
            "transcript_path",
            "/home/u/.claude/projects/p/b.jsonl".into(),
        ),
        ("cwd", wb.to_str().unwrap().into()),
        ("hook_event_name", "SessionStart".into()),
        ("source", "resume".into()),
        ("model", "claude-opus-5-5".into()),
    ]));
    let new = resumed["systemMessage"]
        .as_str()
        .and_then(|notice| notice.strip_prefix("Dispatch is tracking this worktree as Work "))
        .and_then(|rest| rest.split(';').next())
        .unwrap_or_else(|| panic!("not registered: {resumed}"))
        .to_owned();
    assert_ne!(new, b);
    until("B closes as superseded", || {
        p.run(&b)["outcome"]["closed"]["reason"] == "superseded"
    });
    assert_eq!(p.run(&b)["outcome"]["closed"]["replacement"], new.as_str());
    // Its work is committed already.
    git(&wb, &["merge", "--quiet", "--no-edit", &commit]);
    until("the new Work's S0 moved to the landing", || {
        p.events(&new, "attach.s0_moved")
            .iter()
            .any(|moved| moved["landed_run"] == a.as_str())
    });
    until("the new Work continues", || {
        p.run(&new)["coherence"]["validity"]["decision"] == "continue"
    });
    p.stop_owner();
}
