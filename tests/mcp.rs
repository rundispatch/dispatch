//! `dispatch mcp`, driven by a scripted client over a pipe. The handshakes
//! and calls are the clients' own, as packet M0 recorded them (Claude Code
//! 2.1.296, Codex 0.170.0, Cursor Agent 2026.11.13), with paths replaced.
//! The Work is stand-in Work: linked worktrees attached as foreign work that
//! the test edits, and Claude Code sessions and a subagent whose hooks the
//! test sends, shaped as Claude Code sends them. No agent runs, and every
//! process a test starts is stopped when it ends.
#![cfg(unix)]

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{BufRead, BufReader, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Output, Stdio},
    sync::mpsc,
    time::{Duration, Instant, SystemTime},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const AUTH: &str = "pub fn validate(token: &str) -> bool {\n    let a = 1;\n    let b = 2;\n    \
let c = 3;\n    let d = 4;\n    let e = 5;\n    let f = 6;\n    let g = 7;\n    let h = 8;\n    \
!token.is_empty() && a + b + c + d + e + f + g + h > 0\n}\n\npub fn refresh(token: &str) -> String {\n    \
token.to_owned()\n}\n";
const SIGNATURE: (&str, &str) = (
    "validate(token: &str)",
    "validate(token: &str, strict: bool)",
);
const CALLER: &str = "pub fn serve() {}\n\npub fn login(t: &str) -> bool {\n    validate(t)\n}\n";
const DUP: &str = "pub fn dup() {}\n";

/// The Claude Code session of `.claude/worktrees/x`, and the parent session
/// and subagent of `.claude/worktrees/agent-<SUBAGENT>`, as the probes named
/// them.
const SESSION: &str = "96fce6bd-fe23-4b1c-bd1e-b171635c421b";
const PARENT: &str = "80a7793b-dddd-43a5-989c-f275463399fb";
const SUBAGENT: &str = "a85d8d5dd371b34d4";

const TOOLS: [&str; 6] = ["work", "status", "wait", "start", "propose_landing", "send"];

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
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The lock at `path`, held as Dispatch holds one until it is dropped;
/// `None` when another holds it.
fn hold(path: &Path) -> Option<Held> {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .unwrap();
    // SAFETY: the descriptor stays open while the lock is held.
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (locked == 0).then_some(Held(file))
}

/// A lock `hold` took. Dropped, it unlocks, as Dispatch's `OperationLock`
/// does, before its descriptor closes: a child another test's thread is
/// starting holds a copy of every descriptor until it execs, and a lock
/// let go of only by closing stays held that long.
struct Held(fs::File);

