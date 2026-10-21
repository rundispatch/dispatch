//! A session's end and its resume (contract C). With the person's consent for
//! the project's checks, the owner finishes Work once its session ended and
//! its process is gone, as a task that never holds up the owner's ticks; a
//! session that starts again over that result goes on from the same S0, and
//! the result closes as superseded. The payloads are shaped as Claude Code
//! 2.1.296 sends them (the P0 probe), with paths replaced; the session's
//! process is a stand-in that holds the worktree's `claude session` lock.
#![cfg(unix)]

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

use serde_json::Value;
use sha2::{Digest, Sha256};

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

/// Waits for what it waits for: only a hang reaches the bound.
fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(300);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal zero only asks whether the process exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// A stand-in for a Claude Code process: it runs until it is stopped, and
/// is reaped then, so its pid is gone, not a zombie.
struct Agent(Child);

impl Agent {
    fn start() -> Self {
        Self(Command::new("sleep").arg("600").spawn().unwrap())
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }

    /// The lock `claude --worktree <name>` holds while this process runs.
    fn lock(&self, name: &str) -> String {
        let output = Command::new("ps")
            .env("TZ", "UTC0")
            .args(["-o", "lstart=", "-p", &self.pid().to_string()])
            .output()
            .unwrap();
        let start = String::from_utf8(output.stdout).unwrap();
        format!(
            "claude session {name} (pid {} start {})",
            self.pid(),
            start.split_whitespace().collect::<Vec<_>>().join(" ")
        )
    }

