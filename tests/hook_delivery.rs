//! Hooks that never wait on Dispatch: notes delivered to a live agent from an
//! inbox file the owner wrote, claimed by rename, and session ends and waits
//! left as marks the project owner records. The project here is watched by
//! nobody but this test, holding its serve lock, so only `owner` records,
//! when the test says; the payloads are the P0 probes', paths replaced.
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

/// A hook's registration budget where the deadline is not under test.
const NO_DEADLINE_MS: &str = "600000";

/// The timeout `dispatch setup` installs for every hook but `SessionStart`
/// and the removals.
const HOOK_TIMEOUT: Duration = Duration::from_secs(5);

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

fn flock(path: &Path) -> fs::File {
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
    file
}

struct Project {
    _temp: tempfile::TempDir,
    root: PathBuf,
    worktree: PathBuf,
    state: PathBuf,
    watched: Option<fs::File>,
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
        let worktree = ".claude/worktrees/x";
        git(&root, &["worktree", "add", "--quiet", "-b", "x", worktree]);
        let mut project = Self {
            root: fs::canonicalize(&root).unwrap(),
            worktree: fs::canonicalize(root.join(worktree)).unwrap(),
            state: temp.path().join("state"),
            watched: None,
            _temp: temp,
        };
        project.hold_watch();
        project
    }

    fn key(path: &Path) -> String {
        hex::encode(Sha256::digest(path.as_os_str().as_encoded_bytes()))
    }

    /// Hold the project's serve lock, as an owner would.
    fn hold_watch(&mut self) {
        let lock = format!("serve-{}.lock", Self::key(&self.root));
        self.watched = Some(flock(&self.state.join("locks").join(lock)));
    }

    /// Let go of the serve lock, and wait until nobody holds it: a process
    /// forked meanwhile holds it with us until it execs.
    fn release_watch(&mut self) {
        self.watched = None;
        let path = self
            .state
            .join("locks")
            .join(format!("serve-{}.lock", Self::key(&self.root)));
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let probe = fs::File::open(&path).unwrap();
            // SAFETY: a non-blocking probe on a descriptor this test owns.
            if unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return;
            }
            assert!(Instant::now() < deadline, "the serve lock stayed held");
            std::thread::sleep(Duration::from_millis(50));
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

    /// Run `dispatch hook <provider>` with `input` and `env`, and how long
    /// it took. The whole input must be taken: a hook that leaves some
    /// unread fails the write here.
    fn hook(&self, provider: &str, input: &[u8], env: &[(&str, &Path)]) -> (Output, Duration) {
        let mut command = Command::new(assert_cmd::cargo_bin!("dispatch"));
        command
            .arg("--state-dir")
            .arg(&self.state)
            .args(["hook", provider])
            .current_dir(&self.root)
            .env("DISPATCH_REGISTRATION_BUDGET_MS", NO_DEADLINE_MS)
            .env_remove("DISPATCH_REGISTRATION_FAULT")
            .env_remove("TERMINAL_CWD")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in env {
            command.env(name, value);
        }
        let began = Instant::now();
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        let output = child.wait_with_output().unwrap();
        (output, began.elapsed())
    }

    /// A Claude Code hook that succeeded, and what it printed.
    fn claude(&self, input: &str) -> Value {
        let (output, _) = self.hook("claude", input.as_bytes(), &[]);
        Self::printed(&output)
    }

    fn printed(output: &Output) -> Value {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        match stdout.trim() {
            "" => Value::Null,
            reply => serde_json::from_str(reply).unwrap(),
        }
    }

    /// `SessionStart` in the worktree as the probe recorded it.
    fn start(&self, session: &str, source: &str) -> String {
        Self::start_in(&self.worktree, session, source)
    }

    fn start_in(cwd: &Path, session: &str, source: &str) -> String {
        payload(&[
            ("session_id", session.into()),
            (
                "transcript_path",
                format!("/home/u/.claude/projects/p/{session}.jsonl").into(),
            ),
            ("cwd", cwd.to_str().unwrap().into()),
            (
                "scratchpad_dir",
                format!("/tmp/claude/{session}/scratchpad").into(),
            ),
            ("hook_event_name", "SessionStart".into()),
            ("source", source.into()),
            ("model", "claude-opus-5-5".into()),
        ])
    }

    /// `PostToolUse` in `cwd` as the probe recorded it, with the tool's
    /// `stdout`.
    fn tool(cwd: &Path, stdout: &str) -> String {
        payload(&[
            ("session_id", "d8e3f50b-7e08-4064-bae3-d75790d0d970".into()),
            (
                "transcript_path",
                "/home/u/.claude/projects/p/d8e3f50b.jsonl".into(),
            ),
            ("cwd", cwd.to_str().unwrap().into()),
            ("scratchpad_dir", "/tmp/claude/d8e3f50b/scratchpad".into()),
            ("prompt_id", "4cce8570-1f2e-48f3-911e-3f6de3a7936a".into()),
            ("permission_mode", "auto".into()),
            ("effort", serde_json::json!({"level": "high"})),
            ("hook_event_name", "PostToolUse".into()),
            ("tool_name", "Bash".into()),
            (
                "tool_input",
                serde_json::json!({"command": "ls", "description": "List files"}),
            ),
            (
                "tool_response",
                serde_json::json!({"stdout": stdout, "stderr": "", "interrupted": false}),
            ),
            ("tool_use_id", "toolu_012CZdy6zhyfJAasPA9MioXo".into()),
            ("duration_ms", 3519.into()),
        ])
    }

    /// The Work a fresh session in the worktree registered.
    fn registered(&self) -> String {
        let reply = self.claude(&self.start("s1", "startup"));
        let notice = reply["systemMessage"].as_str().unwrap_or_default();
        let id = notice
            .strip_prefix("Dispatch is tracking this worktree as Work ")
            .and_then(|rest| rest.split(';').next())
            .unwrap_or_else(|| panic!("not registered: {reply}"));
        id.to_owned()
    }

    fn run(&self, id: &str) -> Value {
        let path = self.state.join("runs").join(id).join("metadata.json");
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    /// The payloads of `id`'s events of `kind`.
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

    /// Leave notes for Work `id` in its workspace's inbox, as the owner
    /// writes it.
    fn inbox(&self, id: &str, notes: &[(&str, &str)]) {
        let notes: Vec<Value> = notes
            .iter()
            .map(|(id, text)| serde_json::json!({"id": id, "text": text}))
            .collect();
        let inbox = serde_json::json!({"version": 1, "run_id": id, "notes": notes});
        let directory = self.state.join("inbox");
        fs::create_dir_all(directory.join("delivered")).unwrap();
        let path = directory.join(format!("{}.json", Self::key(&self.worktree)));
        fs::write(path, inbox.to_string()).unwrap();
    }

    /// Run the project owner until `done` holds, then stop it.
    fn owner(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        self.release_watch();
        let output = self.dispatch(&["start"]);
        assert!(output.status.success(), "{output:?}");
        let deadline = Instant::now() + Duration::from_secs(300);
        while !done(self) {
            assert!(Instant::now() < deadline, "the owner never {what}");
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(self.dispatch(&["stop"]).status.success());
        self.hold_watch();
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = self.dispatch(&["stop"]);
    }
}

/// Two sessions starting in one worktree at once, while its Work is busy (a
/// finish holds its lock past the 10 s the second used to wait for the
/// first): both join the Work.
#[test]
fn a_second_session_start_waits_for_the_first_while_the_work_is_busy() {
    let p = Project::new();
    let id = p.registered();
    let busy = flock(&p.state.join("runs").join(&id).join(".operation.lock"));
    let starts: Vec<_> = ["s2", "s3"]
        .into_iter()
        .map(|session| {
            let input = p.start(session, "resume");
            let mut command = Command::new(assert_cmd::cargo_bin!("dispatch"));
            command
                .arg("--state-dir")
                .arg(&p.state)
                .args(["hook", "claude"])
                .env("DISPATCH_REGISTRATION_BUDGET_MS", "60000")
                .env_remove("DISPATCH_REGISTRATION_FAULT")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            child
        })
        .collect();
    std::thread::sleep(Duration::from_secs(12));
    drop(busy);
    for child in starts {
        let reply = Project::printed(&child.wait_with_output().unwrap());
        assert_eq!(reply, Value::Null, "the session joins its Work silently");
    }
    let run = p.run(&id);
    let mut sessions: Vec<&str> = run["attachment"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["session_id"].as_str().unwrap())
        .collect();
    sessions.sort();
    assert_eq!(sessions, ["s1", "s2", "s3"]);
}

/// A JSON object with `fields` in this order, as the runtimes send them
/// (`json!` would sort them).
fn payload(fields: &[(&str, Value)]) -> String {
    let fields: Vec<String> = fields
        .iter()
        .map(|(name, value)| format!("{}: {value}", Value::from(*name)))
        .collect();
    format!("{{{}}}", fields.join(", "))
}

/// What a Claude Code hook gave the agent: its `additionalContext`, after
/// the event `event`.
fn context(reply: &Value, event: &str) -> Option<String> {
    let output = &reply["hookSpecificOutput"];
    if output.is_null() {
        return None;
    }
    assert_eq!(output["hookEventName"], event, "{reply}");
    Some(output["additionalContext"].as_str().unwrap().to_owned())
}

#[test]
fn each_note_is_delivered_once_under_twenty_hooks_at_once_and_the_owner_records_it() {
    let mut p = Project::new();
    let id = p.registered();
    let notes = [
        (
            "n1",
            "Work 01ABC (claude) edited fn f in src/lib.rs, which this Work also edits.",
        ),
        (
            "n2",
            "Work 01DEF (hermes) changed fn g in src/api.rs, which this Work uses.",
        ),
    ];
    p.inbox(&id, &notes);
    // Some from a subdirectory, after `cd src`.
    let replies: Vec<Value> = std::thread::scope(|scope| {
        let hooks: Vec<_> = (0..20)
            .map(|n| {
                let cwd = if n % 2 == 0 {
                    p.worktree.clone()
                } else {
                    p.worktree.join("src")
                };
                let p = &p;
                scope.spawn(move || p.claude(&Project::tool(&cwd, "lib.rs\n")))
            })
            .collect();
        hooks.into_iter().map(|hook| hook.join().unwrap()).collect()
    });
    let delivered: Vec<String> = replies
        .iter()
        .filter_map(|reply| context(reply, "PostToolUse"))
        .collect();
    assert_eq!(delivered.len(), 1, "{replies:?}");
    assert_eq!(delivered[0], format!("{}\n{}", notes[0].1, notes[1].1));
    assert_eq!(replies.iter().filter(|reply| reply.is_null()).count(), 19);
    let pending = p
        .state
        .join("inbox")
        .join(format!("{}.json", Project::key(&p.worktree)));
    assert!(!pending.exists());

    p.owner("recorded the delivery", |p| {
        p.events(&id, "note.delivered").len() == 2
    });
    let recorded: Vec<(String, String)> = p
        .events(&id, "note.delivered")
        .iter()
        .map(|event| {
            (
                event["note"].as_str().unwrap().into(),
                event["via"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        recorded,
        [
            ("n1".into(), "PostToolUse".into()),
            ("n2".into(), "PostToolUse".into())
        ]
    );
    assert_eq!(
        fs::read_dir(p.state.join("inbox/delivered"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn a_workspaces_notes_are_found_from_anywhere_in_it_and_only_there() {
    let p = Project::new();
    let id = p.registered();
    p.inbox(
        &id,
        &[("n1", "Work 01ABC (claude) landed fn f in src/lib.rs.")],
    );
    // The main checkout is another workspace, with no notes.
    assert_eq!(
        p.claude(&Project::tool(&p.root.join("src"), "")),
        Value::Null
    );
    let deep = p.worktree.join("src/deeper");
    fs::create_dir_all(&deep).unwrap();
    let reply = p.claude(&Project::tool(&deep, ""));
    assert_eq!(
        context(&reply, "PostToolUse").as_deref(),
        Some("Work 01ABC (claude) landed fn f in src/lib.rs.")
    );
    assert_eq!(
        p.claude(&Project::tool(&deep, "")),
        Value::Null,
        "delivered once"
    );
}

#[test]
fn nothing_pending_prints_nothing_and_an_unwatched_project_costs_nothing() {
    let p = Project::new();
    let id = p.registered();
    assert_eq!(p.claude(&Project::tool(&p.worktree, "")), Value::Null);
    p.inbox(&id, &[]);
    assert_eq!(p.claude(&Project::tool(&p.worktree, "")), Value::Null);
    assert_eq!(p.claude(&p.start("s2", "resume")), Value::Null);

    // No state at all: nothing is made, nothing printed, even for a tool
    // output past what the rest of the hook reads.
    let elsewhere = p._temp.path().join("unwatched-state");
    let big = "x".repeat(200 * 1024);
    for (provider, input) in [
        ("claude", Project::tool(&p.worktree, &big)),
        ("hermes", hermes_turn(&p.root, 1024)),
    ] {
        let output = Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&elsewhere)
            .args(["hook", provider])
            .env("TERMINAL_CWD", &p.worktree)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child.stdin.take().unwrap().write_all(input.as_bytes())?;
                child.wait_with_output()
            })
            .unwrap();
        assert!(output.status.success(), "{provider}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "", "{provider}");
    }
    assert!(!elsewhere.exists());
}

#[test]
fn a_resumed_session_hears_its_notes_with_the_start_reply() {
    let p = Project::new();
    let id = p.registered();
    p.inbox(
        &id,
        &[("n1", "Work 01ABC (claude) landed fn f in src/lib.rs.")],
    );
    let reply = p.claude(&p.start("s2", "resume"));
    assert_eq!(
        context(&reply, "SessionStart").as_deref(),
        Some("Work 01ABC (claude) landed fn f in src/lib.rs.")
    );
    assert!(reply["systemMessage"].is_null(), "{reply}");
    // In the checkout itself: today's notice, and no notes of the worktree's.
    p.inbox(&id, &[("n2", "x")]);
    let reply = p.claude(&Project::start_in(&p.root, "s3", "startup"));
    assert!(
        reply["systemMessage"]
            .as_str()
            .unwrap()
            .contains("works directly in your checkout")
    );
    assert!(reply["hookSpecificOutput"].is_null(), "{reply}");
}

/// A Hermes `pre_llm_call` payload as the probe recorded it (cwd the main
/// checkout), its conversation grown to about `size` bytes.
fn hermes_turn(root: &Path, size: usize) -> String {
    let history: Vec<Value> = (0..size / 1024)
        .map(|turn| serde_json::json!({"role": "user", "content": format!("{turn:04}{}", "y".repeat(1000))}))
        .collect();
    let extra = serde_json::json!({
        "task_id": "20261104_074818_70b971",
        "turn_id": "20261104_074818_70b971:20261104_074818_70b971:856cfd7e",
        "user_message": "Run pwd once with your terminal tool.",
        "conversation_history": history, "is_first_turn": false, "model": "gpt-6.1-sol",
        "platform": "cli", "sender_id": "", "telemetry_schema_version": "hermes.observer.v1",
    });
    payload(&[
        ("hook_event_name", "pre_llm_call".into()),
        ("tool_name", Value::Null),
        ("tool_input", Value::Null),
        ("session_id", "20261104_074818_70b971".into()),
        ("cwd", root.to_str().unwrap().into()),
        ("extra", extra),
    ])
}

#[test]
fn hermes_hears_its_worktrees_notes_before_the_turn_from_a_megabyte_payload() {
    let p = Project::new();
    let id = p.registered();
    p.inbox(
        &id,
        &[("n1", "Work 01ABC (claude) landed fn f in src/lib.rs.")],
    );
    let turn = hermes_turn(&p.root, 1024 * 1024);
    assert!(turn.len() > 1024 * 1024);
    let hermes = |cwd: Option<&Path>| {
        let env: Vec<(&str, &Path)> = cwd.map(|cwd| ("TERMINAL_CWD", cwd)).into_iter().collect();
        let (output, _) = p.hook("hermes", turn.as_bytes(), &env);
        Project::printed(&output)
    };
    // The hook's own cwd and the payload's are the main checkout: only
    // `TERMINAL_CWD` names the worktree.
    assert_eq!(hermes(None), Value::Null);
    assert_eq!(hermes(Some(&p.root)), Value::Null);
    assert_eq!(
        hermes(Some(&p.worktree.join("src"))),
        serde_json::json!({"context": "Work 01ABC (claude) landed fn f in src/lib.rs."})
    );
    assert_eq!(hermes(Some(&p.worktree)), Value::Null, "delivered once");
}

/// While the Work's lock and a write lock on the database are held, every
/// hook path but the two removals (which must keep the Δ first) returns
/// within its budget, and nothing a hook reported is lost: the next owner
/// records it, with the time the hook saw.
#[test]
fn no_hook_waits_on_the_works_lock_or_the_database_and_the_next_owner_records_its_marks() {
    let mut p = Project::new();
    let id = p.registered();
    p.inbox(
        &id,
        &[("n1", "Work 01ABC (claude) landed fn f in src/lib.rs.")],
    );
    let busy = flock(&p.state.join("runs").join(&id).join(".operation.lock"));
    let db = rusqlite::Connection::open(p.state.join("dispatch.db")).unwrap();
    db.execute_batch("BEGIN EXCLUSIVE").unwrap();

    let transcript = p._temp.path().join("s1.jsonl");
    fs::write(&transcript, "{}\n").unwrap();
    let common = |event: &str| {
        serde_json::json!({
            "session_id": "s1", "transcript_path": transcript, "cwd": p.worktree,
            "hook_event_name": event,
        })
    };
    let mut notification = common("Notification");
    notification["notification_type"] = "permission_prompt".into();
    notification["message"] = "Claude needs your permission".into();
    let mut stop = common("Stop");
    stop["stop_hook_active"] = false.into();
    let mut end = common("SessionEnd");
    end["reason"] = "prompt_input_exit".into();
    let hooks: [(&str, String, Duration); 6] = [
        (
            "claude",
            Project::tool(&p.worktree.join("src"), ""),
            HOOK_TIMEOUT,
        ),
        ("claude", notification.to_string(), HOOK_TIMEOUT),
        ("claude", stop.to_string(), HOOK_TIMEOUT),
        ("claude", end.to_string(), HOOK_TIMEOUT),
        ("hermes", hermes_turn(&p.root, 64 * 1024), HOOK_TIMEOUT),
        ("claude", p.start("s2", "resume"), Duration::from_secs(60)),
    ];
    let mut replies = Vec::new();
    for (provider, input, budget) in hooks {
        let env = [
            ("DISPATCH_REGISTRATION_BUDGET_MS", Path::new("2000")),
            ("TERMINAL_CWD", &p.worktree),
        ];
        let (output, took) = p.hook(provider, input.as_bytes(), &env);
        assert!(took < budget, "{input:.80} took {took:?}");
        replies.push(Project::printed(&output));
    }
    let hooks_done = chrono::Utc::now();
    assert_eq!(
        context(&replies[0], "PostToolUse").as_deref(),
        Some("Work 01ABC (claude) landed fn f in src/lib.rs.")
    );
    for reply in &replies[1..5] {
        assert_eq!(reply, &Value::Null, "nothing reported, and no failure");
    }
    drop(busy);
    db.execute_batch("COMMIT").unwrap();
    drop(db);

    // None of it was recorded by a hook; the owner records all of it.
    assert!(p.events(&id, "runtime.session_ended").is_empty());
    assert!(p.events(&id, "runtime.needs_you").is_empty());
    p.owner("recorded the marks", |p| {
        p.events(&id, "runtime.session_ended").len() == 1
            && p.events(&id, "runtime.needs_you").len() == 2
            && p.events(&id, "note.delivered").len() == 1
    });
    let run = p.run(&id);
    let session = &run["attachment"]["sessions"][0];
    assert_eq!(session["session_id"], "s1");
    assert_eq!(session["end_reason"], "prompt_input_exit");
    let ended: chrono::DateTime<chrono::Utc> =
        session["ended_at"].as_str().unwrap().parse().unwrap();
    assert!(ended < hooks_done, "the end is when the hook saw it");
    let kinds: Vec<Value> = p
        .events(&id, "runtime.needs_you")
        .iter()
        .map(|event| event["kind"].clone())
        .collect();
    assert_eq!(
        kinds,
        ["permission", "turn_ended"],
        "in the order they came"
    );
    assert!(
        fs::read_dir(p.state.join("marks"))
            .unwrap()
            .next()
            .is_none()
    );
}
