//! "Waiting on you" (contract D): Claude Code's `Notification` and `Stop`
//! hooks mark the Work of their workspace, `watch` and `status` show it, and
//! the owner clears it when the session moves again. The payloads are shaped
//! as Claude Code 2.1.296 sends them (the P0 probe), with paths replaced.
#![cfg(unix)]

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    time::{Duration, Instant, SystemTime},
};

use serde_json::Value;

/// The session of the probe's `claude --worktree probe-a`.
const SESSION: &str = "96fce6bd-fe23-4b1c-bd1e-b171635c421b";

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

/// Set `path`'s modification time, as a write at `at` would.
fn written_at(path: &Path, at: SystemTime) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(at)
        .unwrap();
}

struct Project {
    _temp: tempfile::TempDir,
    root: PathBuf,
    worktree: PathBuf,
    state: PathBuf,
    /// The session's transcript, last written long before any mark.
    transcript: PathBuf,
}

impl Project {
    /// A project with a worktree under `.claude/worktrees/x`. It carries no
    /// `CLAUDE_BASE`, so no lock decides when its session ends: the hooks do.
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
        let transcript = fs::canonicalize(temp.path())
            .unwrap()
            .join(format!("{SESSION}.jsonl"));
        fs::write(&transcript, "{}\n").unwrap();
        written_at(
            &transcript,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1 << 30),
        );
        Self {
            root: fs::canonicalize(&root).unwrap(),
            worktree: fs::canonicalize(root.join(".claude/worktrees/x")).unwrap(),
            state: temp.path().join("state"),
            transcript,
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
        assert!(output.status.success(), "{}", text(&output));
    }

    /// Run the hook; it must succeed and reply nothing unless `replies`.
    fn hook(&self, event: Value) -> String {
        let mut child = Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&self.state)
            .args(["hook", "claude"])
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

    fn common(&self, event: &str) -> Value {
        serde_json::json!({
            "session_id": SESSION,
            "transcript_path": self.transcript,
            "cwd": self.worktree,
            "scratchpad_dir": format!("/tmp/scratch/{SESSION}/scratchpad"),
            "prompt_id": "7e92d8c0-ed66-464a-845b-98f1cc4cff83",
            "hook_event_name": event,
        })
    }

    /// The Work a `SessionStart` registered.
    fn registered(&self) -> String {
        let mut start = self.common("SessionStart");
        start["source"] = "startup".into();
        start["model"] = "claude-opus-5-5".into();
        let reply = self.hook(start);
        assert!(reply.contains("tracking this worktree"), "{reply}");
        let runs = fs::read_dir(self.state.join("runs")).unwrap();
        let ids: Vec<String> = runs
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(ids.len(), 1, "{ids:?}");
        ids[0].clone()
    }

    /// `Notification` as the probe recorded it. Never replied to.
    fn notification(&self, kind: &str, message: &str) {
        let mut event = self.common("Notification");
        event["notification_type"] = kind.into();
        event["message"] = message.into();
        assert_eq!(self.hook(event), "", "{kind}");
    }

    /// `Stop` as the probe recorded it. Never replied to.
    fn stop(&self) {
        let mut event = self.common("Stop");
        event["permission_mode"] = "default".into();
        event["effort"] = serde_json::json!({"level": "high"});
        event["stop_hook_active"] = false.into();
        event["last_assistant_message"] = "done".into();
        event["background_tasks"] = serde_json::json!([]);
        event["session_crons"] = serde_json::json!([]);
        assert_eq!(self.hook(event), "");
    }

    fn run(&self, id: &str) -> Value {
        let path = self.state.join("runs").join(id).join("metadata.json");
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn mark(&self, id: &str) -> Value {
        self.run(id)["attachment"]["needs_you"].clone()
    }

    fn events(&self, id: &str, kind: &str) -> Vec<Value> {
        fs::read_to_string(self.state.join("runs").join(id).join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event["event_type"] == kind)
            .collect()
    }

    fn status(&self, id: &str) -> String {
        let output = self.dispatch(&["status", id]);
        assert!(output.status.success(), "{}", text(&output));
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Wait for the owner to clear the mark, and say what cleared it.
    fn cleared(&self, id: &str) -> String {
        wait_until("the mark cleared", || self.mark(id).is_null());
        let cleared = self.events(id, "runtime.needs_you_cleared");
        assert_eq!(cleared.len(), 1, "{cleared:?}");
        cleared[0]["payload"]["by"].as_str().unwrap().to_owned()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = self.dispatch(&["stop"]);
    }
}

#[test]
fn each_hook_marks_its_kind_and_status_says_waiting_on_you() {
    let p = Project::new();
    p.watch();
    let id = p.registered();
    assert!(!p.status(&id).contains("waiting on you"));

    p.notification("permission_prompt", "Claude needs your permission");
    let mark = p.mark(&id);
    assert_eq!(mark["kind"], "permission", "{mark}");
    assert_eq!(mark["message"], "Claude needs your permission");
    assert_eq!(mark["transcript"], p.transcript.to_str().unwrap());
    let status = p.status(&id);
    assert!(
        status.contains("waiting on you: permission · Claude needs your permission"),
        "{status}"
    );
    let [event] = &p.events(&id, "runtime.needs_you")[..] else {
        panic!("one mark recorded");
    };
    for field in ["kind", "message", "at", "transcript"] {
        assert!(!event["payload"][field].is_null(), "{field}: {event}");
    }

    p.notification("idle_prompt", "Claude is waiting for your input");
    assert_eq!(p.mark(&id)["kind"], "input");
    assert!(p.status(&id).contains("waiting on you: input"));

    p.stop();
    assert_eq!(p.mark(&id)["kind"], "turn_ended");
    assert!(p.status(&id).contains("waiting on you: turn ended"));

    // Any other notification is not a wait, and leaves the mark as it was.
    let before = p.mark(&id);
    p.notification("auth_success", "Authenticated");
    assert_eq!(p.mark(&id), before);
    assert_eq!(p.events(&id, "runtime.needs_you").len(), 3);
    // A new mark replaced the old each time; none was cleared.
    assert!(p.events(&id, "runtime.needs_you_cleared").is_empty());
}

#[test]
fn a_write_to_the_transcript_clears_the_mark() {
    let p = Project::new();
    p.watch();
    let id = p.registered();
    p.notification("permission_prompt", "Claude needs your permission");
    // The person answered: the tool runs and Claude writes its result.
    written_at(&p.transcript, SystemTime::now() + Duration::from_secs(3600));
    assert_eq!(p.cleared(&id), "transcript");
    assert!(!p.status(&id).contains("waiting on you"));
}

#[test]
fn a_session_end_clears_the_mark() {
    let p = Project::new();
    p.watch();
    let id = p.registered();
    p.notification("idle_prompt", "Claude is waiting for your input");
    let mut end = p.common("SessionEnd");
    end["reason"] = "clear".into();
    assert_eq!(p.hook(end), "");
    // Shown no more from the moment the session ended.
    assert!(!p.status(&id).contains("waiting on you"));
    assert_eq!(p.cleared(&id), "session_end");
    assert_eq!(p.run(&id)["outcome"]["lifecycle"], "working");
}

/// `dispatch watch --json`, its lines read as they come.
struct Watch {
    child: Child,
    lines: mpsc::Receiver<Value>,
}

impl Watch {
    fn start(p: &Project) -> Self {
        let mut child = Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&p.state)
            .args(["watch", "--json"])
            .current_dir(&p.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                let _ = sender.send(serde_json::from_str(&line).unwrap());
            }
        });
        Self { child, lines }
    }

    /// The next Work line for `id`.
    fn next_work(&self, id: &str) -> Value {
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(300))
                .expect("a Work line");
            if line["type"] == "work" && line["run_id"] == id {
                return line;
            }
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn watch_json_emits_a_work_line_when_the_mark_changes_and_when_it_clears() {
    let p = Project::new();
    p.watch();
    let id = p.registered();
    let watch = Watch::start(&p);
    assert_eq!(watch.next_work(&id)["needs_you"], Value::Null);

    p.notification("permission_prompt", "Claude needs your permission");
    let line = watch.next_work(&id);
    let needs_you = &line["needs_you"];
    assert_eq!(needs_you["kind"], "permission", "{line}");
    assert_eq!(needs_you["message"], "Claude needs your permission");
    assert_eq!(needs_you["since"], p.mark(&id)["at"]);
    assert_eq!(line["state"], "working");

    p.stop();
    assert_eq!(watch.next_work(&id)["needs_you"]["kind"], "turn_ended");

    written_at(&p.transcript, SystemTime::now() + Duration::from_secs(3600));
    assert_eq!(watch.next_work(&id)["needs_you"], Value::Null);
    assert_eq!(p.cleared(&id), "transcript");
}

#[test]
fn a_notification_in_an_unwatched_project_touches_nothing() {
    let p = Project::new();
    p.notification("permission_prompt", "Claude needs your permission");
    p.stop();
    assert!(!p.state.join("dispatch.db").exists(), "no database");
    assert!(!p.state.join("runs").exists(), "no Work");
}
