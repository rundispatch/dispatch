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

const TOOLS: [&str; 4] = ["work", "status", "wait", "propose_landing"];

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

/// The lock at `path`, held as Dispatch holds one until the file is
/// dropped; `None` when another holds it.
fn hold(path: &Path) -> Option<fs::File> {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .unwrap();
    // SAFETY: the descriptor stays open while the lock is held.
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (locked == 0).then_some(file)
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
        let output = Command::new("ps")
            .env("TZ", "UTC0")
            .args(["-o", "lstart=", "-p", &self.0.id().to_string()])
            .output()
            .unwrap();
        let start = String::from_utf8(output.stdout).unwrap();
        let start = start.split_whitespace().collect::<Vec<_>>().join(" ");
        format!("claude session {name} (pid {} start {start})", self.0.id())
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Project {
    temp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    /// The serve lock while no owner runs: the project reads as watched.
    watched: Option<fs::File>,
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

    fn newest_in(&self, workspace: &Path) -> String {
        self.ids()
            .into_iter()
            .rev()
            .find(|id| self.run(id)["attachment"]["workspace"].as_str() == workspace.to_str())
            .expect("registered")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        self.watched = None;
        let _ = self.dispatch(&["stop"]);
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
    assert_eq!(names, TOOLS, "exactly the four tools, none that lands");
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
        4
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
        4
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

    mcp.close();
    assert_eq!(
        Snapshot::of(&p),
        before,
        "a tool changed what only a person may"
    );
    assert_eq!(p.of("batch.proposed"), 0);
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
