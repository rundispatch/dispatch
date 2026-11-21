//! Claude Code subagents in their own worktrees (`isolation: worktree`) as
//! Work. Each registers at `SubagentStart` under a session ID of its own,
//! though every hook inside it carries its parent's; takes its own notes and
//! asks and nobody else's; keeps its exact Δ at each `SubagentStop`; and ends
//! once its run stopped and no agent lock is held, or once its parent exits.
//! Payloads are the subagent probes' (Claude Code 2.1.296), in their exact
//! key orders, paths replaced. The parent is a stand-in process that holds
//! the agent locks, and every process a test starts is stopped when it ends.
#![cfg(unix)]

use std::{
    fs,
    io::Write,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The parent session every subagent payload names.
const PARENT: &str = "80a7793b-dddd-43a5-989c-f275463399fb";
/// The probe's subagents, as their payloads name them: B, A and C of the
/// first round, G of P0b, which its parent resumed, and H, which ran
/// without isolation.
const B: &str = "a85d8d5dd371b34d4";
const A: &str = "a3af667cafd45715f";
const C: &str = "a2e9ec256a5f3d587";
const G: &str = "a2f2fb9991cd71185";
const H: &str = "a242e902a82bd3dc9";
/// Claude Code's own agents: a prompt suggestion, and one of P0b's.
const INTERNAL: [&str; 2] = ["aa3af26b4aac9cbf2", "a6d043751d2f2e536"];
const KIND: &str = "general-purpose";

const LIB: &str = "pub fn scale(n: i32) -> i32 {\n    n * 2\n}\n";
const ASK: &str = "Dispatch: Work 01AAAAAA (Claude Code) landed a change this Work relies on.";

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=T", "-c", "user.email=t@example.invalid"])
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

fn key(path: &Path) -> String {
    hex::encode(Sha256::digest(path.as_os_str().as_encoded_bytes()))
}

/// Waits for what it waits for: only a hang reaches the bound.
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(300);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A payload with exactly these keys in this order, as Claude Code sends it:
/// what Dispatch reads from a payload's head depends on it.
fn payload(fields: &[(&str, Value)]) -> String {
    let fields: Vec<String> = fields
        .iter()
        .map(|(name, value)| format!("{}: {value}", Value::from(*name)))
        .collect();
    format!("{{{}}}", fields.join(", "))
}

/// The keys every payload begins with.
fn head(cwd: &Path) -> Vec<(&'static str, Value)> {
    vec![
        ("session_id", PARENT.into()),
        (
            "transcript_path",
            format!("/home/u/.claude/projects/p/{PARENT}.jsonl").into(),
        ),
        ("cwd", cwd.to_str().unwrap().into()),
        (
            "scratchpad_dir",
            format!("/tmp/claude-502/p/{PARENT}/scratchpad").into(),
        ),
        ("prompt_id", "e31ee188-6f61-4913-aa91-5a8aa630c69b".into()),
    ]
}

fn subagent_start(cwd: &Path, agent: &str, kind: &str) -> String {
    let mut fields = head(cwd);
    fields.extend([
        ("agent_id", agent.into()),
        ("agent_type", kind.into()),
        ("hook_event_name", "SubagentStart".into()),
    ]);
    payload(&fields)
}

/// `PostToolUse` of a `Read` in `cwd`, by `agent` (its ID and type) or, for
/// `None`, by the parent.
fn post_tool_use(cwd: &Path, agent: Option<(&str, &str)>) -> String {
    let mut fields = head(cwd);
    fields.push(("permission_mode", "acceptEdits".into()));
    if let Some((agent, kind)) = agent {
        fields.extend([("agent_id", agent.into()), ("agent_type", kind.into())]);
    }
    fields.extend([
        ("effort", json!({"level": "medium"})),
        ("hook_event_name", "PostToolUse".into()),
        ("tool_name", "Read".into()),
        ("tool_input", json!({"file_path": cwd.join("src/lib.rs")})),
        (
            "tool_response",
            json!({"type": "text", "file": {"content": LIB}}),
        ),
        ("tool_use_id", "toolu_01U5r94ff69p2sUecgiLaDu3".into()),
        ("duration_ms", 2.into()),
    ]);
    payload(&fields)
}

/// `SubagentStop` of `agent`; Claude Code's own agents send theirs with an
/// empty type and no last message (`last` is `None`).
fn subagent_stop(cwd: &Path, agent: &str, kind: &str, active: bool, last: Option<&str>) -> String {
    let mut fields = head(cwd);
    fields.extend([
        ("permission_mode", "acceptEdits".into()),
        ("agent_id", agent.into()),
        ("agent_type", kind.into()),
        ("effort", json!({"level": "medium"})),
        ("hook_event_name", "SubagentStop".into()),
        ("stop_hook_active", active.into()),
        (
            "agent_transcript_path",
            format!("/home/u/.claude/projects/p/{PARENT}/subagents/agent-{agent}.jsonl").into(),
        ),
    ]);
    if let Some(last) = last {
        fields.push(("last_assistant_message", last.into()));
    }
    fields.extend([
        (
            "background_tasks",
            json!([{"id": agent, "type": "subagent", "status": "running",
                    "description": "Subagent", "agent_type": kind}]),
        ),
        ("session_crons", json!([])),
    ]);
    payload(&fields)
}

/// The parent's `Stop`.
fn stop(cwd: &Path) -> String {
    let mut fields = head(cwd);
    fields.extend([
        ("permission_mode", "acceptEdits".into()),
        ("effort", json!({"level": "medium"})),
        ("hook_event_name", "Stop".into()),
        ("stop_hook_active", false.into()),
        (
            "last_assistant_message",
            "I started all three subagents.".into(),
        ),
        ("background_tasks", json!([])),
        ("session_crons", json!([])),
    ]);
    payload(&fields)
}

fn session_end(cwd: &Path, reason: &str) -> String {
    let mut fields = head(cwd);
    fields.extend([
        ("hook_event_name", "SessionEnd".into()),
        ("reason", reason.into()),
    ]);
    payload(&fields)
}

/// A permission prompt, inside `agent` when it is `Some`.
fn notification(cwd: &Path, agent: Option<&str>) -> String {
    let mut fields = head(cwd);
    if let Some(agent) = agent {
        fields.extend([("agent_id", agent.into()), ("agent_type", KIND.into())]);
    }
    fields.extend([
        ("hook_event_name", "Notification".into()),
        ("message", "Claude needs your permission".into()),
        ("notification_type", "permission_prompt".into()),
    ]);
    payload(&fields)
}

/// A stand-in for the parent Claude Code process: it runs until stopped, and
/// is reaped then, so its pid is gone, not a zombie.
struct Parent(Child);

impl Parent {
    fn start() -> Self {
        Self(Command::new("sleep").arg("600").spawn().unwrap())
    }

    /// The lock Claude Code writes on `agent`'s worktree.
    fn lock(&self, agent: &str) -> String {
        let pid = self.0.id();
        let output = Command::new("ps")
            .env("TZ", "UTC0")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let start = String::from_utf8(output.stdout).unwrap();
        let start = start.split_whitespace().collect::<Vec<_>>().join(" ");
        format!("claude agent agent-{agent} (pid {pid} start {start})")
    }

    fn stop(mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Parent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Project {
    _temp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    /// The serve lock while no owner runs: the project reads as watched.
    watched: Option<fs::File>,
}

impl Project {
    /// A watched project whose checks are `verify`.
    fn new(verify: &[&str]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), LIB).unwrap();
        let checks: String = verify.iter().map(|c| format!("    - {c:?}\n")).collect();
        let checks = match verify {
            [] => String::new(),
            _ => format!("checks:\n  verify:\n{checks}"),
        };
        fs::write(
            root.join("dispatch.yml"),
            format!("{checks}coherence:\n  poll_secs: 1\n"),
        )
        .unwrap();
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

    /// Run the owner: let go of the serve lock, wait until no process forked
    /// meanwhile holds it with us, and start it.
    fn start_owner(&mut self) {
        self.watched = None;
        until("the serve lock is free", || {
            let probe = fs::File::open(self.serve_lock()).unwrap();
            // SAFETY: a non-blocking probe on a descriptor this test owns.
            unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
        });
        self.ok(&["-vv", "start"]);
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
            .current_dir(&self.root)
            .stdin(Stdio::null());
        command
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.dispatch().args(args).output().unwrap();
        assert!(output.status.success(), "dispatch {args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn consent(&self) {
        let state = dispatch::state::State::discover(Some(self.state.clone())).unwrap();
        dispatch::consent::grant(&state, &self.root).unwrap();
    }

    /// `dispatch hook claude` with `input`, and what it printed.
    fn claude(&self, input: &str) -> Value {
        self.claude_within(input, 600_000)
    }

    /// `claude`, the hook given `budget` milliseconds from its start.
    fn claude_within(&self, input: &str, budget: u64) -> Value {
        let mut child = self
            .dispatch()
            .args(["hook", "claude"])
            .env("DISPATCH_REGISTRATION_BUDGET_MS", budget.to_string())
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

    /// The worktree Claude Code makes for subagent `agent`, from `base`, with
    /// its `CLAUDE_BASE`; locked by `parent` when it is `Some`.
    fn subagent(&self, agent: &str, base: &str, parent: Option<&Parent>) -> PathBuf {
        let path = format!(".claude/worktrees/agent-{agent}");
        let branch = format!("worktree-agent-{agent}");
        git(
            &self.root,
            &["worktree", "add", "--quiet", "-b", &branch, &path, base],
        );
        let worktree = fs::canonicalize(self.root.join(path)).unwrap();
        let head = git(&worktree, &["rev-parse", "HEAD"]);
        fs::write(self.gitdir(&worktree).join("CLAUDE_BASE"), head + "\n").unwrap();
        if let Some(parent) = parent {
            self.lock(&worktree, Some(&parent.lock(agent)));
        }
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

    /// `SubagentStart` in `agent`'s own worktree: its Work, now registered.
    fn start(&self, worktree: &Path, agent: &str) -> String {
        let reply = self.claude(&subagent_start(worktree, agent, KIND));
        assert!(
            reply["systemMessage"]
                .as_str()
                .is_some_and(|notice| notice.contains("tracking this worktree")),
            "{reply}"
        );
        self.work_in(worktree).expect("registered")
    }

    /// The newest Work in `workspace`.
    fn work_in(&self, workspace: &Path) -> Option<String> {
        self.runs()
            .into_iter()
            .rev()
            .find(|run| run["attachment"]["workspace"].as_str() == workspace.to_str())
            .map(|run| run["id"].as_str().unwrap().to_owned())
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

    /// Wait for the owner to change Work `id` until `ready` holds.
    fn until(&self, id: &str, what: &str, ready: impl Fn(&Value) -> bool) -> Value {
        let mut run = Value::Null;
        until(what, || {
            run = self.run(id);
            ready(&run)
        });
        run
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

    fn delta(&self, id: &str) -> String {
        fs::read_to_string(self.state.join("runs").join(id).join("delta.patch")).unwrap()
    }

    /// Edit `scale` in a worktree as an agent would.
    fn edit(&self, worktree: &Path, by: i32) {
        fs::write(
            worktree.join("src/lib.rs"),
            LIB.replace("2", &by.to_string()),
        )
        .unwrap();
    }

    fn watcher_log(&self) -> String {
        let name = format!("{}.log", key(&self.root));
        fs::read_to_string(self.state.join("watchers").join(name)).unwrap_or_default()
    }

    /// Wait until several owner ticks have run from start to end since now.
    fn ticks(&self) {
        let ticks = || self.watcher_log().matches(" tick: ").count();
        let ended = ticks();
        until("three owner ticks", || ticks() > ended + 3);
    }

    /// Leave a note for Work `id` in `workspace`, as the owner does.
    fn note(&self, workspace: &Path, id: &str, text: &str) {
        let inbox = self.state.join("inbox");
        fs::create_dir_all(inbox.join("delivered")).unwrap();
        let notes = json!({"version": 1, "run_id": id, "notes": [{"id": "n1", "text": text}]});
        fs::write(
            inbox.join(format!("{}.json", key(workspace))),
            notes.to_string(),
        )
        .unwrap();
    }

    /// Leave an ask to catch up for Work `id` in `workspace`, as the owner
    /// does, about a landing it never sees its S0 move past.
    fn ask(&self, workspace: &Path, id: &str) {
        let catchup = self.state.join("catchup");
        fs::create_dir_all(catchup.join("delivered")).unwrap();
        let landed = json!({
            "run_id": "01AAAAAAAAAAAAAAAAAAAAAAAA", "runtime": "Claude Code",
            "commit": "0000000000000000000000000000000000000000",
            "at": "2999-01-01T00:00:00Z", "targets": [],
        });
        let ask = json!({"version": 1, "run_id": id, "landings": [landed], "text": ASK});
        fs::write(
            catchup.join(format!("{}.json", key(workspace))),
            ask.to_string(),
        )
        .unwrap();
    }

    fn pending_ask(&self, workspace: &Path) -> bool {
        self.state
            .join("catchup")
            .join(format!("{}.json", key(workspace)))
            .exists()
    }

    fn pending_note(&self, workspace: &Path) -> bool {
        self.state
            .join("inbox")
            .join(format!("{}.json", key(workspace)))
            .exists()
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
        let digest = hex::encode(Sha256::digest(self.delta(id)));
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

fn session(run: &Value, at: usize) -> &Value {
    &run["attachment"]["sessions"][at]
}

fn open(run: &Value) -> bool {
    run["attachment"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["ended_at"].is_null())
}

fn ended(run: &Value) -> bool {
    !open(run)
}

fn stopped(run: &Value) -> bool {
    run["attachment"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| !s["subagent"]["stopped_at"].is_null())
}

fn finished(run: &Value) -> bool {
    run["outcome"]["lifecycle"] == "finished"
}

/// The tree of `commit`, in `dir`'s repository.
fn tree(dir: &Path, commit: &str) -> String {
    git(dir, &["rev-parse", &format!("{commit}^{{tree}}")])
}

#[test]
fn three_subagents_of_one_session_are_three_works_each_from_its_own_head() {
    let p = Project::new(&[]);
    let parent = Parent::start();
    let first = git(&p.root, &["rev-parse", "HEAD"]);
    fs::write(p.root.join("src/lib.rs"), LIB.replace("2", "3")).unwrap();
    git(&p.root, &["commit", "--quiet", "-am", "three"]);
    // C's worktree was made from a commit the root's HEAD has moved past.
    let wb = p.subagent(B, "HEAD", Some(&parent));
    let wa = p.subagent(A, "HEAD", Some(&parent));
    let wc = p.subagent(C, &first, Some(&parent));
    let b = p.start(&wb, B);
    // Claude Code's `agent-` prefix, there or not, names the same subagent.
    let a = p.start(&wa, &format!("agent-{A}"));
    let c = p.start(&wc, C);
    assert_eq!(p.claude(&subagent_start(&wa, A, KIND)), Value::Null);

    for (id, agent, worktree) in [(&b, B, &wb), (&a, A, &wa), (&c, C, &wc)] {
        let run = p.run(id);
        let sessions = run["attachment"]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "{run}");
        assert_eq!(sessions[0]["session_id"], format!("{PARENT}_agent-{agent}"));
        assert_eq!(sessions[0]["source"], "subagent");
        assert_eq!(
            sessions[0]["subagent"],
            json!({"parent_session": PARENT, "agent_id": agent, "agent_type": KIND,
                   "locked": true, "stopped_at": null, "kept_on": null})
        );
        assert_eq!(
            run["task"],
            format!("claude subagent {KIND} in agent-{}", &agent[..8])
        );
        // The parent's process is every subagent's: never one Work's agent.
        assert_eq!(run["attachment"]["agent_process"], Value::Null, "{run}");
        let s0 = run["attachment"]["provenance"]["workspace_at_start"]["commit"]
            .as_str()
            .unwrap();
        assert_eq!(tree(&p.root, s0), tree(worktree, "HEAD"), "{run}");
    }
    assert_ne!(tree(&wc, "HEAD"), tree(&p.root, "HEAD"));
    assert_eq!(p.runs().len(), 3);
    parent.stop();
}

/// A subagent's worktree, made from HEAD while a change sits uncommitted in
/// the root: its Work says what differed and Claude Code's word for that
/// worktree, never a launch or a setting its parent's Agent tool ignores.
#[test]
fn a_subagent_from_head_behind_uncommitted_source_is_told_only_what_its_worktree_holds() {
    let mut p = Project::new(&[]);
    let parent = Parent::start();
    fs::write(p.root.join("src/lib.rs"), LIB.replace("2", "3")).unwrap();
    let worktree = p.subagent(B, "HEAD", Some(&parent));
    p.start_owner();
    let b = p.start(&worktree, B);
    p.until(&b, "its source at start is observed", |run| {
        run["attachment"]["source_at_start"]["observed"].is_object()
    });

    let status = p.ok(&["status", &b]);
    let shown: Vec<&str> = status
        .lines()
        .skip_while(|line| !line.starts_with("  source at start:"))
        .enumerate()
        .take_while(|(n, line)| *n == 0 || line.starts_with("    "))
        .map(|(_, line)| line)
        .collect();
    assert_eq!(shown.len(), 2, "{status}");
    assert!(
        shown[0].starts_with("  source at start: differed from its S0 at 1 path (observed ")
            && shown[0].ends_with("): src/lib.rs"),
        "{status}"
    );
    assert_eq!(
        shown[1],
        "    Claude Code: a subagent's worktree starts from the checkout's HEAD; uncommitted changes are not copied."
    );
    for word in ["dispatch attach", "baseRef"] {
        assert!(!status.contains(word), "{status}");
    }
    parent.stop();
}

/// The parent's own `Stop` and `SessionEnd` carry its session ID, which no
/// subagent's Work holds: they leave that Work alone. A permission prompt
/// inside a subagent waits on the person in that subagent's Work alone, and
/// only that subagent's end clears it.
#[test]
fn the_parent_and_each_subagent_mark_only_their_own_work() {
    let mut p = Project::new(&[]);
    let parent = Parent::start();
    let wa = p.subagent(A, "HEAD", Some(&parent));
    let wb = p.subagent(B, "HEAD", Some(&parent));
    let a = p.start(&wa, A);
    let b = p.start(&wb, B);
    p.start_owner();
    assert_eq!(p.claude(&notification(&wa, Some(A))), Value::Null);
    let waiting = p.until(&a, "A waits on the person", |run| {
        !run["attachment"]["needs_you"].is_null()
    });
    assert_eq!(
        waiting["attachment"]["needs_you"]["session_id"],
        format!("{PARENT}_agent-{A}")
    );
    assert_eq!(p.claude(&stop(&p.root)), Value::Null);
    assert_eq!(
        p.claude(&session_end(&p.root, "prompt_input_exit")),
        Value::Null
    );
    p.ticks();
    for id in [&a, &b] {
        let run = p.run(id);
        assert!(open(&run), "{run}");
        assert!(p.events(id, "runtime.session_ended").is_empty());
    }
    assert!(p.run(&b)["attachment"]["needs_you"].is_null());

    // B ends: A still waits.
    p.lock(&wb, None);
    assert_eq!(
        p.claude(&subagent_stop(&wb, B, KIND, false, Some("Done."))),
        Value::Null
    );
    p.until(&b, "B's session ends", ended);
    p.ticks();
    assert!(!p.run(&a)["attachment"]["needs_you"].is_null());
    // A ends: its wait is over.
    p.lock(&wa, None);
    assert_eq!(
        p.claude(&subagent_stop(&wa, A, KIND, false, Some("Done."))),
        Value::Null
    );
    p.until(&a, "A's wait is cleared", |run| {
        run["attachment"]["needs_you"].is_null()
    });
    parent.stop();
}

#[test]
fn a_subagent_takes_its_notes_at_its_start_and_after_a_tool() {
    let mut p = Project::new(&[]);
    let parent = Parent::start();
    let worktree = p.subagent(G, "HEAD", Some(&parent));
    let id = p.start(&worktree, G);
    p.start_owner();
    let delivered = |via: &str| {
        p.events(&id, "note.delivered")
            .iter()
            .filter(|event| event["via"] == via)
            .count()
    };
    p.note(&worktree, &id, "Work 01BBBBBB changed scale.");
    let reply = p.claude(&post_tool_use(&worktree, Some((G, KIND))));
    assert_eq!(
        reply["hookSpecificOutput"],
        json!({"hookEventName": "PostToolUse", "additionalContext": "Work 01BBBBBB changed scale."})
    );
    assert_eq!(
        p.claude(&post_tool_use(&worktree, Some((G, KIND)))),
        Value::Null
    );
    until("the note is recorded as delivered", || {
        delivered("PostToolUse") == 1
    });

    // Its parent resumes it: the start delivers what waits since.
    p.lock(&worktree, None);
    assert_eq!(
        p.claude(&subagent_stop(&worktree, G, KIND, false, Some("Done."))),
        Value::Null
    );
    p.note(&worktree, &id, "Work 01CCCCCC landed.");
    let reply = p.claude(&subagent_start(&worktree, G, KIND));
    assert_eq!(
        reply["hookSpecificOutput"],
        json!({"hookEventName": "SubagentStart", "additionalContext": "Work 01CCCCCC landed."})
    );
    until("the second note is recorded as delivered", || {
        delivered("SubagentStart") == 1
    });
    assert_eq!(p.work_in(&worktree).as_deref(), Some(id.as_str()));
    parent.stop();
}

/// `SubagentStop` comes once per response. An ask keeps the run going once;
/// every stop that does not deliver one keeps the exact Δ as it is then, and
/// the session goes on while the agent lock is held.
#[test]
fn an_ask_keeps_a_subagent_going_once_and_each_stop_keeps_its_delta() {
    let p = Project::new(&[]);
    let parent = Parent::start();
    let worktree = p.subagent(A, "HEAD", Some(&parent));
    let id = p.start(&worktree, A);
    p.edit(&worktree, 3);
    p.ask(&worktree, &id);
    let reply = p.claude(&subagent_stop(&worktree, A, KIND, false, Some("Done.")));
    assert_eq!(reply, json!({"decision": "block", "reason": ASK}));
    assert!(
        p.events(&id, "subagent.stopped").is_empty(),
        "it did not stop"
    );

    p.edit(&worktree, 4);
    let reply = p.claude(&subagent_stop(&worktree, A, KIND, true, Some("Caught up.")));
    assert_eq!(reply, Value::Null);
    assert!(p.delta(&id).contains("+    n * 4"), "{}", p.delta(&id));
    p.edit(&worktree, 5);
    assert_eq!(
        p.claude(&subagent_stop(&worktree, A, KIND, false, Some("Done."))),
        Value::Null
    );
    assert!(p.delta(&id).contains("+    n * 5"), "{}", p.delta(&id));

    let run = p.run(&id);
    assert!(open(&run) && stopped(&run), "{run}");
    assert_eq!(run["outcome"]["lifecycle"], "working");
    assert_eq!(run["candidates"][0]["diff_stats"]["files_changed"], 1);
    let kept = p.events(&id, "subagent.stopped");
    assert_eq!(kept.len(), 2, "{kept:?}");
    assert!(
        kept.iter()
            .all(|event| event["agent_id"] == A && event["files_changed"] == 1)
    );
    let delivered: Vec<String> = fs::read_dir(p.state.join("catchup/delivered"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        delivered.len() == 1 && delivered[0].ends_with(".subagent_stop.json"),
        "{delivered:?}"
    );
    parent.stop();
}

/// The last message can make `SubagentStop` larger than Dispatch reads; what
/// it needs comes before it.
#[test]
fn a_subagent_stop_of_any_size_delivers_its_ask_and_keeps_its_delta() {
    let p = Project::new(&[]);
    let parent = Parent::start();
    let worktree = p.subagent(B, "HEAD", Some(&parent));
    let id = p.start(&worktree, B);
    p.edit(&worktree, 3);
    let long = "x".repeat(1_200_000);
    p.ask(&worktree, &id);
    let reply = p.claude(&subagent_stop(&worktree, B, KIND, false, Some(&long)));
    assert_eq!(reply, json!({"decision": "block", "reason": ASK}));
    let reply = p.claude(&subagent_stop(&worktree, B, KIND, true, Some(&long)));
    assert_eq!(reply, Value::Null);
    assert!(p.delta(&id).contains("+    n * 3"), "{}", p.delta(&id));
    assert_eq!(p.events(&id, "subagent.stopped").len(), 1);
    parent.stop();
}

/// A subagent without isolation and Claude Code's own agents work in their
/// parent's directory, here a `claude --worktree` session's, or wherever
/// its shell went, here a subagent's worktree. They register nothing there
/// and take none of the notes or asks of the Work there.
#[test]
fn a_subagent_without_isolation_and_claude_codes_own_agents_take_nothing() {
    let p = Project::new(&[]);
    let parent = Parent::start();
    let session = p.root.join(".claude/worktrees/session");
    git(
        &p.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "session",
            session.to_str().unwrap(),
        ],
    );
    let session = fs::canonicalize(session).unwrap();
    let reply = p.claude(&payload(&[
        ("session_id", PARENT.into()),
        (
            "transcript_path",
            "/home/u/.claude/projects/p/s.jsonl".into(),
        ),
        ("cwd", session.to_str().unwrap().into()),
        ("scratchpad_dir", "/tmp/claude-502/p/s".into()),
        ("hook_event_name", "SessionStart".into()),
        ("source", "startup".into()),
        ("model", "claude-opus-5-5".into()),
    ]));
    assert!(
        reply["systemMessage"]
            .as_str()
            .unwrap()
            .contains("tracking")
    );
    let theirs = p.work_in(&session).unwrap();
    let worktree = p.subagent(A, "HEAD", Some(&parent));
    let a = p.start(&worktree, A);
    for (workspace, id) in [(&session, &theirs), (&worktree, &a)] {
        p.note(workspace, id, "a note");
        p.ask(workspace, id);
    }

    // H, without isolation, in the session's worktree.
    let nothing = [
        subagent_start(&session, H, KIND),
        post_tool_use(&session, Some((H, KIND))),
        subagent_stop(&session, H, KIND, false, Some("api.py has health.")),
    ];
    // Claude Code's own agents, in the session's worktree, and in A's,
    // where the parent's shell went.
    let internal = INTERNAL.iter().flat_map(|agent| {
        [&session, &worktree].map(|cwd| subagent_stop(cwd, agent, "", false, None))
    });
    // A's own payloads with another agent's ID.
    let other = [
        subagent_start(&worktree, B, KIND),
        post_tool_use(&worktree, Some((B, KIND))),
        subagent_stop(&worktree, B, KIND, false, Some("Done.")),
    ];
    for input in nothing.into_iter().chain(internal).chain(other) {
        assert_eq!(p.claude(&input), Value::Null, "{input}");
    }
    assert_eq!(p.runs().len(), 2);
    for (workspace, id) in [(&session, &theirs), (&worktree, &a)] {
        assert!(p.pending_note(workspace) && p.pending_ask(workspace));
        assert!(p.events(id, "subagent.stopped").is_empty());
    }
    // Each Work's own agent takes them.
    let reply = p.claude(&post_tool_use(&session, None));
    assert_eq!(reply["hookSpecificOutput"]["additionalContext"], "a note");
    assert_eq!(p.claude(&stop(&session))["decision"], "block");
    let reply = p.claude(&post_tool_use(&worktree, Some((A, KIND))));
    assert_eq!(reply["hookSpecificOutput"]["additionalContext"], "a note");
    let reply = p.claude(&subagent_stop(&worktree, A, KIND, false, Some("Done.")));
    assert_eq!(reply["decision"], "block");
    parent.stop();
}

/// Claude Code removes an unchanged subagent worktree right after its stop,
/// with no `WorktreeRemove`: the Δ kept at the stop says nothing was lost.
/// Its project's check consent finishes nothing meanwhile.
#[test]
fn an_unchanged_subagent_worktree_removed_by_git_closes_its_work_with_no_changes() {
    let mut p = Project::new(&["true"]);
    p.consent();
    let parent = Parent::start();
    let worktree = p.subagent(B, "HEAD", Some(&parent));
    let id = p.start(&worktree, B);
    p.start_owner();
    assert_eq!(
        p.claude(&subagent_stop(&worktree, B, KIND, false, Some("Done."))),
        Value::Null
    );
    p.lock(&worktree, None);
    p.until(&id, "its session ends", ended);
    p.ticks();
    assert_eq!(p.run(&id)["outcome"]["lifecycle"], "working");
    git(&p.root, &["worktree", "remove", worktree.to_str().unwrap()]);
    let run = p.until(&id, "closed with no changes", finished);
    assert_eq!(run["outcome"]["work_result"], "cancelled", "{run}");
    assert_eq!(run["attempts"][0]["outcome"], "no_changes", "{run}");
    assert_eq!(
        run["attachment"]["workspace_removed"]["exact"], true,
        "{run}"
    );
    assert!(p.events(&id, "attach.finished").is_empty());
    let removed = p.events(&id, "workspace.removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(
        (&removed[0]["exact"], &removed[0]["no_changes"]),
        (&true.into(), &true.into())
    );
    parent.stop();
}

/// A changed subagent worktree gone, with its lock, before the owner looked
/// finishes by consent from the Δ kept at its stop, its exact Δ.
#[test]
fn a_changed_subagent_worktree_removed_after_its_stop_finishes_from_the_kept_delta() {
    let mut p = Project::new(&["true"]);
    p.consent();
    let parent = Parent::start();
    let worktree = p.subagent(B, "HEAD", Some(&parent));
    let id = p.start(&worktree, B);
    p.start_owner();
    p.edit(&worktree, 7);
    let input = subagent_stop(&worktree, B, KIND, false, Some("Done."));
    assert_eq!(p.claude(&input), Value::Null);
    git(
        &p.root,
        &["worktree", "remove", "-f", "-f", worktree.to_str().unwrap()],
    );

    let run = p.until(&id, "finished by consent", finished);
    assert_eq!(run["outcome"]["work_result"], "ready", "{run}");
    assert_eq!(run["attachment"]["finish_reason"], "by_consent");
    assert_eq!(run["attachment"]["workspace_removed"]["exact"], true);
    assert!(p.delta(&id).contains("+    n * 7"), "{}", p.delta(&id));
    assert_eq!(p.events(&id, "attach.finished").len(), 1);
    parent.stop();
}

/// The worktree stays after the stop, and a person, or the parent, edits it
/// again: its finish reads it, as any Work's does.
#[test]
fn an_edit_after_a_subagents_stop_is_in_its_finished_delta() {
    let mut p = Project::new(&["true"]);
    p.consent();
    let parent = Parent::start();
    let worktree = p.subagent(A, "HEAD", Some(&parent));
    let id = p.start(&worktree, A);
    p.start_owner();
    p.edit(&worktree, 7);
    let input = subagent_stop(&worktree, A, KIND, false, Some("Done."));
    assert_eq!(p.claude(&input), Value::Null);
    p.edit(&worktree, 8);
    p.lock(&worktree, None);
    let run = p.until(&id, "finished by consent", finished);
    assert_eq!(run["outcome"]["work_result"], "ready", "{run}");
    let delta = p.delta(&id);
    assert!(
        delta.contains("+    n * 8") && !delta.contains("n * 7"),
        "{delta}"
    );
    parent.stop();
}

/// Found in review: with 0.5.0's hooks still installed, a subagent is found
/// by its lock and gets no `SubagentStop`. Its lock going ends its session,
/// and its Work finishes by consent.
#[test]
fn a_subagent_found_with_no_hooks_ends_when_its_lock_goes() {
    let mut p = Project::new(&["true"]);
    p.consent();
    let parent = Parent::start();
    p.start_owner();
    let worktree = p.subagent(C, "HEAD", Some(&parent));
    until("discovery registers it", || p.work_in(&worktree).is_some());
    let id = p.work_in(&worktree).unwrap();
    p.edit(&worktree, 4);
    p.lock(&worktree, None);
    let run = p.until(&id, "finished by consent", finished);
    assert_eq!(session(&run, 0)["end_reason"], "lock_released", "{run}");
    assert_eq!(run["outcome"]["work_result"], "ready", "{run}");
    assert!(p.delta(&id).contains("+    n * 4"));
    parent.stop();
}

/// Found in review: a run that held its lock but whose stop never came (the
/// hook failed, or it was interrupted) ends when the lock goes.
#[test]
fn a_subagent_whose_lock_goes_without_a_stop_ends() {
    let mut p = Project::new(&[]);
    let parent = Parent::start();
    let worktree = p.subagent(B, "HEAD", Some(&parent));
    let id = p.start(&worktree, B);
    p.start_owner();
    p.ticks();
    assert!(open(&p.run(&id)));
    p.lock(&worktree, None);
    let run = p.until(&id, "its session ends", ended);
    assert_eq!(session(&run, 0)["end_reason"], "lock_released", "{run}");
    parent.stop();
}

/// Found in review: the parent is killed while its subagent's first run
/// holds the lock, which stays behind; resumed, the subagent writes none.
/// The stale lock ends the first run only, and the resumed run ends at its
/// stop.
#[test]
fn a_stale_agent_lock_ends_only_the_run_that_held_it() {
    let mut p = Project::new(&[]);
    let parent = Parent::start();
    let worktree = p.subagent(G, "HEAD", Some(&parent));
    let id = p.start(&worktree, G);
    p.start_owner();
    parent.stop();
    let run = p.until(&id, "the first run ends", ended);
    assert_eq!(session(&run, 0)["end_reason"], "process_gone");

    // The person resumes the parent, which sends the subagent a message.
    assert_eq!(p.claude(&subagent_start(&worktree, G, KIND)), Value::Null);
    p.edit(&worktree, 5);
    assert_eq!(
        p.claude(&post_tool_use(&worktree, Some((G, KIND)))),
        Value::Null
    );
    p.ticks();
    let run = p.run(&id);
    assert!(session(&run, 1)["ended_at"].is_null(), "{run}");
    let input = subagent_stop(&worktree, G, KIND, false, Some("Done."));
    assert_eq!(p.claude(&input), Value::Null);
    assert_eq!(session(&p.run(&id), 1)["end_reason"], "subagent_stop");
}

/// Found in review: a person starts Claude Code in a subagent's worktree, as
/// its REFRESH step advises, and their session joins its Work. That
/// session's lock is followed as any session's is: their process gone, the
/// session ends.
#[test]
fn a_persons_session_in_a_subagents_work_ends_with_its_process() {
    let mut p = Project::new(&[]);
    let parent = Parent::start();
    let worktree = p.subagent(A, "HEAD", Some(&parent));
    let id = p.start(&worktree, A);
    let input = subagent_stop(&worktree, A, KIND, false, Some("Done."));
    assert_eq!(p.claude(&input), Value::Null);
    p.lock(&worktree, None);
    p.start_owner();
    p.until(&id, "the subagent's session ends", ended);

    let person = Parent::start();
    let lock = person
        .lock("x")
        .replace("claude agent agent-x", &format!("claude session agent-{A}"));
    p.lock(&worktree, Some(&lock));
    let reply = p.claude(&payload(&[
        ("session_id", "4c1e0f6a-7d2b-4e59-9a83-0b6f2d1c5e77".into()),
        (
            "transcript_path",
            "/home/u/.claude/projects/p/person.jsonl".into(),
        ),
        ("cwd", worktree.to_str().unwrap().into()),
        (
            "scratchpad_dir",
            "/tmp/claude-502/p/person/scratchpad".into(),
        ),
        ("hook_event_name", "SessionStart".into()),
        ("source", "startup".into()),
        ("model", "claude-opus-5-5".into()),
    ]));
    assert_eq!(reply, Value::Null, "it joins the same Work");
    p.until(&id, "the person's process is seen", |run| {
        !run["attachment"]["agent_process"].is_null()
    });
    person.stop();
    let run = p.until(&id, "the person's session ends", ended);
    assert_eq!(session(&run, 1)["end_reason"], "process_gone", "{run}");
    parent.stop();
}

/// Found in review: in a project nobody watches, a subagent's stop decides
/// without the database, as every other hook there does. The database here
/// is unreadable, so opening it would fail the hook.
#[test]
fn a_subagent_stop_in_a_project_nobody_watches_opens_no_database() {
    let mut p = Project::new(&[]);
    let parent = Parent::start();
    let worktree = p.subagent(B, "HEAD", Some(&parent));
    let id = p.start(&worktree, B);
    p.watched = None;
    let db = p.state.join("dispatch.db");
    let kept = fs::read(&db).unwrap();
    fs::write(&db, b"not a database").unwrap();
    let input = subagent_stop(&worktree, B, KIND, false, Some("Done."));
    assert_eq!(p.claude(&input), Value::Null);
    fs::write(&db, kept).unwrap();
    assert!(p.events(&id, "subagent.stopped").is_empty());
    parent.stop();
}

/// Found in review: `SubagentStop` has a minute. Its Work busy past the
/// hook's deadline, it gives up within it and says so.
#[test]
fn a_subagent_stop_gives_up_within_its_deadline() {
    let p = Project::new(&[]);
    let parent = Parent::start();
    let worktree = p.subagent(A, "HEAD", Some(&parent));
    let id = p.start(&worktree, A);
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(p.state.join("runs").join(&id).join(".operation.lock"))
        .unwrap();
    // SAFETY: the descriptor stays open while the lock is held.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let began = Instant::now();
    let input = subagent_stop(&worktree, A, KIND, false, Some("Done."));
    let reply = p.claude_within(&input, 2_000);
    assert!(
        began.elapsed() < Duration::from_secs(15),
        "{:?}",
        began.elapsed()
    );
    let message = reply["systemMessage"].as_str().unwrap_or_default();
    assert!(
        message.contains("keeping this subagent's changes"),
        "{reply}"
    );
    drop(lock);
    parent.stop();
}

#[test]
fn the_parent_exiting_ends_its_subagents_sessions() {
    let mut p = Project::new(&[]);
    let parent = Parent::start();
    let wa = p.subagent(A, "HEAD", Some(&parent));
    let wb = p.subagent(B, "HEAD", Some(&parent));
    let a = p.start(&wa, A);
    let b = p.start(&wb, B);
    p.start_owner();
    p.ticks();
    assert!(open(&p.run(&a)) && open(&p.run(&b)));
    // It exits without a stop: the locks stay, naming a process that is gone.
    parent.stop();
    for id in [&a, &b] {
        let run = p.until(id, "its session ends", ended);
        assert_eq!(session(&run, 0)["end_reason"], "process_gone", "{run}");
        assert_eq!(run["outcome"]["lifecycle"], "working");
    }
}

/// A first run's session ends once its lock goes after its stop, and finish
/// by consent runs. Resumed by its parent, the subagent has no lock: its
/// start goes on from that Work's S0 as new Work, and its session ends at
/// its stop.
#[test]
fn a_resumed_subagent_with_no_lock_ends_at_its_stop_and_goes_on_from_its_s0() {
    let mut p = Project::new(&["true"]);
    p.consent();
    let parent = Parent::start();
    let worktree = p.subagent(G, "HEAD", Some(&parent));
    let first = p.start(&worktree, G);
    p.start_owner();
    p.edit(&worktree, 3);
    assert_eq!(
        p.claude(&subagent_stop(&worktree, G, KIND, false, Some("Done."))),
        Value::Null
    );
    p.ticks();
    assert!(open(&p.run(&first)), "the lock is held");
    p.lock(&worktree, None);
    let run = p.until(&first, "the first run finishes by consent", finished);
    assert_eq!(run["outcome"]["work_result"], "ready", "{run}");
    assert_eq!(session(&run, 0)["end_reason"], "lock_released");

    let reply = p.claude(&subagent_start(&worktree, G, KIND));
    let second = p.work_in(&worktree).unwrap();
    assert_ne!(second, first);
    assert!(
        reply["systemMessage"].as_str().unwrap().contains(&second),
        "{reply}"
    );
    p.edit(&worktree, 4);
    assert_eq!(
        p.claude(&post_tool_use(&worktree, Some((G, KIND)))),
        Value::Null
    );
    assert_eq!(
        p.claude(&subagent_stop(&worktree, G, KIND, false, Some("Done."))),
        Value::Null
    );
    let run = p.run(&second);
    assert!(ended(&run), "{run}");
    assert_eq!(session(&run, 0)["end_reason"], "subagent_stop");
    let run = p.until(&second, "the resumed run finishes by consent", finished);
    assert_eq!(run["outcome"]["work_result"], "ready", "{run}");
    assert_eq!(
        run["attachment"]["provenance"],
        p.run(&first)["attachment"]["provenance"]
    );
    assert!(p.delta(&second).contains("+    n * 4"));
    let closed = p.until(&first, "the first closes as superseded", |run| {
        !run["outcome"]["closed"].is_null()
    });
    assert_eq!(closed["outcome"]["closed"]["reason"], "superseded");
    assert_eq!(closed["outcome"]["closed"]["replacement"], second.as_str());
    parent.stop();
}

/// Resumed after its first run's lock went, but before the owner ended that
/// session, the subagent runs on in the same session, now without a lock:
/// nothing ends it but its next stop.
#[test]
fn a_subagent_resumed_before_its_session_ended_runs_on_in_the_same_work() {
    let mut p = Project::new(&[]);
    let parent = Parent::start();
    let worktree = p.subagent(B, "HEAD", Some(&parent));
    let id = p.start(&worktree, B);
    assert_eq!(
        p.claude(&subagent_stop(&worktree, B, KIND, false, Some("Done."))),
        Value::Null
    );
    p.lock(&worktree, None);
    assert_eq!(p.claude(&subagent_start(&worktree, B, KIND)), Value::Null);
    let run = p.run(&id);
    assert_eq!(run["attachment"]["sessions"].as_array().unwrap().len(), 1);
    let subagent = &session(&run, 0)["subagent"];
    assert!(subagent["stopped_at"].is_null(), "{run}");
    p.start_owner();
    p.ticks();
    assert!(open(&p.run(&id)), "it runs again");
    assert_eq!(
        p.claude(&subagent_stop(&worktree, B, KIND, false, Some("Done."))),
        Value::Null
    );
    assert_eq!(session(&p.run(&id), 0)["end_reason"], "subagent_stop");
    parent.stop();
}

/// Its lock may come after the owner looks, or never be seen: no lock is no
/// end before the run's stop.
#[test]
fn a_subagent_whose_lock_was_never_seen_does_not_end_before_its_stop() {
    let mut p = Project::new(&[]);
    let worktree = p.subagent(C, "HEAD", None);
    let id = p.start(&worktree, C);
    p.start_owner();
    p.ticks();
    let run = p.run(&id);
    assert!(open(&run), "{run}");
    assert!(p.events(&id, "runtime.session_ended").is_empty());
    assert_eq!(
        p.claude(&subagent_stop(&worktree, C, KIND, false, Some("Done."))),
        Value::Null
    );
    let run = p.run(&id);
    assert_eq!(session(&run, 0)["end_reason"], "subagent_stop", "{run}");
}

/// A subagent worktree that appears with no hook to register it is found by
/// its agent lock, with no process recorded for it: the lock names its
/// parent's.
#[test]
fn discovery_of_a_subagent_records_no_process() {
    let mut p = Project::new(&[]);
    let parent = Parent::start();
    p.start_owner();
    let worktree = p.subagent(A, "HEAD", Some(&parent));
    until("discovery registers it", || p.work_in(&worktree).is_some());
    let id = p.work_in(&worktree).unwrap();
    let run = p.run(&id);
    assert_eq!(run["attachment"]["agent_process"], Value::Null, "{run}");
    assert_eq!(session(&run, 0)["session_id"], format!("lock:agent-{A}"));
    assert_eq!(session(&run, 0)["subagent"]["agent_id"], A);
    assert_eq!(run["task"], format!("claude subagent in agent-{}", &A[..8]));
    assert!(
        run["attachment"]["provenance"]["discovered"].is_object(),
        "{run}"
    );
    p.ticks();
    assert!(open(&p.run(&id)));
    // Its hooks name the same Work, and its stop ends the session the lock
    // gave it once the lock goes.
    p.edit(&worktree, 3);
    assert_eq!(
        p.claude(&subagent_stop(&worktree, A, KIND, false, Some("Done."))),
        Value::Null
    );
    assert!(stopped(&p.run(&id)));
    p.lock(&worktree, None);
    let run = p.until(&id, "its session ends", ended);
    assert_eq!(session(&run, 0)["end_reason"], "lock_released");
    assert!(p.delta(&id).contains("+    n * 3"));
    parent.stop();
}

/// `dispatch accept` without a terminal: it proposes, decides nothing, and
/// names the batch digest to accept.
fn propose(p: &Project) -> (String, String) {
    let output = p.dispatch().arg("accept").output().unwrap();
    let (out, err) = (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    );
    assert!(
        !output.status.success() && err.contains("nothing was decided"),
        "{out}{err}"
    );
    let digest = err
        .split("dispatch accept --expect ")
        .nth(1)
        .expect("the command to accept the batch")
        .trim()
        .to_owned();
    (out, digest)
}

/// The proposal's line for its member at `position`, from 1.
fn member(proposal: &str, position: usize) -> Option<&str> {
    let prefix = format!("  {position}. ");
    proposal.lines().find_map(|line| line.strip_prefix(&prefix))
}

/// Why the proposal leaves `id` out: the line under it in "Left out:".
fn left_out<'a>(proposal: &'a str, id: &str) -> Option<&'a str> {
    let mut lines = proposal.lines().skip_while(|line| *line != "Left out:");
    lines.find(|line| line.starts_with(&format!("  {id}  ")))?;
    lines.next().map(str::trim_start)
}

/// Each subagent's run stops and its lock goes: its Work finishes by consent.
fn finish_by_stop(p: &Project, runs: &[(&Path, &str, &str)]) {
    for (worktree, agent, _) in runs {
        let input = subagent_stop(worktree, agent, KIND, false, Some("Done."));
        assert_eq!(p.claude(&input), Value::Null);
        p.lock(worktree, None);
    }
    for (_, _, id) in runs {
        let run = p.until(id, "finished by consent", finished);
        assert_eq!(run["outcome"]["work_result"], "ready", "{run}");
    }
}

/// A changer and a user, each a subagent, and a third whose branch the
/// parent, working in the checkout, merged there: one proposal lands the
/// changer, and leaves out the user as REFRESH, judged against the changer,
/// and the third as already in the source.
#[test]
fn subagent_results_land_through_one_batch_with_the_changer_first() {
    let mut p = Project::new(&["true"]);
    p.consent();
    let parent = Parent::start();
    let wa = p.subagent(A, "HEAD", Some(&parent));
    let wb = p.subagent(B, "HEAD", Some(&parent));
    let wc = p.subagent(C, "HEAD", Some(&parent));
    let (changer, user, merged) = (p.start(&wa, A), p.start(&wb, B), p.start(&wc, C));
    p.start_owner();
    let changed = LIB.replace("scale(n: i32)", "scale(n: i32, by: i32)");
    fs::write(wa.join("src/lib.rs"), changed.replace("n * 2", "n * by")).unwrap();
    let report = "pub fn report() -> i32 {\n    crate::scale(3)\n}\n";
    fs::write(wb.join("src/report.rs"), report).unwrap();
    fs::write(wc.join("src/extra.rs"), "pub fn extra() {}\n").unwrap();
    git(&wc, &["add", "-A"]);
    git(&wc, &["commit", "--quiet", "-m", "extra"]);
    finish_by_stop(
        &p,
        &[(&wb, B, &user), (&wa, A, &changer), (&wc, C, &merged)],
    );
    let branch = format!("worktree-agent-{C}");
    git(&p.root, &["merge", "--quiet", "--no-edit", &branch]);

    let (proposal, digest) = propose(&p);
    assert!(
        member(&proposal, 1).is_some_and(|line| line.starts_with(&format!("{changer}  Δ "))),
        "{proposal}"
    );
    assert!(member(&proposal, 2).is_none(), "{proposal}");
    assert!(
        left_out(&proposal, &user).is_some_and(|why| why.starts_with("REFRESH: fact_broken: ")),
        "{proposal}"
    );
    assert!(
        left_out(&proposal, &merged).is_some_and(|why| why.starts_with("STOP: ")),
        "{proposal}"
    );
    p.ok(&["accept", "--expect", &digest]);
    let lib = fs::read_to_string(p.root.join("src/lib.rs")).unwrap();
    assert!(lib.contains("by: i32"), "{lib}");
    assert!(!p.root.join("src/report.rs").exists());
    assert_eq!(
        fs::read_to_string(p.root.join("src/extra.rs")).unwrap(),
        "pub fn extra() {}\n"
    );
    assert_eq!(p.run(&changer)["outcome"]["review"], "accepted");
    assert_eq!(p.run(&user)["outcome"]["review"], "pending");
    assert_eq!(p.run(&merged)["outcome"]["application"], "not_applied");
    parent.stop();
}

/// One subagent lands a change to a signature another, still running, uses:
/// that one is asked once, at its next `SubagentStop`, merges, and its S0
/// moves.
#[test]
fn a_running_subagent_is_asked_once_at_its_stop_merges_and_its_s0_moves() {
    let mut p = Project::new(&["true"]);
    p.consent();
    let parent = Parent::start();
    let wa = p.subagent(A, "HEAD", Some(&parent));
    let wb = p.subagent(B, "HEAD", Some(&parent));
    let (a, b) = (p.start(&wa, A), p.start(&wb, B));
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
    let input = subagent_stop(&wa, A, KIND, false, Some("Done."));
    assert_eq!(p.claude(&input), Value::Null);
    p.lock(&wa, None);
    p.until(&a, "A finishes by consent", finished);
    until("the owner lists A's frozen patch", || p.lists_frozen(&a));
    p.ok(&["accept", &a]);
    until("B is asked", || !p.events(&b, "catchup.written").is_empty());

    // B catches up and stops before the owner looks again: the Δ kept at
    // that stop is on the S0 the merge then moves.
    p.stop_owner();
    let reply = p.claude(&subagent_stop(&wb, B, KIND, false, Some("Done.")));
    assert_eq!(reply["decision"], "block", "{reply}");
    let reason = reply["reason"].as_str().unwrap();
    let given = reason
        .split_once("run git merge ")
        .and_then(|(_, rest)| rest.split_once(','))
        .map(|(commit, _)| commit.to_owned())
        .unwrap_or_else(|| panic!("{reason}"));
    assert!(open(&p.run(&b)), "the ask keeps B going");
    git(&wb, &["add", "-A"]);
    git(&wb, &["commit", "--quiet", "-m", "report"]);
    git(&wb, &["merge", "--quiet", "--no-edit", &given]);
    let updated = report.replace("scale(3)", "scale(3, 2)");
    fs::write(wb.join("src/report.rs"), updated).unwrap();
    git(
        &wb,
        &["commit", "--quiet", "-am", "report with the new scale"],
    );
    let input = subagent_stop(&wb, B, KIND, true, Some("Merged."));
    assert_eq!(p.claude(&input), Value::Null);
    p.start_owner();
    until("B's S0 moved to the landing", || {
        p.events(&b, "attach.s0_moved")
            .iter()
            .any(|moved| moved["landed_run"] == a.as_str())
    });
    until("the ask is recorded as delivered", || {
        !p.events(&b, "catchup.delivered").is_empty()
    });
    let delivered = p.events(&b, "catchup.delivered");
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0]["via"], "subagent_stop");
    assert_eq!(delivered[0]["landings"], json!([a]));
    assert_eq!(p.events(&b, "catchup.written").len(), 1, "once per landing");

    // Its lock goes, and it finishes from its worktree on the moved S0: its
    // Δ is its own change alone.
    p.lock(&wb, None);
    let run = p.until(&b, "B finishes by consent", finished);
    assert_eq!(run["outcome"]["work_result"], "ready", "{run}");
    let delta = p.delta(&b);
    assert!(
        delta.contains("+    crate::scale(3, 2)") && !delta.contains("by: i32"),
        "{delta}"
    );
    parent.stop();
}

/// A parent working in a `claude --worktree` worktree of its own merges its
/// subagent's branch there, so its Work holds the subagent's change too. The
/// subagent finished first, so it comes first, and the parent's Work is left
/// out: the change lands once.
#[test]
fn a_subagent_whose_branch_the_parent_merged_into_its_own_work_lands_once() {
    let mut p = Project::new(&["true"]);
    p.consent();
    let parent = Parent::start();
    let own = p.root.join(".claude/worktrees/parent");
    git(
        &p.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "worktree-parent",
            own.to_str().unwrap(),
        ],
    );
    let own = fs::canonicalize(own).unwrap();
    let head = git(&own, &["rev-parse", "HEAD"]);
    fs::write(p.gitdir(&own).join("CLAUDE_BASE"), head + "\n").unwrap();
    let lock = parent
        .lock("x")
        .replace("claude agent agent-x", "claude session parent");
    p.lock(&own, Some(&lock));
    let reply = p.claude(&payload(&[
        ("session_id", PARENT.into()),
        (
            "transcript_path",
            format!("/home/u/.claude/projects/p/{PARENT}.jsonl").into(),
        ),
        ("cwd", own.to_str().unwrap().into()),
        (
            "scratchpad_dir",
            format!("/tmp/claude-502/p/{PARENT}/scratchpad").into(),
        ),
        ("hook_event_name", "SessionStart".into()),
        ("source", "startup".into()),
        ("model", "claude-opus-5-5".into()),
    ]));
    assert!(
        reply["systemMessage"]
            .as_str()
            .unwrap()
            .contains("tracking")
    );
    let theirs = p.work_in(&own).unwrap();
    let wc = p.subagent(C, "HEAD", Some(&parent));
    let merged = p.start(&wc, C);
    p.start_owner();
    fs::write(wc.join("src/extra.rs"), "pub fn extra() {}\n").unwrap();
    git(&wc, &["add", "-A"]);
    git(&wc, &["commit", "--quiet", "-m", "extra"]);
    finish_by_stop(&p, &[(&wc, C, &merged)]);
    git(
        &own,
        &[
            "merge",
            "--quiet",
            "--no-edit",
            &format!("worktree-agent-{C}"),
        ],
    );
    fs::write(own.join("src/cli.rs"), "pub fn main() {}\n").unwrap();
    p.lock(&own, None);
    assert_eq!(
        p.claude(&session_end(&p.root, "prompt_input_exit")),
        Value::Null
    );
    parent.stop();
    let run = p.until(&theirs, "the parent's Work finishes by consent", finished);
    assert_eq!(run["outcome"]["work_result"], "ready", "{run}");

    let (proposal, digest) = propose(&p);
    assert!(
        member(&proposal, 1).is_some_and(|line| line.starts_with(&format!("{merged}  Δ "))),
        "{proposal}"
    );
    assert!(member(&proposal, 2).is_none(), "{proposal}");
    assert!(
        left_out(&proposal, &theirs)
            .is_some_and(|why| why.starts_with("REFRESH: patch_conflict: ")),
        "{proposal}"
    );
    p.ok(&["accept", "--expect", &digest]);
    let extra = fs::read_to_string(p.root.join("src/extra.rs")).unwrap();
    assert_eq!(extra, "pub fn extra() {}\n");
    assert!(!p.root.join("src/cli.rs").exists());
    assert_eq!(p.run(&merged)["outcome"]["application"], "applied");
    assert_eq!(p.run(&theirs)["outcome"]["application"], "not_applied");
}