impl Drop for Held {
    fn drop(&mut self) {
        // SAFETY: unlocks only the descriptor this guard locked.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Waits for what it waits for: only a hang reaches the bound.
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(300);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn verdict(run: &Value) -> &Value {
    &run["coherence"]["validity"]["decision"]
}

/// A payload with exactly these keys in this order, as Claude Code sends it.
fn payload(fields: &[(&str, Value)]) -> String {
    let fields: Vec<String> = fields
        .iter()
        .map(|(name, value)| format!("{}: {value}", Value::from(*name)))
        .collect();
    format!("{{{}}}", fields.join(", "))
}

/// A stand-in for a Claude Code process: it runs until it is stopped.
struct Agent(Child);

impl Agent {
    fn start() -> Self {
        Self(Command::new("sleep").arg("600").spawn().unwrap())
    }

    /// The lock `claude --worktree <name>` holds while this process runs.
    fn lock(&self, name: &str) -> String {
        self.holding(&format!("session {name}"))
    }

    /// The lock this process, a parent session, holds on the worktree of
    /// its subagent `agent_id`.
    fn subagent_lock(&self, agent_id: &str) -> String {
        self.holding(&format!("agent agent-{agent_id}"))
    }

    fn holding(&self, what: &str) -> String {
        let output = Command::new("ps")
            .env("TZ", "UTC0")
            .args(["-o", "lstart=", "-p", &self.0.id().to_string()])
            .output()
            .unwrap();
        let start = String::from_utf8(output.stdout).unwrap();
        let start = start.split_whitespace().collect::<Vec<_>>().join(" ");
        format!("claude {what} (pid {} start {start})", self.0.id())
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A waiter this test started, stopped when the test ends unless it left.
struct Waiter(Option<Child>);

impl Waiter {
    /// How it left, once it does.
    fn left(mut self) -> Output {
        let child = self.0.as_mut().unwrap();
        until("the waiter left", || child.try_wait().unwrap().is_some());
        self.0.take().unwrap().wait_with_output().unwrap()
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

struct Project {
    temp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    /// The serve lock while no owner runs: the project reads as watched.
    watched: Option<Held>,
}

impl Project {
    /// A Git project whose `checks.verify` is `verify`.
    fn new(verify: &[&str]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/auth.rs"), AUTH).unwrap();
        fs::write(root.join("src/api.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(root.join("src/cli.rs"), "pub fn main() {}\n").unwrap();
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
        Self {
            root: fs::canonicalize(&root).unwrap(),
            state: temp.path().join("state"),
            temp,
            watched: None,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(assert_cmd::cargo_bin!("dispatch"));
        command
            .arg("--state-dir")
            .arg(&self.state)
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::null());
        command
    }

    fn dispatch(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.dispatch(args);
        assert!(
            output.status.success(),
            "dispatch {args:?}: {}",
            text(&output)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Watch the project with an owner, `dispatch start`.
    fn start(&self) {
        self.ok(&["start"]);
    }

    /// Stop the owner and hold its serve lock instead: the project reads as
    /// watched, and nothing changes its Work any more.
    fn quiet(&mut self) {
        self.ok(&["stop"]);
        let file = hold(&self.lock("serve"));
        assert!(file.is_some(), "the owner let go of the serve lock");
        self.watched = file;
    }

    /// The project's serve or source lock.
    fn lock(&self, kind: &str) -> PathBuf {
        let key = hex::encode(Sha256::digest(self.root.as_os_str().as_encoded_bytes()));
        self.state.join("locks").join(format!("{kind}-{key}.lock"))
    }

    /// A linked worktree attached as foreign Work, not yet finished.
    fn attach(&self, name: &str, files: &[(&str, &str)]) -> String {
        let workspace = self.temp.path().join(name);
        let path = workspace.to_str().unwrap();
        git(
            &self.root,
            &["worktree", "add", "--quiet", "-b", name, path],
        );
        let out = self.ok(&[
            "attach",
            "--workspace",
            path,
            "--agent",
            name,
            "--task",
            &format!("the task of {name}"),
            "--allow-unsafe-local",
        ]);
        for (path, contents) in files {
            fs::write(workspace.join(path), contents).unwrap();
        }
        out.lines()
            .find_map(|line| line.strip_prefix("ATTACHED "))
            .expect("attach prints ATTACHED <id>")
            .to_owned()
    }

    /// `attach`, then `finish`: a result waiting for review.
    fn work(&self, name: &str, files: &[(&str, &str)]) -> String {
        let id = self.attach(name, files);
        self.ok(&["finish", &id]);
        id
    }

    fn run(&self, id: &str) -> Value {
        let path = self.state.join("runs").join(id).join("metadata.json");
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    /// Wait until Work `id` is as `ready` says, and when it was first seen so.
    fn until(&self, id: &str, what: &str, ready: impl Fn(&Value) -> bool) -> Instant {
        until(what, || ready(&self.run(id)));
        Instant::now()
    }

    fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = fs::read_dir(self.state.join("runs"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        ids.sort();
        ids
    }

    /// Every event as committed: id, run, kind and payload.
    fn events(&self) -> Vec<(i64, String, String, String)> {
        let db = rusqlite::Connection::open(self.state.join("dispatch.db")).unwrap();
        let mut statement = db
            .prepare("SELECT id, run_id, event_type, payload_json FROM events ORDER BY id")
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn of(&self, kind: &str) -> usize {
        self.events().iter().filter(|event| event.2 == kind).count()
    }

    /// `dispatch hook claude` with `input`; it must succeed.
    fn hook(&self, input: &str) -> String {
        let mut child = self
            .command(&["hook", "claude"])
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
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", text(&output));
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// A worktree under `.claude/worktrees/<name>`, as Claude Code makes one.
    fn claude_worktree(&self, name: &str, base: bool) -> PathBuf {
        let path = format!(".claude/worktrees/{name}");
        let branch = format!("worktree-{name}");
        git(
            &self.root,
            &["worktree", "add", "--quiet", "-b", &branch, &path],
        );
        let worktree = fs::canonicalize(self.root.join(path)).unwrap();
        if base {
            let head = git(&worktree, &["rev-parse", "HEAD"]);
            fs::write(self.gitdir(&worktree).join("CLAUDE_BASE"), head).unwrap();
        }
        worktree
    }

    fn gitdir(&self, worktree: &Path) -> PathBuf {
        let file = fs::read_to_string(worktree.join(".git")).unwrap();
        PathBuf::from(file.trim().strip_prefix("gitdir: ").unwrap())
    }

    /// The fields every Claude Code payload of `session` begins with.
    fn head(&self, session: &str, cwd: &Path) -> Vec<(&'static str, Value)> {
        vec![
            ("session_id", session.into()),
            (
                "transcript_path",
                self.transcript(session).to_str().unwrap().into(),
            ),
            ("cwd", cwd.to_str().unwrap().into()),
            (
                "scratchpad_dir",
                format!("/tmp/scratch/{session}/scratchpad").into(),
            ),
            ("prompt_id", "7e92d8c0-ed66-464a-845b-98f1cc4cff83".into()),
        ]
    }

    /// The session's transcript, last written long before any mark, so that
    /// no mark is cleared by it.
    fn transcript(&self, session: &str) -> PathBuf {
        let path = fs::canonicalize(self.temp.path())
            .unwrap()
            .join(format!("{session}.jsonl"));
        if !path.exists() {
            fs::write(&path, "{}\n").unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1 << 30))
                .unwrap();
        }
        path
    }

    /// `SessionStart` in `cwd`: the Work it registered.
    fn session_start(&self, cwd: &Path) -> String {
        let mut fields = self.head(SESSION, cwd);
        fields.extend([
            ("hook_event_name", "SessionStart".into()),
            ("source", "startup".into()),
            ("model", "claude-opus-5-5".into()),
        ]);
        let reply = self.hook(&payload(&fields));
        assert!(reply.contains("tracking this worktree"), "{reply}");
        self.newest_in(cwd)
    }

    /// `Notification` of `kind` in `cwd`, as the probe recorded it.
    fn notification(&self, cwd: &Path, kind: &str, message: &str) {
        let mut fields = self.head(SESSION, cwd);
        fields.extend([
            ("hook_event_name", "Notification".into()),
            ("notification_type", kind.into()),
            ("message", message.into()),
        ]);
        assert_eq!(self.hook(&payload(&fields)), "");
    }

    /// `SessionEnd` after `/exit`, from the main checkout.
    fn session_end(&self) {
        let mut fields = self.head(SESSION, &self.root);
        fields.extend([
            ("hook_event_name", "SessionEnd".into()),
            ("reason", "prompt_input_exit".into()),
        ]);
        assert_eq!(self.hook(&payload(&fields)), "");
    }

    /// `SubagentStart` of a general-purpose subagent in its own worktree.
    fn subagent_start(&self, cwd: &Path) -> String {
        let mut fields = self.head(PARENT, cwd);
        fields.extend([
            ("agent_id", SUBAGENT.into()),
            ("agent_type", "general-purpose".into()),
            ("hook_event_name", "SubagentStart".into()),
        ]);
        let reply = self.hook(&payload(&fields));
        assert!(reply.contains("tracking this worktree"), "{reply}");
        self.newest_in(cwd)
    }

    /// `SessionStart` of `session` in `cwd`: the Work it registered.
    fn session_start_as(&self, session: &str, cwd: &Path) -> String {
        let mut fields = self.head(session, cwd);
        fields.extend([
            ("hook_event_name", "SessionStart".into()),
            ("source", "startup".into()),
            ("model", "claude-opus-5-5".into()),
        ]);
        let reply = self.hook(&payload(&fields));
        assert!(reply.contains("tracking this worktree"), "{reply}");
        self.newest_in(cwd)
    }

    /// What a hook gave the agent, as Claude Code reads its reply.
    fn reply(&self, fields: &[(&str, Value)]) -> Value {
        match self.hook(&payload(fields)).trim() {
            "" => Value::Null,
            reply => serde_json::from_str(reply).unwrap(),
        }
    }

    /// `SessionStart` of `SESSION` in `cwd`, resumed.
    fn resume(&self, cwd: &Path) -> Value {
        let mut fields = self.head(SESSION, cwd);
        fields.extend([
            ("hook_event_name", "SessionStart".into()),
            ("source", "resume".into()),
            ("model", "claude-opus-5-5".into()),
        ]);
        self.reply(&fields)
    }

    /// `PostToolUse` of a `Bash` call in `cwd`, as the probe recorded it:
    /// `SESSION`'s, or inside the subagent `SUBAGENT` when `subagent`.
    fn tool_use(&self, cwd: &Path, subagent: bool) -> Value {
        let session = if subagent { PARENT } else { SESSION };
        let mut fields = self.head(session, cwd);
        fields.push(("permission_mode", "auto".into()));
        if subagent {
            fields.extend([
                ("agent_id", SUBAGENT.into()),
                ("agent_type", "general-purpose".into()),
            ]);
        }
        fields.extend([
            ("effort", json!({"level": "high"})),
            ("hook_event_name", "PostToolUse".into()),
            ("tool_name", "Bash".into()),
            (
                "tool_input",
                json!({"command": "ls", "description": "List files"}),
            ),
            (
                "tool_response",
                json!({"stdout": "lib.rs\n", "stderr": "", "interrupted": false}),
            ),
            ("tool_use_id", "toolu_012CZdy6zhyfJAasPA9MioXo".into()),
            ("duration_ms", 3519.into()),
        ]);
        self.reply(&fields)
    }

    /// `Stop` of `session` in `cwd`, as the probe recorded it.
    fn stop(&self, session: &str, cwd: &Path) -> String {
        let mut fields = self.head(session, cwd);
        fields.extend([
            ("permission_mode", "auto".into()),
            ("effort", json!({"level": "high"})),
            ("hook_event_name", "Stop".into()),
            ("stop_hook_active", false.into()),
            ("last_assistant_message", "Done.".into()),
            ("background_tasks", json!([])),
            ("session_crons", json!([])),
        ]);
        payload(&fields)
    }

    /// `SubagentStop` of `SUBAGENT` in `cwd`, as the probe recorded it.
    fn subagent_stop(&self, cwd: &Path) {
        let mut fields = self.head(PARENT, cwd);
        fields.extend([
            ("permission_mode", "auto".into()),
            ("agent_id", SUBAGENT.into()),
            ("agent_type", "general-purpose".into()),
            ("effort", json!({"level": "high"})),
            ("hook_event_name", "SubagentStop".into()),
            ("stop_hook_active", false.into()),
            (
                "agent_transcript_path",
                format!("/tmp/subagents/agent-{SUBAGENT}.jsonl").into(),
            ),
            ("last_assistant_message", "Done.".into()),
            ("background_tasks", json!([])),
            ("session_crons", json!([])),
        ]);
        assert_eq!(self.reply(&fields), Value::Null);
    }

    /// `dispatch hook claude-wait`, the waiter `Stop` of `session` in `cwd`
    /// starts, once it waits.
    fn waiter(&self, session: &str, cwd: &Path) -> Waiter {
        let mut child = self
            .command(&["hook", "claude-wait"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(self.stop(session, cwd).as_bytes()).unwrap();
        drop(stdin);
        let waiter = Waiter(Some(child));
        until("the waiter waits", || {
            self.state.join("waiters").join(session).exists()
        });
        waiter
    }

    /// Let go of the serve lock held for the project, and watch it with an
    /// owner.
    fn watch(&mut self) {
        self.watched = None;
        // A process forked meanwhile holds the lock with this one until it
        // execs.
        until("the serve lock is free", || {
            hold(&self.lock("serve")).is_some()
        });
        self.start();
    }

    /// The payloads of the events of `kind`.
    fn payloads(&self, kind: &str) -> Vec<Value> {
        let events = self.events().into_iter().filter(|event| event.2 == kind);
        events
            .map(|event| serde_json::from_str(&event.3).unwrap())
            .collect()
    }

    fn newest_in(&self, workspace: &Path) -> String {
        self.ids()
            .into_iter()
            .rev()
            .find(|id| self.run(id)["attachment"]["workspace"].as_str() == workspace.to_str())
            .expect("registered")
    }

    /// Launch consent for `agents`, as setup grants it: each with what its
    /// launch runs on the host as the project is now.
    fn allow(&self, agents: &[&str], max_running: usize) {
        let state = dispatch::state::State::discover(Some(self.state.clone())).unwrap();
        let (config, _) = dispatch::Config::discover(&self.root, None).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let commands = |agent: &str| {
            let now = dispatch::consent::native_commands(&self.root, agent, &config);
            runtime.block_on(now).unwrap()
        };
        let consent = dispatch::consent::LaunchConsent {
            agents: agents
                .iter()
                .map(|agent| ((*agent).to_owned(), commands(agent)))
                .collect(),
            max_running,
        };
        dispatch::consent::grant_launch(&state, &self.root, &consent).unwrap();
    }

    /// Whether run `id` was created: its `metadata.json` is written.
    fn created(&self, id: &str) -> bool {
        self.state
            .join("runs")
            .join(id)
            .join("metadata.json")
            .exists()
    }

    /// The ids `start` launched, by the task files it wrote.
    fn launched(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.state.join("started")) else {
            return Vec::new();
        };
        let names = entries.map(|entry| entry.unwrap().file_name().into_string().unwrap());
        let mut ids: Vec<String> = names
            .filter_map(|name| name.strip_suffix(".task").map(str::to_owned))
            .collect();
        ids.sort();
        ids
    }

    /// The process group of the agent run `id` started, once its launch is
    /// recorded.
    fn agent_group(&self, id: &str) -> Option<i32> {
        let db = dispatch::db::Database::open(self.state.join("dispatch.db")).unwrap();
        let launches = dispatch::launch::launches_for_run(&db, id).unwrap();
        launches
            .into_iter()
            .filter(|launch| launch.state == dispatch::launch::LaunchState::Spawned)
            .find_map(|launch| launch.child?.process_group)
    }

    /// The `dispatch run` launched as `id`, while it runs.
    fn launch_pid(&self, id: &str) -> Option<i32> {
        let found = Command::new("pgrep")
            .args(["-f", &format!("run-id={id}")])
            .output()
            .unwrap();
        String::from_utf8_lossy(&found.stdout).trim().parse().ok()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        self.watched = None;
        let _ = self.dispatch(&["stop"]);
        // Each run `start` launched stops its agent and records how it
        // ended, a run a test stopped too.
        for id in self.launched() {
            let Some(pid) = self.launch_pid(&id) else {
                continue;
            };
            // SAFETY: signals to the one process this test launched.
            unsafe {
                libc::kill(pid, libc::SIGCONT);
                libc::kill(pid, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(60);
            while self.launch_pid(&id).is_some() {
                if Instant::now() > deadline {
                    // SAFETY: as above.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// `dispatch mcp` as a client sees it. Every line it writes is checked as
/// it is read: one JSON-RPC message, answering once a request this client
/// sent, or progress for a token this client sent. Anything else fails.
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<(Instant, String)>,
    next: u64,
    /// Request ids sent and not answered yet, and progress tokens sent,
    /// each as JSON.
    asked: HashSet<String>,
    tokens: HashSet<String>,
    /// Messages read while looking for another.
    read: Vec<(Instant, Value)>,
}

impl Mcp {
    fn start(p: &Project, cwd: &Path, args: &[&str]) -> Self {
        let mut child = Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&p.state)
            .arg("mcp")
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).split(b'\n') {
                let Ok(line) = line else { return };
                let line = String::from_utf8(line).expect("stdout is UTF-8");
                let _ = sender.send((Instant::now(), line));
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            lines,
            next: 100,
            asked: HashSet::new(),
            tokens: HashSet::new(),
            read: Vec::new(),
        }
    }

    fn send_bytes(&mut self, line: &[u8]) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(line).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    /// Send one message as it is written, noting the request id and the
    /// progress token it carries.
    fn send(&mut self, line: &str) {
        if let Ok(message) = serde_json::from_str::<Value>(line)
            && message.get("method").is_some()
        {
            if let Some(id) = message.get("id") {
                assert!(self.asked.insert(id.to_string()), "{id} sent twice");
            }
            let token = &message["params"]["_meta"]["progressToken"];
            if !token.is_null() {
                self.tokens.insert(token.to_string());
            }
        }
        self.send_bytes(line.as_bytes());
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = json!(self.next);
        self.next += 1;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send(&request.to_string());
        id
    }

    /// The next message, checked; `None` when none comes within `within`.
    fn next(&mut self, within: Duration) -> Option<(Instant, Value)> {
        let (at, line) = match self.lines.recv_timeout(within) {
            Ok(read) => read,
            Err(mpsc::RecvTimeoutError::Timeout) => return None,
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("dispatch mcp exited"),
        };
        let message: Value = serde_json::from_str(&line)
            .unwrap_or_else(|_| panic!("stdout line is not JSON: {line:?}"));
        assert_eq!(message["jsonrpc"], "2.0", "{line}");
        if let Some(method) = message.get("method") {
            assert_eq!(method, "notifications/progress", "{line}");
            assert!(message.get("id").is_none(), "{line}");
            let token = message["params"]["progressToken"].to_string();
            assert!(
                self.tokens.contains(&token),
                "progress for a token not sent: {line}"
            );
        } else {
            let id = message["id"].to_string();
            assert!(
                self.asked.remove(&id),
                "answers no request in flight: {line}"
            );
            let answers = ["result", "error"].map(|key| message.get(key).is_some());
            assert!(
                answers == [true, false] || answers == [false, true],
                "{line}"
            );
        }
        Some((at, message))
    }

    /// The answer to request `id`, and when it came.
    fn answer(&mut self, id: &Value) -> (Instant, Value) {
        if let Some(at) = self.read.iter().position(|(_, m)| m.get("id") == Some(id)) {
            return self.read.remove(at);
        }
        loop {
            let read = self.next(Duration::from_secs(300)).expect("an answer");
            if read.1.get("id") == Some(id) {
                return read;
            }
            self.read.push(read);
        }
    }

    /// The progress read so far for `token`.
    fn progress(&self, token: &Value) -> Vec<&Value> {
        self.read
            .iter()
            .map(|(_, message)| message)
            .filter(|message| message["params"]["progressToken"] == *token)
            .collect()
    }

    /// The three messages every client begins with, as Claude Code sends them.
    fn initialize(&mut self) {
        let id = self.request(
            "initialize",
            json!({"protocolVersion": "2025-11-25", "capabilities": {},
                   "clientInfo": {"name": "claude-code", "version": "2.1.296"}}),
        );
        assert!(self.answer(&id).1["result"]["protocolVersion"].is_string());
        self.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    }

    fn call(&mut self, name: &str, arguments: Value) -> Value {
        let id = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        self.answer(&id).1
    }

    /// A tool's result, parsed from its text.
    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        result(&self.call(name, arguments))
    }

    /// A tool's refusal: one plain sentence.
    fn refused(&mut self, name: &str, arguments: Value) -> String {
        refusal(&self.call(name, arguments))
    }

    /// Close stdin: the server exits 0 within a second, writing nothing more.
    fn close(mut self) {
        drop(self.stdin.take());
        let closed = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                closed.elapsed() < Duration::from_secs(1),
                "dispatch mcp still runs a second after stdin closed"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success(), "{status}");
        if let Ok((_, line)) = self.lines.recv_timeout(Duration::from_secs(5)) {
            panic!("written after stdin closed: {line}");
        }
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A tool call's answer, checked as a result, its text parsed.
fn result(answer: &Value) -> Value {
    let result = &answer["result"];
    assert_ne!(result["isError"], true, "{answer}");
    let [content] = &result["content"].as_array().expect("content")[..] else {
        panic!("one content item: {answer}");
    };
    assert_eq!(content["type"], "text", "{answer}");
    let text = content["text"].as_str().unwrap();
    assert!(!text.contains('\n'), "compact JSON: {text}");
    serde_json::from_str(text).unwrap()
}

/// A tool call's answer, checked as a refusal: one sentence that says what
/// to do.
fn refusal(answer: &Value) -> String {
    let result = &answer["result"];
    assert_eq!(result["isError"], true, "{answer}");
    let text = result["content"][0]["text"].as_str().unwrap().to_owned();
    let sentence = text.ends_with('.')
        && !text.contains('\n')
        && text.matches(". ").count() == 0
        && text.len() < 200;
    assert!(sentence, "one plain sentence: {text:?}");
    text
}

fn error_code(answer: &Value) -> i64 {
    answer["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("an error: {answer}"))
}

/// Everything a landing, review or close would change: the source's HEAD,
/// index and files, `refs/dispatch/*`, every run as stored, and the events.
#[derive(Debug, PartialEq)]
struct Snapshot {
    head: String,
    index: String,
    refs: String,
    files: BTreeMap<PathBuf, String>,
    runs: BTreeMap<String, Value>,
    events: Vec<(i64, String, String, String)>,
}

impl Snapshot {
    fn of(p: &Project) -> Self {
        // Every file of the project and of each Work's worktree; Git's own
        // directories and Dispatch's state are not the source.
        let files = walkdir::WalkDir::new(p.temp.path())
            .into_iter()
            .filter_entry(|entry| entry.file_name() != ".git" && entry.path() != p.state)
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                let digest = Sha256::digest(fs::read(entry.path()).unwrap());
                (entry.path().to_owned(), hex::encode(digest))
            })
            .collect();
        Self {
            head: git(&p.root, &["rev-parse", "HEAD"]),
            index: git(&p.root, &["ls-files", "--stage"]),
            refs: git(&p.root, &["for-each-ref", "refs/dispatch"]),
            files,
            runs: p
                .ids()
                .into_iter()
                .map(|id| (id.clone(), p.run(&id)))
                .collect(),
            events: p.events(),
        }
    }
}

/// The Work of `every_state`, by what it is.
struct States {
    running: String,
    needs_you: String,
    ready: String,
    refresh: String,
    stop: String,
    landed: String,
    closed: String,
    subagent: String,
}

impl States {
    fn all(&self) -> [&str; 8] {
        [
            &self.running,
            &self.needs_you,
            &self.ready,
            &self.refresh,
            &self.stop,
            &self.landed,
            &self.closed,
            &self.subagent,
        ]
    }
}

/// Work in every state there is: running, waiting on the person, ready,
/// REFRESH, STOP, landed, closed, and a subagent. The owner judged each,
/// then stopped: nothing changes the Work after, and the project reads as
/// watched.
fn every_state() -> (Project, States) {
    let mut p = Project::new(&[]);
    p.start();
    let landed = p.work(
        "wt-landed",
        &[("src/cli.rs", "pub fn main() {\n    run();\n}\n")],
    );
    p.ok(&["accept", &landed]);
    let closed = p.attach("wt-closed", &[]);
    p.ok(&["close", &closed, "--reason", "unfinished"]);
    let refresh = p.work("wt-refresh", &[("src/api.rs", CALLER)]);
    let stop = p.work("wt-stop", &[("src/dup.rs", DUP)]);
    let ready = p.work("wt-ready", &[("src/new.rs", "pub fn new() {}\n")]);
    let running = p.attach("wt-running", &[("src/run.rs", "pub fn run() {}\n")]);
    let session = p.claude_worktree("x", false);
    let needs_you = p.session_start(&session);
    p.notification(
        &session,
        "permission_prompt",
        "Claude needs your permission",
    );
    let subagent = p.claude_worktree(&format!("agent-{SUBAGENT}"), true);
    let subagent = p.subagent_start(&subagent);
    // The source moves under them: `refresh` uses what changed, and `stop`
    // is in it already.
    let changed = AUTH.replacen(SIGNATURE.0, SIGNATURE.1, 1);
    fs::write(p.root.join("src/auth.rs"), changed).unwrap();
    fs::write(p.root.join("src/dup.rs"), DUP).unwrap();
    p.until(&refresh, "REFRESH", |run| verdict(run) == "refresh");
    p.until(&stop, "STOP", |run| verdict(run) == "stop");
    p.until(&ready, "CONTINUE on the moved source", |run| {
        verdict(run) == "continue" && run["coherence"]["validity"]["world_changed"] == true
    });
    p.until(&needs_you, "the mark", |run| {
        run["attachment"]["needs_you"]["kind"] == "permission"
    });
    // The owner writes `metadata.json` a moment before `events.jsonl`.
    until("the mark's event", || {
        let events = fs::read_to_string(p.state.join("runs").join(&needs_you).join("events.jsonl"));
        events.is_ok_and(|events| events.contains("runtime.needs_you"))
    });
    p.quiet();
    let states = States {
        running,
        needs_you,
        ready,
        refresh,
        stop,
        landed,
        closed,
        subagent,
    };
    (p, states)
}

/// A project with no Work, for the protocol alone.
fn server() -> (Project, Mcp) {
    let p = Project::new(&[]);
    let mcp = Mcp::start(&p, &p.root, &[]);
    (p, mcp)
}

#[test]
fn claude_code_asks_server_discover_first_then_initializes() {
    let (_p, mut mcp) = server();
    // As Claude Code 2.1.296 sent them, in order.
    mcp.send(
        r#"{"jsonrpc":"2.0","id":"server-discover-probe-1","method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"claude-code","title":"Claude Code","version":"2.1.296","description":"Anthropic's agentic coding tool","websiteUrl":"https://claude.com/claude-code"},"io.modelcontextprotocol/clientCapabilities":{"roots":{"listChanged":true},"elicitation":{"form":{},"url":{}}}}}}"#,
    );
    let discover = mcp.answer(&json!("server-discover-probe-1")).1;
    assert_eq!(error_code(&discover), -32601, "{discover}");
    mcp.send(
        r#"{"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"roots":{"listChanged":true},"elicitation":{"form":{},"url":{}}},"clientInfo":{"name":"claude-code","title":"Claude Code","version":"2.1.296","description":"Anthropic's agentic coding tool","websiteUrl":"https://claude.com/claude-code"}},"jsonrpc":"2.0","id":0}"#,
    );
    let initialized = mcp.answer(&json!(0)).1;
    assert_eq!(
        initialized["result"],
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "dispatch", "version": env!("CARGO_PKG_VERSION")},
        })
    );
    mcp.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    mcp.send(r#"{"method":"tools/list","jsonrpc":"2.0","id":1}"#);
    let tools = mcp.answer(&json!(1)).1;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, TOOLS, "exactly these tools, none that lands");
    for tool in tools["result"]["tools"].as_array().unwrap() {
        assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false, "{tool}");
    }
    mcp.close();
}

#[test]
fn codex_and_cursor_initialize_with_their_own_versions() {
    let (_p, mut mcp) = server();
    // Codex 0.170.0: 2025-06-18, a progress token on tools/list, and its own
    // `_meta` keys on a call, its workspace path replaced.
    mcp.send(
        r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{"experimental":{"codex/auth-change":{}},"elicitation":{"form":{},"url":{}}},"clientInfo":{"name":"codex-mcp-client","title":"Codex","version":"0.170.0"}}}"#,
    );
    assert_eq!(
        mcp.answer(&json!(0)).1["result"]["protocolVersion"],
        "2025-06-18"
    );
    mcp.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    mcp.send(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"progressToken":0}}}"#,
    );
    assert_eq!(
        mcp.answer(&json!(1)).1["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        TOOLS.len()
    );
    mcp.send(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"_meta":{"callId":"exec-177f9dd3-4fcf-4ad8-84eb-36724cdc2a43","x-codex-turn-metadata":{"session_id":"01a1f044-0a99-7f43-a994-73a6fa154f81","thread_id":"01a1f044-0a99-7f43-a994-73a6fa154f81","reasoning_effort":"ultra","turn_id":"01a1f044-0c1b-7d02-af3e-13787addaf43","model":"gpt-6.2-astra","thread_source":"user","turn_trigger":"exec","sandbox":"seatbelt","sandbox_mode":"workspace-write","auto_review_enabled":false,"node_repl_auto_review_required":true,"node_repl_disabled":false,"workspaces":{"/home/u/project":{"has_changes":false}},"turn_started_at_unix_ms":1795032353831,"codex_version":"0.170.0"},"threadId":"01a1f044-0a99-7f43-a994-73a6fa154f81","sessionId":"01a1f044-0a99-7f43-a994-73a6fa154f81","windowId":"01a1f044-0a99-7f43-a994-73a6fa154f81:0","itemId":"ctc_0bd745e451b4c175016afe052b17a487d1afee19d33bdb9e6f","progressToken":1},"name":"work","arguments":{}}}"#,
    );
    let work = result(&mcp.answer(&json!(2)).1);
    assert_eq!(work["work"], json!([]));
    assert_eq!(work["watched"], false);
    mcp.close();

    // Cursor Agent: 2025-11-25, and `params: {}` on tools/list.
    let (_p, mut mcp) = server();
    mcp.send(
        r#"{"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{"form":{}},"extensions":{"io.modelcontextprotocol/ui":{"mimeTypes":["text/html;profile=mcp-app"]}}},"clientInfo":{"name":"Cursor","version":"1.0.0"}},"jsonrpc":"2.0","id":0}"#,
    );
    assert_eq!(
        mcp.answer(&json!(0)).1["result"]["protocolVersion"],
        "2025-11-25"
    );
    mcp.send(r#"{"method":"notifications/initialized","jsonrpc":"2.0"}"#);
    mcp.send(r#"{"method":"tools/list","params":{},"jsonrpc":"2.0","id":1}"#);
    assert_eq!(
        mcp.answer(&json!(1)).1["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        TOOLS.len()
    );
    // A version not answered as asked gets the newest.
    mcp.send(r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2099-01-01"}}"#);
    assert_eq!(
        mcp.answer(&json!(2)).1["result"]["protocolVersion"],
        "2025-11-25"
    );
    for version in ["2025-03-26", "2024-11-05"] {
        let id = mcp.request("initialize", json!({"protocolVersion": version}));
        assert_eq!(mcp.answer(&id).1["result"]["protocolVersion"], version);
    }
    mcp.close();
}

#[test]
fn malformed_messages_get_json_rpc_errors_and_notifications_get_nothing() {
    let (_p, mut mcp) = server();
    mcp.initialize();
    let id = mcp.request("ping", json!({}));
    assert_eq!(mcp.answer(&id).1["result"], json!({}));
    let id = mcp.request("resources/list", json!({}));
    assert_eq!(error_code(&mcp.answer(&id).1), -32601);

    // Notifications, known or not, and a cancel of nothing in flight: no
    // answer. The ping after them is answered first.
    mcp.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    mcp.send(r#"{"jsonrpc":"2.0","method":"notifications/roots/list_changed"}"#);
    mcp.send(
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":12345}}"#,
    );
    let id = mcp.request("ping", json!({}));
    assert_eq!(mcp.next(Duration::from_secs(60)).unwrap().1["id"], id);

    // Malformed, each answered with its id when it has a valid one, else
    // with null: the next message is that error.
    for (line, id, code) in [
        ("{\"jsonrpc\":\"2.0\",\"id\":", Value::Null, -32700),
        ("not json", Value::Null, -32700),
        (
            "[{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}]",
            Value::Null,
            -32600,
        ),
        ("[]", Value::Null, -32600),
        ("null", Value::Null, -32600),
        ("\"ping\"", Value::Null, -32600),
        ("{\"jsonrpc\":\"2.0\",\"method\":5}", Value::Null, -32600),
        (
            "{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"ping\"}",
            Value::Null,
            -32600,
        ),
        (
            "{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"ping\"}",
            Value::Null,
            -32600,
        ),
        ("{\"jsonrpc\":\"2.0\",\"id\":8}", json!(8), -32600),
        (
            "{\"jsonrpc\":\"1.0\",\"id\":9,\"method\":\"ping\"}",
            json!(9),
            -32600,
        ),
        ("{\"id\":\"10\",\"method\":\"ping\"}", json!("10"), -32600),
    ] {
        mcp.asked.insert(id.to_string());
        mcp.send_bytes(line.as_bytes());
        let answer = mcp.next(Duration::from_secs(60)).unwrap().1;
        assert_eq!(answer["id"], id, "{line}: {answer}");
        assert_eq!(error_code(&answer), code, "{line}: {answer}");
    }
    // Not UTF-8.
    mcp.asked.insert("null".into());
    mcp.send_bytes(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"\xff\"}");
    assert_eq!(
        error_code(&mcp.next(Duration::from_secs(60)).unwrap().1),
        -32700
    );

    // Calls a server cannot make sense of.
    for params in [
        json!({"name": "land", "arguments": {}}),
        json!({"name": "accept"}),
        json!({"arguments": {}}),
        json!({"name": "work", "arguments": "{}"}),
        json!({"name": "work", "arguments": []}),
        json!({"name": "work", "arguments": null}),
        json!({"name": 5}),
        json!(null),
    ] {
        let id = mcp.request("tools/call", params.clone());
        assert_eq!(error_code(&mcp.answer(&id).1), -32602, "{params}");
    }
    mcp.close();
}

#[test]
fn a_wait_holds_up_no_other_call_and_a_cancelled_wait_answers_nothing() {
    let p = Project::new(&[]);
    let id = p.attach("wt-a", &[]);
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    mcp.initialize();
    let long = mcp.request(
        "tools/call",
        json!({"name": "wait", "arguments": {"timeout_s": 600}}),
    );
    let sent = Instant::now();
    let ping = mcp.request("ping", json!({}));
    let work = mcp.request("tools/call", json!({"name": "work", "arguments": {}}));
    let status = mcp.request(
        "tools/call",
        json!({"name": "status", "arguments": {"id": id}}),
    );
    for call in [ping, work, status] {
        let (at, answer) = mcp.answer(&call);
        assert!(answer.get("result").is_some(), "{answer}");
        assert!(at - sent < Duration::from_secs(60), "{answer}");
    }
    // Cancelled as Cursor cancels a call it gave up on.
    mcp.send(&format!(
        r#"{{"jsonrpc":"2.0","method":"notifications/cancelled","params":{{"requestId":{long},"reason":"McpError: MCP error -32001: Request timed out"}}}}"#
    ));
    // A wait cancelled as it begins would answer at 10 s, before one of 11 s
    // does: the 11 s answer comes first and alone. The cancel has 10 s to
    // arrive.
    let short = mcp.request(
        "tools/call",
        json!({"name": "wait", "arguments": {"timeout_s": 10}}),
    );
    mcp.send(&format!(
        r#"{{"jsonrpc":"2.0","method":"notifications/cancelled","params":{{"requestId":{short}}}}}"#
    ));
    let after = mcp.request(
        "tools/call",
        json!({"name": "wait", "arguments": {"timeout_s": 11}}),
    );
    let (_, answer) = mcp.next(Duration::from_secs(300)).expect("the 11 s wait");
    assert_eq!(answer["id"], after, "{answer}");
    assert_eq!(result(&answer)["woke"], "timeout");
    // Stdin closes with a wait in flight: it answers nothing either.
    mcp.request(
        "tools/call",
        json!({"name": "wait", "arguments": {"timeout_s": 600}}),
    );
    mcp.close();
}

#[test]
fn a_server_in_a_subdirectory_or_a_worktree_serves_the_project() {
    let p = Project::new(&[]);
    let id = p.attach("wt-a", &[("src/a.rs", "pub fn a() {}\n")]);
    let worktree = p.temp.path().join("wt-a");
    for cwd in [p.root.join("src"), worktree] {
        let mut mcp = Mcp::start(&p, &cwd, &[]);
        let work = mcp.tool("work", json!({}));
        assert_eq!(work["work"][0]["run_id"], id, "{}: {work}", cwd.display());
        assert_eq!(work["work"].as_array().unwrap().len(), 1);
        mcp.close();
    }
    // `--root` names the project wherever the server runs, by the same rule:
    // a subdirectory or a worktree of it is the project.
    let elsewhere = tempfile::tempdir().unwrap();
    let worktree = p.temp.path().join("wt-a");
    for root in [p.root.clone(), p.root.join("src"), worktree] {
        let mut mcp = Mcp::start(&p, elsewhere.path(), &["--root", root.to_str().unwrap()]);
        let work = mcp.tool("work", json!({}));
        assert_eq!(work["work"][0]["run_id"], id, "{}: {work}", root.display());
        mcp.close();
    }
}

/// `printf '<lines>' | dispatch mcp`: stdin ends at once, and every answer
/// is written before the server exits, the tool call's too.
#[test]
fn a_one_shot_pipe_gets_every_answer() {
    let p = Project::new(&[]);
    let id = p.attach("wt-a", &[]);
    let mut child = Command::new(assert_cmd::cargo_bin!("dispatch"))
        .arg("--state-dir")
        .arg(&p.state)
        .arg("mcp")
        .current_dir(&p.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let lines = [
        r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"work","arguments":{}}}"#,
    ];
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(lines.join("\n").as_bytes()).unwrap();
    stdin.write_all(b"\n").unwrap();
    drop(stdin);
    let (sender, exited) = mpsc::channel();
    std::thread::spawn(move || sender.send(child.wait_with_output().unwrap()));
    let output = exited
        .recv_timeout(Duration::from_secs(60))
        .expect("dispatch mcp exits once stdin ends");
    assert!(output.status.success(), "{}", text(&output));
    let answers: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let ids: Vec<&Value> = answers.iter().map(|answer| &answer["id"]).collect();
    assert_eq!(ids, [&json!(0), &json!(1), &json!(2)], "{answers:?}");
    assert_eq!(result(&answers[2])["work"][0]["run_id"], id);
}

/// Every tool with valid, missing, wrong-typed and unknown arguments,
/// unknown and ambiguous ids and very long strings, over Work in every
/// state: stdout is the protocol only (`Mcp::next`), and nothing a landing,
/// review or close would change has changed.
#[test]
fn no_tool_can_land_and_stdout_is_the_protocol_only() {
    let (p, w) = every_state();
    let before = Snapshot::of(&p);
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    mcp.initialize();

    let work = mcp.tool("work", json!({}));
    assert_eq!(work["watched"], true, "{work}");
    let rows = work["work"].as_array().unwrap();
    let row = |id: &str| {
        rows.iter()
            .find(|row| row["run_id"] == id)
            .unwrap_or_else(|| panic!("{id} in {work}"))
    };
    assert_eq!(row(&w.running)["state"], "working");
    assert_eq!(row(&w.needs_you)["needs_you"]["kind"], "permission");
    assert_eq!(row(&w.ready)["ready_for_review"], true);
    assert_eq!(row(&w.refresh)["verdict"], "refresh");
    assert_eq!(row(&w.stop)["verdict"], "stop");
    assert_eq!(row(&w.landed)["state"], "applied");
    assert_eq!(row(&w.closed)["state"], "closed");
    assert_eq!(
        row(&w.subagent)["subagent"],
        json!({"agent_id": SUBAGENT, "agent_type": "general-purpose"})
    );
    assert_eq!(row(&w.ready)["files_changed"], 1);
    // Running Work has edits, but no Δ is recorded until it finishes.
    assert_eq!(row(&w.running)["files_changed"], Value::Null);
    assert_eq!(row(&w.ready)["task"], "the task of wt-ready");
    assert_eq!(row(&w.running)["subagent"], Value::Null);

    let proposal = mcp.tool("propose_landing", json!({}));
    let members: Vec<&Value> = proposal["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|member| &member["id"])
        .collect();
    assert_eq!(members, [&json!(w.ready)], "{proposal}");
    let batch = proposal["batch"].as_str().unwrap();
    assert_eq!(
        proposal["accept"],
        format!("dispatch accept --expect {batch}")
    );
    for left in proposal["left_out"].as_array().unwrap() {
        assert_eq!(left["accept"], Value::Null, "{proposal}");
    }

    // Valid calls, every wait at once.
    let long = "A".repeat(100_000);
    let mut waits = vec![mcp.request(
        "tools/call",
        json!({"name": "wait", "arguments": {"timeout_s": 1}}),
    )];
    for id in w.all() {
        assert_eq!(mcp.tool("status", json!({"id": id}))["run_id"], id);
        let prefix = id[..12].to_ascii_lowercase();
        assert_eq!(mcp.tool("status", json!({"id": prefix}))["run_id"], id);
        let arguments = json!({"ids": [id], "timeout_s": 1});
        waits.push(mcp.request(
            "tools/call",
            json!({"name": "wait", "arguments": arguments}),
        ));
    }
    let arguments = json!({"ids": w.all(), "timeout_s": 1.5});
    waits.push(mcp.request(
        "tools/call",
        json!({"name": "wait", "arguments": arguments}),
    ));
    for wait in waits {
        assert_eq!(
            result(&mcp.answer(&wait).1),
            json!({"woke": "timeout", "work": []})
        );
    }

    // Refused, each with one sentence.
    let refused = [
        ("work", json!({"all": true})),
        (
            "work",
            Value::Object([(long.clone(), json!(1))].into_iter().collect()),
        ),
        ("propose_landing", json!({"land": true})),
        ("propose_landing", json!({"expect": "0123456789ab"})),
        ("status", json!({})),
        ("status", json!({"id": 5})),
        ("status", json!({"id": null})),
        ("status", json!({"id": [w.ready.clone()]})),
        ("status", json!({"id": ""})),
        ("status", json!({"id": "0"})),
        ("status", json!({"id": "ZZZZZZZZ"})),
        ("status", json!({"id": "../../etc"})),
        ("status", json!({"id": long.clone()})),
        ("status", json!({"id": w.ready.clone(), "accept": true})),
        ("wait", json!({"ids": "all"})),
        ("wait", json!({"ids": []})),
        ("wait", json!({"ids": [1]})),
        ("wait", json!({"ids": ["ZZZZZZZZ"], "timeout_s": 1})),
        ("wait", json!({"ids": [long.clone()], "timeout_s": 1})),
        ("wait", json!({"ids": ["0"], "timeout_s": 1})),
        ("wait", json!({"timeout_s": "5"})),
        ("wait", json!({"timeout_s": 0})),
        ("wait", json!({"timeout_s": 0.5})),
        ("wait", json!({"timeout_s": 901})),
        ("wait", json!({"timeout_s": -1})),
        ("wait", json!({"timeout_s": 1, "then": "accept"})),
        // There is no launch consent here.
        ("start", json!({})),
        ("start", json!({"agent": "claude"})),
        ("start", json!({"agent": "claude", "task": ""})),
        ("start", json!({"agent": 5, "task": "t"})),
        ("start", json!({"agent": "claude", "task": ["t"]})),
        ("start", json!({"agent": "claude", "task": "t"})),
        (
            "start",
            json!({"agent": "claude", "task": "t", "accept": true}),
        ),
        (
            "start",
            json!({"agent": long.clone(), "task": long.clone()}),
        ),
        ("send", json!({})),
        ("send", json!({"id": w.needs_you.clone()})),
        ("send", json!({"text": "x"})),
        ("send", json!({"id": 5, "text": "x"})),
        ("send", json!({"id": w.needs_you.clone(), "text": 5})),
        ("send", json!({"id": w.needs_you.clone(), "text": null})),
        ("send", json!({"id": w.needs_you.clone(), "text": ""})),
        (
            "send",
            json!({"id": w.needs_you.clone(), "text": long.clone()}),
        ),
        (
            "send",
            json!({"id": w.needs_you.clone(), "text": "x", "land": true}),
        ),
        ("send", json!({"id": "ZZZZZZZZ", "text": "x"})),
        ("send", json!({"id": "0", "text": "x"})),
        ("send", json!({"id": long.clone(), "text": "x"})),
    ];
    for (name, arguments) in refused {
        let words = mcp.refused(name, arguments.clone());
        assert!(words.len() < 200, "{name} {arguments}: {words}");
    }
    let unknown = mcp.refused("status", json!({"id": "ZZZZZZZZ"}));
    assert_eq!(
        unknown,
        "No Work \"ZZZZZZZZ\" in this project; work lists its Work."
    );
    let ambiguous = mcp.refused("wait", json!({"ids": ["0"], "timeout_s": 1}));
    assert!(
        ambiguous.starts_with("More than one Work here"),
        "{ambiguous}"
    );

    // Not tools at all.
    for name in [
        "accept", "apply", "reject", "close", "refresh", "land", &long,
    ] {
        let id = mcp.request("tools/call", json!({"name": name, "arguments": {}}));
        assert_eq!(
            error_code(&mcp.answer(&id).1),
            -32602,
            "{}",
            &name[..name.len().min(20)]
        );
    }
    let id = mcp.request(&long, json!({}));
    assert_eq!(error_code(&mcp.answer(&id).1), -32601);
    mcp.asked.insert("null".into());
    mcp.send_bytes(long.as_bytes());
    assert_eq!(
        error_code(&mcp.next(Duration::from_secs(60)).unwrap().1),
        -32700
    );
    assert_eq!(
        Snapshot::of(&p),
        before,
        "a tool changed what only a person may"
    );

    // No Work here but the session and the subagent has a hook that
    // delivers a message.
    for id in w.all() {
        if id == w.needs_you || id == w.subagent {
            sent(&mut mcp, id, "x", "next_tool_call");
        } else {
            let words = mcp.refused("send", json!({"id": id, "text": "x"}));
            assert!(
                words.starts_with("No hook carries messages to wt-"),
                "{words}"
            );
        }
    }
    mcp.close();
    // A message is written, and nothing else.
    let mut after = Snapshot::of(&p);
    let mut written = after.events.clone();
    written.retain(|event| event.2 == "message.written");
    after.events.retain(|event| event.2 != "message.written");
    let ids: HashSet<&str> = written.iter().map(|event| event.1.as_str()).collect();
    assert_eq!(ids, HashSet::from([&w.needs_you[..], &w.subagent[..]]));
    for id in ids {
        let revision = &before.runs[id]["state_revision"];
        after.runs.get_mut(id).unwrap()["state_revision"] = revision.clone();
    }
    assert_eq!(after, before, "a tool changed what only a person may");
    assert_eq!(p.of("batch.proposed"), 0);
    assert!(!p.state.join("started").exists(), "something was launched");
}

#[test]
fn work_rows_are_the_rows_watch_json_shows() {
    let (p, w) = every_state();
    let mut watch = p
        .command(&["watch", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = watch.stdout.take().unwrap();
    let (sender, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            let _ = sender.send(line);
        }
    });
    let mut shown = BTreeMap::new();
    while shown.len() < w.all().len() {
        let line = lines
            .recv_timeout(Duration::from_secs(300))
            .expect("watch --json shows every row");
        let line: Value = serde_json::from_str(&line).unwrap();
        if line["type"] == "work" {
            shown.insert(line["run_id"].as_str().unwrap().to_owned(), line);
        }
    }
    let _ = watch.kill();
    let _ = watch.wait();

    let mut mcp = Mcp::start(&p, &p.root, &[]);
    let work = mcp.tool("work", json!({}));
    let rows: BTreeMap<String, Value> = work["work"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| (row["run_id"].as_str().unwrap().to_owned(), row.clone()))
        .collect();
    assert_eq!(rows, shown);
    // The keys watch --json has always shown, then what an orchestrator
    // needs to act.
    let keys: Vec<&String> = rows[&w.needs_you].as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "agent",
            "applied_by",
            "asked_to_catch_up",
            "files_changed",
            "interactions",
            "needs_you",
            "origin",
            "overridden",
            "ready_for_review",
            "reason",
            "review",
            "run_id",
            "s0",
            "state",
            "subagent",
            "task",
            "type",
            "verdict",
            "verification",
            "workspace",
        ]
    );
    mcp.close();
}

#[test]
fn status_is_what_status_json_prints() {
    let (p, w) = every_state();
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    for id in w.all() {
        let mut printed: Value = serde_json::from_str(&p.ok(&["status", id, "--json"])).unwrap();
        let mut status = mcp.tool("status", json!({"id": id}));
        // Work still in progress has run for as long as it has when asked.
        if printed["outcome"]["lifecycle"] == "working" {
            for value in [&mut printed, &mut status] {
                assert!(value["elapsed_ms"].as_u64().is_some(), "{value}");
                value["elapsed_ms"] = Value::Null;
            }
        }
        assert_eq!(status, printed, "{id}");
    }
    mcp.close();
}

#[test]
fn propose_landing_is_the_batch_accept_proposes_and_records_nothing() {
    let p = Project::new(&["true"]);
    let user = p.work("wt-user", &[("src/api.rs", CALLER)]);
    let changer = p.work(
        "wt-changer",
        &[("src/auth.rs", &AUTH.replacen(SIGNATURE.0, SIGNATURE.1, 1))],
    );
    let other = p.work(
        "wt-other",
        &[("src/cli.rs", "pub fn main() {\n    run();\n}\n")],
    );
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    let proposal = mcp.tool("propose_landing", json!({}));
    assert_eq!(
        p.of("batch.proposed"),
        0,
        "a proposal is recorded only by accept"
    );

    // `dispatch accept` without a terminal proposes and decides nothing.
    let output = p.dispatch(&["accept"]);
    assert!(!output.status.success(), "{}", text(&output));
    let printed = String::from_utf8_lossy(&output.stdout).into_owned();
    let digest = String::from_utf8_lossy(&output.stderr)
        .split("dispatch accept --expect ")
        .nth(1)
        .expect("the command to accept the batch")
        .trim()
        .to_owned();
    assert_eq!(proposal["batch"], digest, "{proposal}\n{printed}");
    assert_eq!(
        proposal["accept"],
        format!("dispatch accept --expect {digest}")
    );
    // Members in order, each with the why `accept` prints under it.
    let members = proposal["members"].as_array().unwrap();
    assert_eq!(members.len(), 2, "{proposal}");
    for (position, member) in members.iter().enumerate() {
        let id = member["id"].as_str().unwrap();
        let line = format!("  {}. {id}  Δ ", position + 1);
        let at = printed
            .find(&line)
            .unwrap_or_else(|| panic!("{line} in {printed}"));
        let why = printed[at..].lines().nth(1).unwrap();
        assert_eq!(why, format!("       {}", member["why"].as_str().unwrap()));
        assert_eq!(member["task"], p.run(id)["task"]);
    }
    assert_eq!(members[0]["id"], changer, "a change before what uses it");
    assert_eq!(members[1]["id"], other);
    let left_out = &proposal["left_out"].as_array().unwrap()[..];
    let [left] = left_out else {
        panic!("{proposal}")
    };
    assert_eq!(left["id"], user);
    assert_eq!(left["decision"], "refresh");
    assert_eq!(left["accept"], Value::Null);
    let reason = left["reasons"][0].as_str().unwrap();
    assert!(
        printed.contains(&format!("       REFRESH: {reason}")),
        "{reason}\n{printed}"
    );
    assert!(
        proposal["note"]
            .as_str()
            .unwrap()
            .contains("only the person lands Work")
    );

    // The person types it, and it lands them.
    p.ok(&["accept", "--expect", &digest]);
    for id in [&changer, &other] {
        assert_eq!(p.run(id)["outcome"]["application"], "applied", "{id}");
    }
    assert_eq!(p.run(&user)["outcome"]["application"], "not_applied");
    mcp.close();
}

#[test]
fn propose_landing_with_one_result_is_its_accept() {
    let p = Project::new(&[]);
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    let none = mcp.tool("propose_landing", json!({}));
    assert_eq!(
        none,
        json!({"batch": null, "members": [], "left_out": [], "accept": null,
               "note": "No result waits for review here."})
    );
    let id = p.work("wt-a", &[("src/a.rs", "pub fn a() {}\n")]);
    p.attach("wt-b", &[]);
    let one = mcp.tool("propose_landing", json!({}));
    assert_eq!(one["batch"], Value::Null);
    assert_eq!(one["members"][0]["id"], id, "{one}");
    assert_eq!(one["left_out"], json!([]));
    assert_eq!(one["accept"], format!("dispatch accept {id}"));
    mcp.close();
    assert_eq!(p.of("batch.proposed"), 0);
    p.ok(&["accept", &id]);
    assert_eq!(p.run(&id)["outcome"]["application"], "applied");
}

/// The source moves under the one result waiting: REFRESH, so it is no
/// member and no command names it.
#[test]
fn propose_landing_names_no_command_for_a_lone_stale_result() {
    let p = Project::new(&[]);
    let user = p.work("wt-user", &[("src/api.rs", CALLER)]);
    let changed = AUTH.replacen(SIGNATURE.0, SIGNATURE.1, 1);
    fs::write(p.root.join("src/auth.rs"), changed).unwrap();
    git(&p.root, &["commit", "--quiet", "-am", "a new signature"]);
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    let proposal = mcp.tool("propose_landing", json!({}));
    assert_eq!(proposal["members"], json!([]), "{proposal}");
    assert_eq!(proposal["accept"], Value::Null, "{proposal}");
    let [left] = &proposal["left_out"].as_array().unwrap()[..] else {
        panic!("{proposal}")
    };
    assert_eq!(left["id"], user);
    assert_eq!(left["decision"], "refresh");
    assert_eq!(left["accept"], Value::Null);
    let reasons = left["reasons"].as_array().unwrap();
    assert!(
        !reasons.is_empty() && reasons.iter().all(Value::is_string),
        "{left}"
    );
    assert_eq!(
        proposal["note"],
        "Nothing here can land now; left_out says why."
    );
    mcp.close();
    // The command it no longer names is refused.
    let output = p.dispatch(&["accept", &user]);
    assert!(!output.status.success(), "{}", text(&output));
}

/// Two results wait and none can land with another: the one whose own
/// checks failed, which is stale too, and a CONTINUE result, which lands
/// alone, by the command only it is given.
#[test]
fn propose_landing_gives_its_own_accept_to_a_result_that_lands_alone() {
    let p = Project::new(&["test ! -f bad"]);
    let ready = p.work("wt-ready", &[("src/new.rs", "pub fn new() {}\n")]);
    let stale = p.work("wt-stale", &[("src/api.rs", CALLER), ("bad", "")]);
    assert_eq!(p.run(&stale)["outcome"]["verification"], "failed");
    let changed = AUTH.replacen(SIGNATURE.0, SIGNATURE.1, 1);
    fs::write(p.root.join("src/auth.rs"), changed).unwrap();
    git(&p.root, &["commit", "--quiet", "-am", "a new signature"]);
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    let proposal = mcp.tool("propose_landing", json!({}));
    assert_eq!(proposal["batch"], Value::Null, "{proposal}");
    assert_eq!(proposal["members"], json!([]), "{proposal}");
    assert_eq!(proposal["accept"], Value::Null, "{proposal}");
    let left_out = proposal["left_out"].as_array().unwrap();
    let left = |id: &str| {
        left_out
            .iter()
            .find(|left| left["id"] == id)
            .unwrap_or_else(|| panic!("{id} in {proposal}"))
    };
    assert_eq!(left_out.len(), 2, "{proposal}");
    assert_eq!(left(&ready)["decision"], "continue");
    assert_eq!(left(&ready)["accept"], format!("dispatch accept {ready}"));
    assert_eq!(left(&stale)["decision"], "refresh");
    assert_eq!(left(&stale)["accept"], Value::Null);
    let note = proposal["note"].as_str().unwrap();
    assert!(
        note.contains("only the person lands Work") && note.contains("that result alone"),
        "{note}"
    );
    mcp.close();
    p.ok(&["accept", &ready]);
    assert_eq!(p.run(&ready)["outcome"]["application"], "applied");
}

/// The owner's checks hold the source: `propose_landing` answers at once
/// that it is busy, where `accept` waits, and it holds no lock after.
#[test]
fn propose_landing_waits_for_no_lock() {
    let p = Project::new(&["true"]);
    let a = p.work("wt-a", &[("src/new.rs", "pub fn new() {}\n")]);
    let b = p.work(
        "wt-b",
        &[("src/cli.rs", "pub fn main() {\n    run();\n}\n")],
    );
    let run_lock = |id: &str| p.state.join("runs").join(id).join(".operation.lock");
    let busy = "The source is busy with checks or a landing; call again shortly.";
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    for held in [p.lock("source"), run_lock(&b)] {
        let holder = hold(&held).expect("free");
        let id = mcp.request(
            "tools/call",
            json!({"name": "propose_landing", "arguments": {}}),
        );
        let (_, answer) = mcp
            .next(Duration::from_secs(60))
            .expect("an answer while the lock is held");
        assert_eq!(answer["id"], id);
        assert_eq!(refusal(&answer), busy, "{}", held.display());
        drop(holder);
        for lock in [p.lock("source"), run_lock(&a), run_lock(&b)] {
            assert!(hold(&lock).is_some(), "{} is still held", lock.display());
        }
    }
    let proposal = mcp.tool("propose_landing", json!({}));
    assert_eq!(
        proposal["members"].as_array().unwrap().len(),
        2,
        "{proposal}"
    );
    mcp.close();
}

/// The owner finishes a session's Work by consent once it ended: `wait`
/// answers that, long before its timeout. How soon is measured, not
/// asserted: load decides it.
#[test]
fn wait_answers_a_finish_the_owner_records() {
    let p = Project::new(&["true"]);
    let state = dispatch::state::State::discover(Some(p.state.clone())).unwrap();
    dispatch::consent::grant(&state, &p.root).unwrap();
    let agent = Agent::start();
    let worktree = p.claude_worktree("x", true);
    let locked = p.gitdir(&worktree).join("locked");
    fs::write(&locked, agent.lock("x")).unwrap();
    p.start();
    let id = p.session_start(&worktree);
    p.until(&id, "the agent seen running", |run| {
        run["attachment"]["agent_process"]["pid"] == agent.0.id()
    });
    fs::write(
        worktree.join("src/cli.rs"),
        "pub fn main() {\n    run();\n}\n",
    )
    .unwrap();
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    assert_eq!(mcp.tool("work", json!({}))["work"][0]["state"], "working");
    let wait = mcp.request(
        "tools/call",
        json!({"name": "wait", "arguments": {"ids": [id], "timeout_s": 300}}),
    );
    // `/exit`: the lock goes, then `SessionEnd`, then the process.
    fs::remove_file(&locked).unwrap();
    p.session_end();
    drop(agent);
    let woke = result(&mcp.answer(&wait).1);
    assert_eq!(woke["woke"], "finished", "{woke}");
    assert_eq!(woke["work"][0]["run_id"], id);
    assert_eq!(woke["work"][0]["ready_for_review"], true);
    assert_eq!(p.run(&id)["outcome"]["lifecycle"], "finished");
    mcp.close();
}

#[test]
fn wait_answers_a_needs_you() {
    let p = Project::new(&[]);
    p.start();
    let worktree = p.claude_worktree("x", false);
    let id = p.session_start(&worktree);
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    assert_eq!(
        mcp.tool("work", json!({}))["work"][0]["needs_you"],
        Value::Null
    );
    let wait = mcp.request(
        "tools/call",
        json!({"name": "wait", "arguments": {"timeout_s": 300}}),
    );
    p.notification(
        &worktree,
        "permission_prompt",
        "Claude needs your permission",
    );
    let woke = result(&mcp.answer(&wait).1);
    assert_eq!(woke["woke"], "needs_you", "{woke}");
    assert_eq!(woke["work"][0]["run_id"], id);
    assert_eq!(
        woke["work"][0]["needs_you"]["message"],
        "Claude needs your permission"
    );
    mcp.close();
}

#[test]
fn wait_answers_a_verdict_change() {
    let p = Project::new(&[]);
    p.start();
    let id = p.work("wt-user", &[("src/api.rs", CALLER)]);
    p.until(&id, "its first verdict", |run| verdict(run) == "continue");
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    assert_eq!(
        mcp.tool("work", json!({}))["work"][0]["verdict"],
        "continue"
    );
    let wait = mcp.request(
        "tools/call",
        json!({"name": "wait", "arguments": {"ids": [&id[..10]], "timeout_s": 300}}),
    );
    let changed = AUTH.replacen(SIGNATURE.0, SIGNATURE.1, 1);
    fs::write(p.root.join("src/auth.rs"), changed).unwrap();
    let woke = result(&mcp.answer(&wait).1);
    assert_eq!(woke["woke"], "verdict", "{woke}");
    assert_eq!(woke["work"][0]["verdict"], "refresh");
    mcp.close();
}

/// Work finishes after `work` gave its row and before the `wait`: the wait
/// answers at once.
#[test]
fn a_wait_answers_at_once_for_a_change_made_before_it() {
    let p = Project::new(&[]);
    let id = p.attach("wt-a", &[("src/a.rs", "pub fn a() {}\n")]);
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    assert_eq!(mcp.tool("work", json!({}))["work"][0]["state"], "working");
    p.ok(&["finish", &id]);
    let woke = mcp.tool("wait", json!({"timeout_s": 60}));
    assert_eq!(woke["woke"], "finished", "{woke}");
    assert_eq!(woke["work"][0]["run_id"], id);
    mcp.close();
}

/// An orchestrator's loop: Work that finishes while it handles a wake is
/// the next wait's, and what one wait reported, no other reports again.
#[test]
fn a_change_between_two_waits_is_the_next_waits() {
    let p = Project::new(&[]);
    let a = p.attach("wt-a", &[("src/a.rs", "pub fn a() {}\n")]);
    let b = p.attach("wt-b", &[("src/b.rs", "pub fn b() {}\n")]);
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    assert_eq!(
        mcp.tool("work", json!({}))["work"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let ids = |woke: &Value| -> Vec<String> {
        let rows = woke["work"].as_array().unwrap();
        rows.iter()
            .map(|row| row["run_id"].as_str().unwrap().to_owned())
            .collect()
    };
    let wait = mcp.request(
        "tools/call",
        json!({"name": "wait", "arguments": {"timeout_s": 60}}),
    );
    p.ok(&["finish", &a]);
    let woke = result(&mcp.answer(&wait).1);
    assert_eq!((&woke["woke"], ids(&woke)), (&json!("finished"), vec![a]));
    p.ok(&["finish", &b]);
    let woke = mcp.tool("wait", json!({"timeout_s": 60}));
    assert_eq!((&woke["woke"], ids(&woke)), (&json!("finished"), vec![b]));
    let woke = mcp.tool("wait", json!({"timeout_s": 1}));
    assert_eq!(woke, json!({"woke": "timeout", "work": []}));
    mcp.close();
}

/// `dispatch run` makes its run's directory, builds the baseline, and only
/// then publishes `metadata.json`; nothing in `runs/` itself changes then.
/// A wait on any Work sees the run once it is published.
#[test]
fn a_wait_on_any_work_sees_a_run_published_in_a_directory_made_during_it() {
    let p = Project::new(&[]);
    // A finished result, taken out of `runs/` to be published again as a
    // native run publishes: its directory first, `metadata.json` and then
    // `events.jsonl` last.
    let id = p.work("wt-a", &[("src/a.rs", "pub fn a() {}\n")]);
    let run = p.state.join("runs").join(&id);
    let away = p.temp.path().join("away");
    fs::rename(&run, &away).unwrap();
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    assert_eq!(mcp.tool("work", json!({}))["work"], json!([]));
    let wait = mcp.request(
        "tools/call",
        json!({"name": "wait", "arguments": {"timeout_s": 60}}),
    );
    fs::create_dir(&run).unwrap();
    // Time for the wait to read `runs/` again and find the run unpublished:
    // were it too short, the code before this fix would pass too, but this
    // never fails for it.
    std::thread::sleep(Duration::from_secs(2));
    let mut entries: Vec<PathBuf> = fs::read_dir(&away)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    let last = ["metadata.json", "events.jsonl"];
    entries.sort_by_key(|path| last.iter().position(|name| path.ends_with(name)));
    for entry in entries {
        fs::rename(&entry, run.join(entry.file_name().unwrap())).unwrap();
    }
    let woke = result(&mcp.answer(&wait).1);
    assert_eq!(woke["woke"], "finished", "{woke}");
    assert_eq!(woke["work"][0]["run_id"], id);
    mcp.close();
}

/// A null argument is one left out, as models send them, and Work named
/// twice is one row.
#[test]
fn wait_takes_a_null_as_left_out_and_each_work_once() {
    let p = Project::new(&[]);
    let a = p.work("wt-a", &[("src/a.rs", "pub fn a() {}\n")]);
    let b = p.work("wt-b", &[("src/b.rs", "pub fn b() {}\n")]);
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    // No row was given yet: each finished Work wakes a wait at once.
    let twice = json!([a, a[..12].to_ascii_lowercase(), a]);
    let woke = mcp.tool("wait", json!({"ids": twice, "timeout_s": null}));
    assert_eq!(woke["woke"], "finished", "{woke}");
    assert_eq!(woke["work"].as_array().unwrap().len(), 1, "{woke}");
    assert_eq!(woke["work"][0]["run_id"], a);
    let woke = mcp.tool("wait", json!({"ids": null, "timeout_s": 30}));
    assert_eq!(woke["woke"], "finished", "{woke}");
    let rows = woke["work"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{woke}");
    assert_eq!(rows[0]["run_id"], b);
    mcp.close();
}

/// Each client's call shape with its own token, a number or a string, and
/// one without: progress about every 10 s for each token sent (`Mcp::next`
/// fails on any other), then `timeout` once `timeout_s` has passed, 5 s
/// after the one tick.
#[test]
fn wait_times_out_at_timeout_s_with_progress_for_each_token() {
    let (_p, mut mcp) = server();
    mcp.initialize();
    let calls = [
        (
            json!(2),
            json!({"claudecode/toolUseId": "toolu_01SD8TGhFWvZJH6iVHeMtHiW", "progressToken": 2}),
        ),
        (json!("cursor-0"), json!({"progressToken": "cursor-0"})),
        (Value::Null, json!({})),
    ];
    let sent = Instant::now();
    let waits: Vec<(Value, Value)> = calls
        .into_iter()
        .map(|(token, meta)| {
            let params = json!({"name": "wait", "arguments": {"timeout_s": 15}, "_meta": meta});
            (mcp.request("tools/call", params), token)
        })
        .collect();
    for (wait, token) in &waits {
        let (at, answer) = mcp.answer(wait);
        assert_eq!(result(&answer), json!({"woke": "timeout", "work": []}));
        let waited = at - sent;
        assert!(waited >= Duration::from_secs(15), "{waited:?}");
        assert!(waited < Duration::from_secs(60), "{waited:?}");
        if !token.is_null() {
            let progress = mcp.progress(token);
            let [notification] = &progress[..] else {
                panic!("one progress for {token}: {progress:?}");
            };
            let at = notification["params"]["progress"].as_u64().unwrap();
            assert!((10..15).contains(&at), "{notification}");
            assert_eq!(notification["params"]["total"], 15.0);
        }
    }
    mcp.close();
}

/// What `send` puts above a message from Claude Code (`initialize`).
const LABEL: &str = "Message from the orchestrating session (claude-code):";

/// A message to deliver as it is: blank and indented lines, Unicode, a line
/// that reads like a Dispatch note, and a final newline.
const MESSAGE: &str = "  Rebase on main first.\n\nThen rerun:\n\tcargo test\n\
Dispatch: Work 01AAAAAA (Claude Code) landed a change this Work relies on.\n\
Ünïcödé ✓ 日本語 🚀\n";

/// `send`'s answer, checked: Work `id`, delivered by `by`. The message's id.
fn sent(mcp: &mut Mcp, id: &str, text: &str, by: &str) -> String {
    let sent = mcp.tool("send", json!({"id": id, "text": text}));
    assert_eq!(sent["delivered_by"], by, "{sent}");
    assert!(sent["id"].as_str().is_some_and(|sent| sent.starts_with(id)));
    sent["message"].as_str().unwrap().to_owned()
}

/// What a Claude Code hook gave the agent, after `event`.
fn context(reply: &Value, event: &str) -> String {
    let output = &reply["hookSpecificOutput"];
    assert_eq!(output["hookEventName"], event, "{reply}");
    output["additionalContext"].as_str().unwrap().to_owned()
}

#[test]
fn send_is_delivered_verbatim_at_the_next_tool_call_and_recorded_as_a_message() {
    let mut p = Project::new(&[]);
    p.watched = hold(&p.lock("serve"));
    let worktree = p.claude_worktree("x", false);
    let id = p.session_start(&worktree);
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    mcp.initialize();
    let message = sent(&mut mcp, &id[..12], MESSAGE, "next_tool_call");
    // From a subdirectory, once.
    let reply = p.tool_use(&worktree.join("src"), false);
    assert_eq!(
        context(&reply, "PostToolUse"),
        format!("{LABEL}\n{MESSAGE}")
    );
    assert_eq!(p.tool_use(&worktree, false), Value::Null, "delivered once");

    p.watch();
    until("the delivery recorded", || {
        !p.payloads("message.delivered").is_empty()
    });
    let [written] = &p.payloads("message.written")[..] else {
        panic!("written once");
    };
    assert_eq!(
        [&written["message"], &written["text"], &written["client"]],
        [message.as_str(), MESSAGE, "claude-code"]
    );
    let [delivered] = &p.payloads("message.delivered")[..] else {
        panic!("delivered once");
    };
    assert_eq!(
        [&delivered["message"], &delivered["via"]],
        [message.as_str(), "PostToolUse"]
    );
    let events = p.events();
    assert!(
        events
            .iter()
            .filter(|event| event.2.starts_with("message."))
            .all(|event| event.1 == id)
    );
    assert!(
        events.iter().all(|event| !event.2.starts_with("note.")),
        "a message is never a note"
    );
    mcp.close();
}

#[test]
fn send_reaches_a_resumed_session_and_a_subagent_at_their_start_and_tool_calls() {
    let p = Project::new(&[]);
    let _watched = hold(&p.lock("serve"));
    let worktree = p.claude_worktree("x", false);
    let session = p.session_start(&worktree);
    let isolated = p.claude_worktree(&format!("agent-{SUBAGENT}"), true);
    let subagent = p.subagent_start(&isolated);
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    mcp.initialize();

    sent(&mut mcp, &session, "to the session", "next_tool_call");
    let reply = p.resume(&worktree);
    assert_eq!(
        context(&reply, "SessionStart"),
        format!("{LABEL}\nto the session")
    );

    // A subagent run again starts again: its start carries the message.
    sent(&mut mcp, &subagent, "to the subagent", "next_tool_call");
    let mut fields = p.head(PARENT, &isolated);
    fields.extend([
        ("agent_id", SUBAGENT.into()),
        ("agent_type", "general-purpose".into()),
        ("hook_event_name", "SubagentStart".into()),
    ]);
    assert_eq!(
        context(&p.reply(&fields), "SubagentStart"),
        format!("{LABEL}\nto the subagent")
    );
    // Its tool calls take its own messages, never its parent's.
    sent(&mut mcp, &subagent, "again", "next_tool_call");
    sent(&mut mcp, &session, "for the parent", "next_tool_call");
    assert_eq!(
        context(&p.tool_use(&isolated, true), "PostToolUse"),
        format!("{LABEL}\nagain")
    );
    assert_eq!(
        context(&p.tool_use(&worktree, false), "PostToolUse"),
        format!("{LABEL}\nfor the parent")
    );
    mcp.close();
}

/// An idle session whose agent was never asked to catch up still has its
/// waiter: the owner keeps a record for every live Work's workspace. The
/// waiter wakes the session with the message, and the owner records it.
#[test]
fn an_idle_session_never_asked_to_catch_up_is_woken_by_its_waiter_with_a_message() {
    let mut p = Project::new(&[]);
    p.start();
    let worktree = p.claude_worktree("x", false);
    let id = p.session_start(&worktree);
    let key = hex::encode(Sha256::digest(worktree.as_os_str().as_encoded_bytes()));
    let record = p.state.join(format!("catchup/work/{key}.json"));
    until("the owner's record", || record.exists());
    let record: Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
    assert_eq!(record["asked"], json!([]), "never asked");
    assert_eq!(p.hook(&p.stop(SESSION, &worktree)), "");
    p.until(&id, "the turn's end", |run| {
        run["attachment"]["needs_you"]["kind"] == "turn_ended"
    });
    p.quiet();
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    mcp.initialize();
    // Idle, and no waiter waits: the next tool call carries it.
    sent(&mut mcp, &id, "no waiter yet", "next_tool_call");
    assert_eq!(
        context(&p.tool_use(&worktree, false), "PostToolUse"),
        format!("{LABEL}\nno waiter yet")
    );

    let waiter = p.waiter(SESSION, &worktree);
    let message = sent(&mut mcp, &id, MESSAGE, "waiter");
    let woke = waiter.left();
    assert_eq!(woke.status.code(), Some(2), "{woke:?}");
    assert!(woke.stdout.is_empty());
    assert_eq!(
        String::from_utf8(woke.stderr).unwrap(),
        format!("{LABEL}\n{MESSAGE}\n")
    );
    assert_eq!(p.tool_use(&worktree, false), Value::Null, "delivered once");
    // The waiter left with it, though the mark still says the turn ended.
    sent(&mut mcp, &id, "after the waiter", "next_tool_call");

    p.watch();
    until("the deliveries recorded", || {
        p.payloads("message.delivered").len() == 2
    });
    let delivered = &p.payloads("message.delivered")[1];
    assert_eq!(
        [&delivered["message"], &delivered["via"]],
        [message.as_str(), "wake"]
    );
    mcp.close();
}

/// Hermes Work, found as `hermes chat --worktree` leaves it, hears a message
/// before its next turn.
#[test]
fn send_reaches_hermes_at_its_next_turn() {
    let p = Project::new(&[]);
    fs::write(p.root.join(".gitignore"), ".claude/\n.worktrees/\n").unwrap();
    git(&p.root, &["commit", "--quiet", "-am", "ignore worktrees"]);
    p.start();
    // Its process, then its worktree, locked by that process.
    let hermes = Agent::start();
    let path = ".worktrees/hermes-0a1b2c3d";
    git(
        &p.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "hermes/hermes-0a1b2c3d",
            path,
        ],
    );
    let lock = format!("hermes pid={}", hermes.0.id());
    git(&p.root, &["worktree", "lock", "--reason", &lock, path]);
    let worktree = fs::canonicalize(p.root.join(path)).unwrap();
    until("the owner finds it", || {
        p.ids()
            .iter()
            .any(|id| p.run(id)["attachment"]["workspace"] == worktree.to_str().unwrap())
    });
    let id = p.newest_in(&worktree);
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    mcp.initialize();
    sent(&mut mcp, &id, MESSAGE, "next_turn");

    // `pre_llm_call` as Hermes Agent sends it, from the main checkout.
    let extra = json!({"task_id": "20261104_074818_70b971", "user_message": "Go on.",
                       "conversation_history": [], "is_first_turn": false});
    let turn = payload(&[
        ("hook_event_name", "pre_llm_call".into()),
        ("tool_name", Value::Null),
        ("tool_input", Value::Null),
        ("session_id", "20261104_074818_70b971".into()),
        ("cwd", p.root.to_str().unwrap().into()),
        ("extra", extra),
    ]);
    let mut child = p
        .command(&["hook", "hermes"])
        .env("TERMINAL_CWD", &worktree)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(turn.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply, json!({"context": format!("{LABEL}\n{MESSAGE}")}));
    mcp.close();
}

#[test]
fn send_is_refused_where_no_hook_carries_a_message_or_while_the_work_is_busy() {
    let p = Project::new(&[]);
    let _watched = hold(&p.lock("serve"));
    let run = p.ok(&[
        "run",
        "--allow-unsafe-local",
        "--agent",
        "fake-good",
        "--json",
        "Create the fake artifact.",
    ]);
    let native: Value = serde_json::from_str(&run).unwrap();
    let native = native["run_id"].as_str().unwrap();
    let codex = p.attach("codex", &[]);
    let cursor = p.attach("cursor", &[]);
    // `dispatch attach --agent claude`: no hook reports its session.
    let unhooked = p.attach("claude", &[]);
    let worktree = p.claude_worktree("x", false);
    let live = p.session_start(&worktree);
    let mut mcp = Mcp::start(&p, &p.root, &[]);
    mcp.initialize();
    let refused =
        |mcp: &mut Mcp, id: &str, text: &str| mcp.refused("send", json!({"id": id, "text": text}));
    assert_eq!(
        refused(&mut mcp, native, "x"),
        "No hook carries messages to fake-good in a native run."
    );
    assert_eq!(
        refused(&mut mcp, &codex, "x"),
        "No hook carries messages to Codex."
    );
    assert_eq!(
        refused(&mut mcp, &cursor, "x"),
        "No hook carries messages to Cursor."
    );
    assert_eq!(
        refused(&mut mcp, &unhooked, "x"),
        format!("No Claude Code hook has reported a session for Work {unhooked}.")
    );
    assert_eq!(
        refused(&mut mcp, &live, ""),
        "text is empty; give the message to send."
    );
    assert_eq!(
        refused(&mut mcp, &live, " \n\t"),
        "text is blank; give the message to send."
    );
    let lock = p.state.join("runs").join(&live).join(".operation.lock");
    let busy = hold(&lock).unwrap();
    assert_eq!(
        refused(&mut mcp, &live, "x"),
        format!("Work {live} is busy; call again shortly.")
    );
    drop(busy);
    sent(&mut mcp, &live, "x", "next_tool_call");
    assert_eq!(p.of("message.written"), 1, "only the message sent");
    // Each connection's own client, `unknown` when it names none.
    let mut anonymous = Mcp::start(&p, &p.root, &[]);
    let initialize = anonymous.request(
        "initialize",
        json!({"protocolVersion": "2025-11-25", "capabilities": {}}),
    );
    anonymous.answer(&initialize);
    sent(&mut anonymous, &live, "y", "next_tool_call");
    assert_eq!(
        context(&p.tool_use(&worktree, false), "PostToolUse"),
        format!("{LABEL}\nx\nMessage from the orchestrating session (unknown):\ny")
    );
    let clients: Vec<Value> = p
        .payloads("message.written")
        .iter()
        .map(|written| written["client"].clone())
        .collect();
    assert_eq!(clients, ["claude-code", "unknown"]);
    anonymous.close();
    mcp.close();
}

#[test]
fn send_is_refused_for_work_that_is_not_live_and_a_subagent_that_stopped() {
    let p = Project::new(&[]);
    p.start();
    let sessions = ["c", "f", "e"].map(|name| {
        let session = format!("{name}0000000-0000-4000-8000-000000000000");
        (session, p.claude_worktree(name, false))
    });
    let [closed, finished, ended] = sessions
        .each_ref()
        .map(|(session, worktree)| p.session_start_as(session, worktree));
    p.ok(&["close", &closed, "--reason", "unfinished"]);
    p.ok(&["finish", &finished]);
    let mut fields = p.head(&sessions[2].0, &p.root);
    fields.extend([
        ("hook_event_name", "SessionEnd".into()),
        ("reason", "prompt_input_exit".into()),
    ]);
    assert_eq!(p.hook(&payload(&fields)), "");
    p.until(&ended, "the session's end", |run| {
        !run["attachment"]["sessions"][0]["ended_at"].is_null()
    });
    // A subagent whose parent holds its worktree's lock: its stop ends no
    // session.
    let parent = Agent::start();
    let isolated = p.claude_worktree(&format!("agent-{SUBAGENT}"), true);
    fs::write(
        p.gitdir(&isolated).join("locked"),
        parent.subagent_lock(SUBAGENT),
    )
    .unwrap();
    let subagent = p.subagent_start(&isolated);
    p.subagent_stop(&isolated);
    p.until(&subagent, "the subagent's stop", |run| {
        let session = &run["attachment"]["sessions"][0];
        session["ended_at"].is_null() && !session["subagent"]["stopped_at"].is_null()
    });

    let mut mcp = Mcp::start(&p, &p.root, &[]);
    mcp.initialize();
    let only = "only a live session gets a message";
    for (id, why) in [
        (&closed, format!("Work {closed} is closed; {only}.")),
        (&finished, format!("Work {finished} is finished; {only}.")),
        (&ended, format!("Work {ended}'s session ended; {only}.")),
        (
            &subagent,
            format!(
                "Work {subagent} is a subagent that stopped, and a stopped subagent is not woken."
            ),
        ),
    ] {
        assert_eq!(mcp.refused("send", json!({"id": id, "text": "x"})), why);
    }
    assert_eq!(p.of("message.written"), 0);
    mcp.close();
}

/// No launch consent, an agent it does not name, forwarded environment and
/// arguments that are not an agent and a task: each refused in a sentence,
/// and nothing launched.
#[test]
fn start_launches_nothing_its_launch_consent_does_not_allow() {
    let (p, mut mcp) = server();
    mcp.initialize();
    let task = json!({"agent": "fake-good", "task": "add a file"});
    assert_eq!(
        mcp.refused("start", task.clone()),
        "Nothing may be launched here until the person allows it, with `dispatch setup` in a terminal."
    );
    p.allow(&["fake-good", "fake-bad"], 3);
    assert_eq!(
        mcp.refused("start", json!({"agent": "claude", "task": "add a file"})),
        "The launch consent here names fake-bad, fake-good, not \"claude\"; start one of \
         those, or ask the person to change it with `dispatch setup`."
    );
    for arguments in [
        json!({}),
        json!({"agent": "fake-good"}),
        json!({"task": "add a file"}),
        json!({"agent": "fake-good", "task": " \n"}),
        json!({"agent": 5, "task": "add a file"}),
        json!({"agent": "fake-good", "task": ["add a file"]}),
        json!({"agent": "fake-good", "task": "add a file", "then": "accept"}),
    ] {
        mcp.refused("start", arguments);
    }
    // Only the person, at a terminal, passes --allow-forwarded-env.
    fs::write(
        p.root.join("dispatch.yml"),
        "execution:\n  forwarded_env: [DISPATCH_TEST_TOKEN]\n",
    )
    .unwrap();
    assert_eq!(
        mcp.refused("start", task),
        "This project's dispatch.yml forwards environment variables to its runs, so only the \
         person can launch one, from a terminal, with --allow-forwarded-env."
    );
    assert!(!p.state.join("started").exists(), "something was launched");
    assert_eq!(p.ids(), Vec::<String>::new(), "a run was made");
    mcp.close();
}

/// Launch consent holds for each agent's command and the project's checks
/// as the person allowed them. A check or an executable that a pull or a
/// branch switch changed since is refused before anything runs; unchanged,
/// the agent launches.
#[test]
fn start_refuses_an_agent_whose_command_or_checks_changed_since_it_was_allowed() {
    use std::os::unix::fs::PermissionsExt;

    let (p, mut mcp) = server();
    mcp.initialize();
    let ran = p.temp.path().join("ran");
    let stand_in = |name: &str| {
        let path = p.temp.path().join(name);
        fs::write(&path, format!("#!/bin/sh\ntouch {ran:?}\nexit 91\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    let (first, second) = (stand_in("claude-first"), stand_in("claude-second"));
    let config = |claude: &Path, verify: &[String]| {
        let verify: String = verify.iter().map(|c| format!("    - {c:?}\n")).collect();
        let checks = if verify.is_empty() {
            String::new()
        } else {
            format!("checks:\n  verify:\n{verify}")
        };
        let yml = format!("{checks}harnesses:\n  claude:\n    executable: {claude:?}\n");
        fs::write(p.root.join("dispatch.yml"), yml).unwrap();
    };
    let changed = |name: &str| {
        format!(
            "This project's command or checks for {name} changed since the person allowed \
             launches; ask them to allow it again with `dispatch setup`."
        )
    };
    config(&first, &[]);
    p.allow(&["fake-good", "claude"], 3);
    // A check added since, as the reviewer reproduced it.
    config(&first, &[format!("touch {}", ran.display())]);
    for (agent, name) in [("fake-good", "fake-good"), ("claude", "Claude Code")] {
        let task = json!({"agent": agent, "task": "add a file"});
        assert_eq!(mcp.refused("start", task), changed(name));
    }
    // Another executable since.
    config(&second, &[]);
    let task = json!({"agent": "claude", "task": "add a file"});
    assert_eq!(mcp.refused("start", task), changed("Claude Code"));
    assert!(!ran.exists(), "a changed command ran");
    assert!(!p.state.join("started").exists(), "something was launched");

    // As the person allowed it: it launches, and its checks run.
    config(&first, &["true".to_owned()]);
    p.allow(&["fake-good"], 3);
    let started = mcp.tool("start", json!({"agent": "fake-good", "task": "add a file"}));
    let woke = mcp.tool("wait", json!({"ids": [started["id"]], "timeout_s": 300}));
    assert_eq!(woke["woke"], "finished", "{woke}");
    assert_eq!(woke["work"][0]["state"], "ready", "{woke}");
    assert!(!ran.exists());
    mcp.close();
}

/// `max_running` counts this server's own launch before its run is
/// created, and once it is, counts it once.
#[test]
fn start_holds_to_max_running_counting_its_own_launch_before_its_run_is_created() {
    let (p, mut mcp) = server();
    p.allow(&["fake-timeout"], 1);
    mcp.initialize();
    let task = json!({"agent": "fake-timeout", "task": "take your time"});
    let full = "1 of this project's native runs is running, and its launch consent allows 1 at \
                once; wait for one to finish, then start again.";
    // Hold writes once the database is made, so that `dispatch run` cannot
    // record its run before it is stopped, as soon as it is found.
    dispatch::db::Database::open(p.state.join("dispatch.db")).unwrap();
    let db = rusqlite::Connection::open(p.state.join("dispatch.db")).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let start = mcp.request(
        "tools/call",
        json!({"name": "start", "arguments": task.clone()}),
    );
    until("the launch", || !p.launched().is_empty());
    let id = p.launched()[0].clone();
    let mut pid = None;
    until("its dispatch run", || {
        pid = p.launch_pid(&id);
        pid.is_some()
    });
    let pid = pid.unwrap();
    // SAFETY: signals to the one process this test launched.
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    drop(db);
    assert_eq!(result(&mcp.answer(&start).1)["id"], id);
    assert!(!p.created(&id));
    assert_eq!(mcp.refused("start", task.clone()), full);
    // SAFETY: as above.
    unsafe { libc::kill(pid, libc::SIGCONT) };
    until("its run created", || p.created(&id));
    assert_eq!(mcp.refused("start", task), full);
    assert_eq!(p.launched(), [id]);
    mcp.close();
}

/// A launched run whose `dispatch run` was killed after its agent started
/// stays unfinished, and keeps its place while that agent, in its own
/// process group, still runs; once the agent is gone too, it gives its
/// place back.
#[test]
fn a_launched_run_whose_dispatch_run_was_killed_keeps_its_place_while_its_agent_runs() {
    let (p, mut mcp) = server();
    p.allow(&["fake-timeout"], 1);
    mcp.initialize();
    let task = json!({"agent": "fake-timeout", "task": "take your time"});
    let full = "1 of this project's native runs is running, and its launch consent allows 1 at \
                once; wait for one to finish, then start again.";
    let first = mcp.tool("start", task.clone())["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut group = None;
    until("its agent started", || {
        group = p.agent_group(&first);
        group.is_some()
    });
    let group = group.unwrap();
    assert_eq!(mcp.refused("start", task.clone()), full);
    // As an OOM kill or a SIGKILL would: kill only its `dispatch run`, and
    // wait until this server reaped it.
    let pid = p.launch_pid(&first).expect("its dispatch run");
    // SAFETY: a signal to the one process this test launched, then probes
    // of it.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    until(
        "its dispatch run reaped",
        || unsafe { libc::kill(pid, 0) } != 0,
    );
    let while_alive = mcp.call("start", task.clone());
    // Its agent still runs. Kill its group before asserting, so that a
    // failure leaves no agent behind.
    // SAFETY: the agent leads its own process group; this kills only it.
    unsafe { libc::kill(-group, libc::SIGKILL) };
    assert_eq!(while_alive["result"]["isError"], true, "{while_alive}");
    assert_eq!(
        while_alive["result"]["content"][0]["text"], full,
        "{while_alive}"
    );
    let mut again = Value::Null;
    until("its place back", || {
        again = mcp.call("start", task.clone());
        again["result"]["isError"] != true
    });
    let again = result(&again)["id"].as_str().unwrap().to_owned();
    assert_ne!(again, first);
    let rows = mcp.tool("work", json!({}))["work"].clone();
    let row = |id: &str| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|r| r["run_id"] == id)
            .cloned()
    };
    assert_ne!(p.run(&first)["outcome"]["lifecycle"], "finished");
    assert!(row(&first).is_some(), "{rows}");
    mcp.close();
}

/// A launch answers its id once its run is created, with the task given
/// verbatim, its own output in its private log and none on stdout; `wait`
/// on the id answers `finished`; the run records who asked right after
/// `run.created`; and its process is reaped.
#[test]
fn a_launch_answers_its_id_and_wait_answers_finished_for_it() {
    use std::os::unix::fs::PermissionsExt;

    let (p, mut mcp) = server();
    p.allow(&["fake-good"], 3);
    mcp.initialize();
    let task = "--allow-forwarded-env \"quoted\" ü 🙂\nDispatch: a second line\n";
    let started = mcp.tool("start", json!({"agent": "fake-good", "task": task}));
    let id = started["id"].as_str().unwrap().to_owned();
    assert_eq!(p.launched(), std::slice::from_ref(&id));
    assert_eq!(started["agent"], "fake-good");
    assert_eq!(started.get("task"), None, "{started}");
    let log = p.state.join("started").join(format!("{id}.log"));
    assert_eq!(started["log"], log.to_str().unwrap());
    assert!(
        started["note"]
            .as_str()
            .unwrap()
            .starts_with(&format!("Wait on it with wait {{\"ids\": [\"{id}\"]}};")),
        "{started}"
    );
    let file = p.state.join("started").join(format!("{id}.task"));
    assert_eq!(fs::read_to_string(&file).unwrap(), task);
    for private in [&file, &log] {
        let mode = fs::metadata(private).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{}", private.display());
    }
    // Once its run is created, `status` knows it.
    until("its run created", || p.created(&id));
    assert_eq!(mcp.tool("status", json!({"id": id}))["run_id"], id);

    let woke = mcp.tool("wait", json!({"ids": [id], "timeout_s": 300}));
    assert_eq!(woke["woke"], "finished", "{woke}");
    let [row] = &woke["work"].as_array().unwrap()[..] else {
        panic!("one row: {woke}");
    };
    assert_eq!(row["run_id"], id);
    assert_eq!(row["origin"], "native");
    assert_eq!(row["agent"], "fake-good");
    assert_eq!(row["state"], "ready");
    assert_eq!(row["task"], task);
    assert_eq!(p.run(&id)["task"], task);
    let output = fs::read_to_string(&log).unwrap();
    assert!(output.contains(&format!("RUN {id}")), "{output}");

    let events: Vec<_> = p.events().into_iter().filter(|e| e.1 == id).collect();
    assert_eq!(events[0].2, "run.created");
    assert_eq!(events[1].2, "launch.requested");
    let asked: Value = serde_json::from_str(&events[1].3).unwrap();
    assert_eq!(
        (&asked["by"], &asked["client"], &asked["agent"]),
        (&json!("mcp"), &json!("claude-code"), &json!("fake-good"))
    );
    assert_eq!(p.of("launch.requested"), 1);

    let pid = p.run(&id)["execution"]["supervisor"]["pid"]
        .as_i64()
        .unwrap();
    let pid = i32::try_from(pid).unwrap();
    // SAFETY: signal 0 only asks whether the process, a zombie too, exists.
    until(
        "the launched run reaped",
        || unsafe { libc::kill(pid, 0) } != 0,
    );
    mcp.close();
}

/// `launch.requested` names the client as its `initialize` named itself,
/// as Codex and Cursor do, or `unknown`.
#[test]
fn launch_requested_names_the_client_that_asked() {
    let p = Project::new(&[]);
    p.allow(&["fake-good"], 3);
    let handshakes = [
        (
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{"experimental":{"codex/auth-change":{}},"elicitation":{"form":{},"url":{}}},"clientInfo":{"name":"codex-mcp-client","title":"Codex","version":"0.170.0"}}}"#,
            "codex-mcp-client",
        ),
        (
            r#"{"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{"form":{}},"extensions":{"io.modelcontextprotocol/ui":{"mimeTypes":["text/html;profile=mcp-app"]}}},"clientInfo":{"name":"Cursor","version":"1.0.0"}},"jsonrpc":"2.0","id":0}"#,
            "Cursor",
        ),
        (
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#,
            "unknown",
        ),
    ];
    for (initialize, client) in handshakes {
        let mut mcp = Mcp::start(&p, &p.root, &[]);
        mcp.send(initialize);
        mcp.answer(&json!(0));
        let started = mcp.tool("start", json!({"agent": "fake-good", "task": client}));
        let id = started["id"].as_str().unwrap();
        let woke = mcp.tool("wait", json!({"ids": [id], "timeout_s": 300}));
        assert_eq!(woke["woke"], "finished", "{woke}");
        let event = p
            .events()
            .into_iter()
            .find(|e| e.1 == id && e.2 == "launch.requested")
            .expect("launch.requested");
        let asked: Value = serde_json::from_str(&event.3).unwrap();
        assert_eq!(asked["client"], client);
        mcp.close();
    }
}

/// A launch whose `dispatch run` exits before its run is created is
/// reported once, with its log's last lines: by `start` when that happens
/// while it waits, else by the next `work`, or by a `wait` on it. It is
/// never launched again, and what it left half made takes no place.
#[test]
fn a_launch_that_exits_before_its_run_is_created_is_reported_once() {
    let (p, mut mcp) = server();
    p.allow(&["fake-good", "fake-nope"], 1);
    mcp.initialize();
    let id = mcp.tool("start", json!({"agent": "fake-good", "task": "first"}))["id"].clone();
    let woke = mcp.tool("wait", json!({"ids": [id], "timeout_s": 300}));
    assert_eq!(woke["woke"], "finished", "{woke}");

    // No harness answers to fake-nope: `dispatch run` stops at once. `start`
    // reports it, or, when it stopped only after `start` answered, the first
    // `work` that sees it does; then neither reports it again. The first
    // `dispatch run` holds the one place until this server reaped it, and a
    // refused `start` launches nothing, so it is sent again until then.
    let full = "1 of this project's native runs is running, and its launch consent allows 1 at \
                once; wait for one to finish, then start again.";
    let mut answer = Value::Null;
    until("the first run's place", || {
        answer = mcp.call("start", json!({"agent": "fake-nope", "task": "nope"}));
        answer["result"]["isError"] != true || answer["result"]["content"][0]["text"] != full
    });
    let said = if answer["result"]["isError"] == true {
        let said = answer["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            said.starts_with("The launch exited before its run was created (exit status: "),
            "{said}"
        );
        said.to_owned()
    } else {
        let id = result(&answer)["id"].clone();
        let mut work = Value::Null;
        until("work to report it", || {
            work = mcp.tool("work", json!({}));
            work.get("launch_failed").is_some()
        });
        let [failed] = &work["launch_failed"].as_array().unwrap()[..] else {
            panic!("one failure: {work}");
        };
        assert_eq!(failed["id"], id);
        failed["last_lines"].as_str().unwrap().to_owned()
    };
    assert!(said.contains("unknown harness \"fake-nope\""), "{said}");
    assert!(mcp.tool("work", json!({})).get("launch_failed").is_none());

    for reporter in ["work", "wait"] {
        // Hold writes, so that `dispatch run` cannot record its run before
        // it is stopped, as soon as it is found.
        let db = rusqlite::Connection::open(p.state.join("dispatch.db")).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let before = p.launched();
        let start = mcp.request(
            "tools/call",
            json!({"name": "start", "arguments": {"agent": "fake-good", "task": reporter}}),
        );
        let mut id = String::new();
        until("the launch", || {
            let new = p.launched().into_iter().find(|id| !before.contains(id));
            id = new.unwrap_or_default();
            !id.is_empty()
        });
        let log = p.state.join("started").join(format!("{id}.log"));
        let mut pid = None;
        until("its dispatch run", || {
            pid = p.launch_pid(&id);
            pid.is_some()
        });
        let pid = pid.unwrap();
        // SAFETY: signals to the one process this test launched.
        unsafe { libc::kill(pid, libc::SIGSTOP) };
        drop(db);
        let started = result(&mcp.answer(&start).1);
        assert_eq!(started["id"], id);
        assert!(!p.created(&id));
        let waiting = (reporter == "wait").then(|| {
            let arguments = json!({"ids": [id], "timeout_s": 300});
            mcp.request(
                "tools/call",
                json!({"name": "wait", "arguments": arguments}),
            )
        });
        // SAFETY: as above.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let failed = match waiting {
            Some(waiting) => {
                let woke = result(&mcp.answer(&waiting).1);
                assert_eq!(woke["woke"], "launch_failed", "{woke}");
                assert_eq!(woke["work"], json!([]));
                woke["launch_failed"].clone()
            }
            None => {
                let mut work = Value::Null;
                until("work to report it", || {
                    work = mcp.tool("work", json!({}));
                    work.get("launch_failed").is_some()
                });
                work["launch_failed"].clone()
            }
        };
        let [failed] = &failed.as_array().unwrap()[..] else {
            panic!("one failure: {failed}");
        };
        assert_eq!(failed["id"], id);
        assert_eq!(failed["agent"], "fake-good");
        assert_eq!(failed["task"], reporter);
        assert_eq!(failed["log"], log.to_str().unwrap());
        assert!(
            failed["exit"].as_str().unwrap().contains("SIGKILL"),
            "{failed}"
        );
        // It was stopped as soon as it was found: its log holds what it
        // said by then, if anything.
        let written = fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = written.lines().collect();
        let last = lines[lines.len().saturating_sub(20)..].join("\n");
        assert_eq!(failed["last_lines"], last);

        // Once: neither work nor a wait reports it again.
        assert!(mcp.tool("work", json!({})).get("launch_failed").is_none());
        let unknown = mcp.refused("wait", json!({"ids": [id], "timeout_s": 1}));
        assert!(unknown.starts_with("No Work"), "{unknown}");
        assert!(p.launch_pid(&id).is_none());
    }
    // Never launched again, and the runs left half made take no place.
    assert_eq!(p.launched().len(), 4);
    let id = mcp.tool("start", json!({"agent": "fake-good", "task": "last"}))["id"].clone();
    let woke = mcp.tool("wait", json!({"ids": [id], "timeout_s": 300}));
    assert_eq!(woke["woke"], "finished", "{woke}");
    mcp.close();
}