    fn stop(mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The session of the probe's `claude --worktree probe-a`.
const SESSION: &str = "96fce6bd-fe23-4b1c-bd1e-b171635c421b";

struct Project {
    _temp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    /// Where the stand-in checks leave what they did.
    marks: PathBuf,
}

impl Project {
    /// A watched-to-be project whose check is `check`, a shell command run
    /// by `sh -lc` with `$MARKS` meaning `marks`.
    fn new(check: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let marks = fs::canonicalize(temp.path()).unwrap().join("marks");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(&marks).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn f() -> i32 {\n    1\n}\n").unwrap();
        let check = check.replace("$MARKS", &marks.display().to_string());
        fs::write(
            root.join("dispatch.yml"),
            format!("coherence:\n  poll_secs: 1\nchecks:\n  verify: ['{check}']\n"),
        )
        .unwrap();
        fs::write(root.join(".gitignore"), ".claude/\n").unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
        Self {
            root: fs::canonicalize(&root).unwrap(),
            state: temp.path().join("state"),
            marks,
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

    /// `dispatch start`, its owner with `-vv` so each tick logs a line.
    fn watch(&self) {
        let output = self.dispatch(&["-vv", "start"]);
        assert!(output.status.success(), "{}", text(&output));
    }

    fn consent(&self) {
        let state = dispatch::state::State::discover(Some(self.state.clone())).unwrap();
        dispatch::consent::grant(&state, &self.root).unwrap();
    }

    fn watcher_file(&self, suffix: &str) -> PathBuf {
        let key = hex::encode(Sha256::digest(self.root.to_string_lossy().as_bytes()));
        self.state.join("watchers").join(format!("{key}.{suffix}"))
    }

    fn ticks_ended(&self) -> usize {
        fs::read_to_string(self.watcher_file("log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(" tick: "))
            .count()
    }

    /// Wait until several owner ticks have run from start to end since now.
    fn ticks(&self) {
        let ended = self.ticks_ended();
        wait_until("three owner ticks", || self.ticks_ended() > ended + 3);
    }

    /// A worktree as `claude --worktree <name>` makes it: under
    /// `.claude/worktrees/`, with `CLAUDE_BASE`, locked by `agent`.
    fn worktree(&self, name: &str, agent: &Agent) -> PathBuf {
        git(
            &self.root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                &format!("worktree-{name}"),
                &format!(".claude/worktrees/{name}"),
            ],
        );
        let worktree = fs::canonicalize(self.root.join(".claude/worktrees").join(name)).unwrap();
        let head = git(&worktree, &["rev-parse", "HEAD"]);
        fs::write(
            self.gitdir(&worktree).join("CLAUDE_BASE"),
            format!("{head}\n"),
        )
        .unwrap();
        self.lock(&worktree, Some(&agent.lock(name)));
        worktree
    }

    fn gitdir(&self, worktree: &Path) -> PathBuf {
        let file = fs::read_to_string(worktree.join(".git")).unwrap();
        PathBuf::from(file.trim().strip_prefix("gitdir: ").unwrap())
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

    fn hook(&self, event: Value) -> String {
        let mut child = Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&self.state)
            .args(["hook", "claude"])
            // The registration deadline is not under test here.
            .env("DISPATCH_REGISTRATION_BUDGET_MS", "600000")
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

    /// `SessionStart`, as the probe recorded it, in `cwd`.
    fn session_start(&self, cwd: &Path, session: &str, source: &str) -> String {
        self.hook(serde_json::json!({
            "session_id": session,
            "transcript_path": format!("/dev/null/{session}.jsonl"),
            "cwd": cwd,
            "scratchpad_dir": format!("/tmp/scratch/{session}/scratchpad"),
            "hook_event_name": "SessionStart",
            "source": source,
            "model": "claude-opus-5-5",
        }))
    }

    /// `SessionEnd`, as the probe recorded it: `cwd` is the main checkout
    /// after a normal exit, the worktree otherwise.
    fn session_end(&self, cwd: &Path, session: &str, reason: &str) -> String {
        self.hook(serde_json::json!({
            "session_id": session,
            "transcript_path": format!("/dev/null/{session}.jsonl"),
            "cwd": cwd,
            "scratchpad_dir": format!("/tmp/scratch/{session}/scratchpad"),
            "prompt_id": "7974f0b8-4112-4e80-ba44-4c7054892b1a",
            "hook_event_name": "SessionEnd",
            "reason": reason,
        }))
    }

    fn runs(&self) -> Vec<Value> {
        let Ok(entries) = fs::read_dir(self.state.join("runs")) else {
            return Vec::new();
        };
        let mut runs: Vec<Value> = entries
            .filter_map(|entry| fs::read(entry.unwrap().path().join("metadata.json")).ok())
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
            .collect();
        runs.sort_by_key(|run| run["id"].as_str().unwrap().to_owned());
        runs
    }

    fn run(&self, id: &str) -> Value {
        let path = self.state.join("runs").join(id).join("metadata.json");
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn only_id(&self) -> String {
        let runs = self.runs();
        assert_eq!(runs.len(), 1, "{runs:?}");
        runs[0]["id"].as_str().unwrap().to_owned()
    }

    /// Wait for the owner to change run `id` until `ready` holds.
    fn until(&self, id: &str, what: &str, ready: impl Fn(&Value) -> bool) -> Value {
        let mut run = Value::Null;
        wait_until(what, || {
            run = self.run(id);
            ready(&run)
        });
        run
    }

    fn events(&self, id: &str, kind: &str) -> Vec<Value> {
        fs::read_to_string(self.state.join("runs").join(id).join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event["event_type"] == kind)
            .collect()
    }

    /// How many times the stand-in check ran.
    fn checks_run(&self) -> usize {
        fs::read_to_string(self.marks.join("ran"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    /// Edit the worktree as an agent would.
    fn edit(&self, worktree: &Path, value: i32) {
        fs::write(
            worktree.join("src/lib.rs"),
            format!("pub fn f() -> i32 {{\n    {value}\n}}\n"),
        )
        .unwrap();
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::write(self.marks.join("gate"), b"released\n");
        let _ = self.dispatch(&["stop"]);
    }
}

/// A check that counts its runs.
const COUNTED: &str = "echo ran >> $MARKS/ran";

/// A check that says it started, with its pid, and waits until the test
/// opens the gate.
const GATED: &str = "echo $$ > $MARKS/started; while [ ! -f $MARKS/gate ]; do sleep 0.1; done; echo ran >> $MARKS/ran";

fn finished(run: &Value) -> bool {
    run["outcome"]["lifecycle"] == "finished"
}

/// A session that registered Work in a Claude worktree locked by `agent`,
/// once the owner has seen that agent run. The worktree was there before
/// the watch began, so only the hook registers it: discovery would after
/// its grace, and a slow hook would then join that Work.
fn registered(p: &Project, worktree: &Path, agent: &Agent) -> String {
    let reply = p.session_start(worktree, SESSION, "startup");
    assert!(reply.contains("tracking this worktree"), "{reply}");
    let id = p.only_id();
    p.until(&id, "the agent seen running", |run| {
        run["attachment"]["agent_process"]["pid"] == agent.pid()
    });
    id
}

/// `/exit`: the lock goes, then `SessionEnd` from the main checkout, then
/// the process.
fn exit(p: &Project, worktree: &Path, agent: Agent) {
    p.lock(worktree, None);
    assert_eq!(p.session_end(&p.root, SESSION, "prompt_input_exit"), "");
    agent.stop();
}

#[test]
fn an_end_with_the_process_gone_finishes_the_work_and_its_checks_run_once() {
    let p = Project::new(COUNTED);
    p.consent();
    let agent = Agent::start();
    let worktree = p.worktree("x", &agent);
    p.watch();
    let id = registered(&p, &worktree, &agent);
    p.edit(&worktree, 2);
    exit(&p, &worktree, agent);

    let run = p.until(&id, "finished by consent", finished);
    assert_eq!(run["outcome"]["work_result"], "ready", "{run}");
    assert_eq!(run["outcome"]["verification"], "passed", "{run}");
    assert_eq!(run["attachment"]["finish_reason"], "by_consent");
    assert_eq!(run["attachment"]["workspace_removed"], Value::Null);
    assert_eq!(run["outcome"]["review"], "pending");
    assert_eq!(run["outcome"]["application"], "not_applied");
    let patch = fs::read_to_string(p.state.join("runs").join(&id).join("delta.patch")).unwrap();
    assert!(patch.contains("+    2"), "{patch}");
    assert_eq!(
        fs::read_to_string(p.root.join("src/lib.rs")).unwrap(),
        "pub fn f() -> i32 {\n    1\n}\n",
        "nothing is applied"
    );
    p.ticks();
    assert_eq!(p.checks_run(), 1);
    assert_eq!(p.events(&id, "attach.finished").len(), 1);
}

/// `/clear` ends a session in a process that keeps running: nothing
/// finishes, and the session that follows joins the same Work.
#[test]
fn an_end_with_the_process_alive_finishes_nothing() {
    let p = Project::new(COUNTED);
    p.consent();
    let agent = Agent::start();
    let worktree = p.worktree("x", &agent);
    p.watch();
    let id = registered(&p, &worktree, &agent);
    p.edit(&worktree, 2);
    assert_eq!(p.session_end(&worktree, SESSION, "clear"), "");
    p.ticks();
    let run = p.run(&id);
    assert_eq!(run["outcome"]["lifecycle"], "working", "{run}");
    assert!(run["attachment"]["sessions"][0]["ended_at"].is_string());
    assert_eq!(p.checks_run(), 0);

    let cleared = "091aab7f-0000-4000-8000-000000000000";
    assert_eq!(p.session_start(&worktree, cleared, "clear"), "");
    assert_eq!(p.only_id(), id);
    assert_eq!(
        p.run(&id)["attachment"]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    agent.stop();
}

/// A closed terminal (`other`) leaves the lock behind, naming a pid that is
/// gone: that counts as gone.
#[test]
fn a_lock_left_behind_by_a_gone_pid_counts_as_gone() {
    let p = Project::new(COUNTED);
    p.consent();
    let agent = Agent::start();
    let worktree = p.worktree("x", &agent);
    p.watch();
    let id = registered(&p, &worktree, &agent);
    p.edit(&worktree, 2);
    let lock = fs::read_to_string(p.gitdir(&worktree).join("locked")).unwrap();
    assert_eq!(p.session_end(&worktree, SESSION, "other"), "");
    agent.stop();
    assert_eq!(
        fs::read_to_string(p.gitdir(&worktree).join("locked")).unwrap(),
        lock,
        "the lock is still there"
    );
    let run = p.until(&id, "finished by consent", finished);
    assert_eq!(run["outcome"]["verification"], "passed", "{run}");
    assert_eq!(p.checks_run(), 1);
}

#[test]
fn without_consent_a_session_end_is_only_recorded() {
    let p = Project::new(COUNTED);
    let agent = Agent::start();
    let worktree = p.worktree("x", &agent);
    p.watch();
    let id = registered(&p, &worktree, &agent);
    p.edit(&worktree, 2);
    exit(&p, &worktree, agent);
    p.until(&id, "the agent seen gone", |run| {
        run["attachment"]["agent_process"].is_null()
    });
    p.ticks();
    let run = p.run(&id);
    assert_eq!(run["outcome"]["lifecycle"], "working", "{run}");
    assert_eq!(
        run["attachment"]["sessions"][0]["end_reason"],
        "prompt_input_exit"
    );
    assert_eq!(p.checks_run(), 0);
}

/// While a finish's checks run, the owner's ticks go on, and another Work's
/// verdict follows the source as it moves. The finish here is the one a
/// removed worktree gets, which the owner once waited for within its tick.
#[test]
fn ticks_and_other_verdicts_go_on_while_a_finish_runs() {
    let p = Project::new(GATED);
    p.consent();
    let agent = Agent::start();
    let worktree = p.worktree("x", &agent);
    // Other Work, its session still open, editing what the source will move.
    let other_agent = Agent::start();
    let other = p.worktree("y", &other_agent);
    p.watch();
    let id = registered(&p, &worktree, &agent);
    p.edit(&worktree, 2);

    let reply = p.session_start(&other, "d992b6e4-0000-4000-8000-000000000000", "startup");
    assert!(reply.contains("tracking this worktree"), "{reply}");
    let other_id = p
        .runs()
        .into_iter()
        .map(|run| run["id"].as_str().unwrap().to_owned())
        .find(|other| *other != id)
        .unwrap();
    fs::write(other.join("src/lib.rs"), "pub fn f() -> i32 {\n    3\n}\n").unwrap();
    p.until(&other_id, "a first verdict", |run| {
        run["coherence"]["validity"]["world_changed"] == false
    });

    let removed = p.hook(serde_json::json!({
        "hook_event_name": "WorktreeRemove", "session_id": SESSION, "cwd": p.root,
        "worktree_path": worktree, "name": "x",
    }));
    assert!(
        removed.contains("will run the project's checks"),
        "{removed}"
    );
    wait_until("the finish's check started", || {
        p.marks.join("started").is_file()
    });
    let ticked = p.ticks_ended();
    wait_until("ticks while the check runs", || {
        p.ticks_ended() > ticked + 3
    });
    fs::write(p.root.join("src/lib.rs"), "pub fn f() -> i32 {\n    4\n}\n").unwrap();
    git(&p.root, &["commit", "--quiet", "-am", "moved"]);
    p.until(&other_id, "the other verdict follows the move", |run| {
        run["coherence"]["validity"]["world_changed"] == true
    });
    assert_eq!(p.run(&id)["outcome"]["lifecycle"], "working");
    assert_eq!(p.checks_run(), 0, "the check is still waiting");

    fs::write(p.marks.join("gate"), b"released\n").unwrap();
    let run = p.until(&id, "finished by consent", finished);
    assert_eq!(run["outcome"]["work_result"], "ready", "{run}");
    assert_eq!(p.checks_run(), 1);
    agent.stop();
    other_agent.stop();
}

/// The S0 commit a run records in the project's repository.
fn s0(run: &Value) -> String {
    let provenance = &run["attachment"]["provenance"];
    provenance["workspace_at_start"]["commit"]
        .as_str()
        .or(provenance["discovered"]["commit"].as_str())
        .unwrap_or_else(|| panic!("no S0 commit: {run}"))
        .to_owned()
}

/// The ID of the run other than `id`.
fn other_than(p: &Project, id: &str) -> String {
    let runs = p.runs();
    assert_eq!(runs.len(), 2, "{runs:?}");
    runs.into_iter()
        .map(|run| run["id"].as_str().unwrap().to_owned())
        .find(|other| other != id)
        .unwrap()
}

/// The old Work closed as superseded by the new one, its Δ kept, and the new
/// one began from the old S0, so its Δ holds both sessions' changes.
fn assert_reopened(p: &Project, old: &str, new: &str) {
    let old_run = p.until(old, "the old Work closed", |run| {
        run["outcome"]["closed"].is_object()
    });
    let new_run = p.run(new);
    assert_eq!(old_run["outcome"]["closed"]["reason"], "superseded");
    assert_eq!(old_run["outcome"]["closed"]["replacement"], new);
    assert_eq!(old_run["outcome"]["review"], "not_requested");
    assert_eq!(old_run["outcome"]["application"], "not_applied");
    let kept = fs::read_to_string(p.state.join("runs").join(old).join("delta.patch")).unwrap();
    assert!(kept.contains("+    2"), "{kept}");
    let [closed] = p.events(old, "work.closed").try_into().unwrap();
    assert_eq!(closed["payload"]["by"], "reopen");
    assert_eq!(closed["payload"]["replacement"], new);

    assert_eq!(s0(&new_run), s0(&old_run));
    assert_eq!(
        new_run["attachment"]["provenance"],
        old_run["attachment"]["provenance"]
    );
    assert_eq!(new_run["outcome"]["lifecycle"], "working");
    let [reopened] = p.events(new, "work.reopened").try_into().unwrap();
    assert_eq!(reopened["payload"]["reopens"], old);
    // S0 is the source before either session: the old change is in Δ.
    let baseline = PathBuf::from(new_run["baseline_path"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(baseline.join("src/lib.rs")).unwrap(),
        "pub fn f() -> i32 {\n    1\n}\n"
    );
    assert!(
        !p.state.join("reopen").join(old).exists(),
        "the note is gone"
    );
}

/// Here a person finished the Work, and the session ends after.
#[test]
fn a_session_that_starts_again_after_a_finish_goes_on_from_the_old_s0() {
    let p = Project::new(COUNTED);
    p.consent();
    let agent = Agent::start();
    let worktree = p.worktree("x", &agent);
    p.watch();
    let old = registered(&p, &worktree, &agent);
    p.edit(&worktree, 2);
    let finish = p.dispatch(&["finish", &old]);
    assert!(finish.status.success(), "{}", text(&finish));
    exit(&p, &worktree, agent);

    // `claude --worktree x --resume <id>`: the new process locks the
    // worktree, then `SessionStart` comes with the same session ID.
    let resumed = Agent::start();
    p.lock(&worktree, Some(&resumed.lock("x")));
    let reply = p.session_start(&worktree, SESSION, "resume");
    assert!(reply.contains("tracking this worktree"), "{reply}");
    let new = other_than(&p, &old);
    assert_reopened(&p, &old, &new);
    assert_eq!(p.checks_run(), 1, "only the old finish ran checks");
    // The new Work's Δ holds both sessions' changes.
    fs::write(worktree.join("notes.txt"), "later\n").unwrap();
    exit(&p, &worktree, resumed);
    let run = p.until(&new, "the new Work finished", finished);
    let mut changed: Vec<String> =
        serde_json::from_value(run["candidates"][0]["diff_stats"]["changed_files"].clone())
            .unwrap();
    changed.sort();
    assert_eq!(changed, ["notes.txt", "src/lib.rs"]);
}

/// Once `git gc` has pruned the old S0, the session starts Work of its own,
/// says why it could not go on from the old one, and leaves that for review.
#[test]
fn a_session_whose_old_s0_is_gone_starts_work_of_its_own() {
    let p = Project::new(COUNTED);
    p.consent();
    let agent = Agent::start();
    let worktree = p.worktree("x", &agent);
    p.watch();
    let old = registered(&p, &worktree, &agent);
    p.edit(&worktree, 2);
    let finish = p.dispatch(&["finish", &old]);
    assert!(finish.status.success(), "{}", text(&finish));
    exit(&p, &worktree, agent);
    let pruned = s0(&p.run(&old));
    fs::remove_file(
        p.root
            .join(".git/objects")
            .join(&pruned[..2])
            .join(&pruned[2..]),
    )
    .unwrap();

    let resumed = Agent::start();
    p.lock(&worktree, Some(&resumed.lock("x")));
    let reply = p.session_start(&worktree, SESSION, "resume");
    assert!(reply.contains("tracking this worktree"), "{reply}");
    let new = other_than(&p, &old);
    let [refused] = p.events(&new, "work.not_reopened").try_into().unwrap();
    assert_eq!(refused["payload"]["reopens"], old);
    let reason = refused["payload"]["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("its S0 commit {pruned} can no longer be read")),
        "{reason}"
    );
    assert!(p.events(&new, "work.reopened").is_empty());
    let new_run = p.run(&new);
    assert_ne!(s0(&new_run), pruned);
    assert_eq!(new_run["outcome"]["lifecycle"], "working");
    // Its S0 is the worktree as the session found it, old change and all.
    let baseline = PathBuf::from(new_run["baseline_path"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(baseline.join("src/lib.rs")).unwrap(),
        "pub fn f() -> i32 {\n    2\n}\n"
    );

    p.ticks();
    let old_run = p.run(&old);
    assert_eq!(old_run["outcome"]["closed"], Value::Null, "{old_run}");
    assert_eq!(old_run["outcome"]["review"], "pending");
    assert!(p.events(&old, "work.closed").is_empty());
    assert!(!p.state.join("reopen").join(&old).exists());
    resumed.stop();
}

/// A person who types `finish` or `close` while the owner finishes the Work
/// by their consent is told so, not only that it is busy.
#[test]
fn a_typed_command_during_the_owners_finish_says_why_it_waits() {
    let p = Project::new(GATED);
    p.consent();
    let agent = Agent::start();
    let worktree = p.worktree("x", &agent);
    p.watch();
    let id = registered(&p, &worktree, &agent);
    p.edit(&worktree, 2);
    exit(&p, &worktree, agent);
    wait_until("the finish's check started", || {
        p.marks.join("started").is_file()
    });

    let why = format!(
        "Work {id} is being finished by its project's owner, as you allowed (checks running); \
         wait for it, or close it once it is ready"
    );
    for typed in [
        vec!["finish", id.as_str()],
        vec!["close", id.as_str(), "--reason", "unfinished"],
    ] {
        let output = p.dispatch(&typed);
        assert!(!output.status.success(), "{typed:?}");
        assert!(text(&output).contains(&why), "{typed:?}: {}", text(&output));
    }

    fs::write(p.marks.join("gate"), b"released\n").unwrap();
    let run = p.until(&id, "finished by consent", finished);
    assert_eq!(run["outcome"]["closed"], Value::Null, "{run}");
    assert!(
        !p.state
            .join("runs")
            .join(&id)
            .join("finishing-by-consent")
            .exists()
    );
}

#[test]
fn a_session_that_starts_again_during_a_finish_cancels_its_checks() {
    let p = Project::new(GATED);
    p.consent();
    let agent = Agent::start();
    let worktree = p.worktree("x", &agent);
    p.watch();
    let old = registered(&p, &worktree, &agent);
    p.edit(&worktree, 2);
    exit(&p, &worktree, agent);
    wait_until("the finish's check started", || {
        fs::read_to_string(p.marks.join("started")).is_ok_and(|pid| !pid.trim().is_empty())
    });
    let check: u32 = fs::read_to_string(p.marks.join("started"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(alive(check));

    let resumed = Agent::start();
    p.lock(&worktree, Some(&resumed.lock("x")));
    let reply = p.session_start(&worktree, SESSION, "resume");
    assert!(reply.contains("tracking this worktree"), "{reply}");
    let new = other_than(&p, &old);
    assert_reopened(&p, &old, &new);
    wait_until("the check killed", || !alive(check));
    let old_run = p.run(&old);
    assert_eq!(old_run["outcome"]["work_result"], "cancelled", "{old_run}");
    assert_eq!(p.events(&old, "attach.finished").len(), 0);
    assert_eq!(p.checks_run(), 0, "the check never got past its gate");
    resumed.stop();
}

/// Without any hook: discovery registers the session, liveness sees it end,
/// the owner finishes it, and a new session's lock reopens it.
#[test]
fn discovery_reopens_work_when_a_new_session_locks_its_worktree() {
    let p = Project::new(COUNTED);
    p.consent();
    p.watch();
    // Made after the watch began, so discovery registers it.
    let agent = Agent::start();
    let worktree = p.worktree("x", &agent);
    wait_until("discovered Work", || !p.runs().is_empty());
    let old = p.only_id();
    p.edit(&worktree, 2);
    // A closed terminal: the lock stays, naming a process that is gone.
    agent.stop();
    let run = p.until(&old, "finished by consent", finished);
    assert_eq!(
        run["attachment"]["sessions"][0]["end_reason"],
        "process_gone"
    );

    let resumed = Agent::start();
    p.lock(&worktree, Some(&resumed.lock("x")));
    wait_until("the reopened Work", || p.runs().len() == 2);
    let new = other_than(&p, &old);
    assert_reopened(&p, &old, &new);
    let created = p.events(&new, "attach.created");
    assert_eq!(created[0]["payload"]["via"], "discovery");
    resumed.stop();
}
