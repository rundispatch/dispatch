//! The project owner's notes, end to end: two Claude Code worktree sessions
//! (stand-ins sending the P0 probes' payloads, paths replaced) change one
//! declaration, and each hears of the other once, through its hook. One
//! lands; the other then hears of the landing, with the source commit
//! recorded for it. The project is watched only while the test runs its
//! owner, and the test holds its serve lock otherwise.
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

const LIB: &str = "pub fn f() -> i32 {\n    1\n}\n\npub fn g() -> i32 {\n    2\n}\n";

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

fn key(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
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
        let name = format!("serve-{}.lock", key(self.root.to_str().unwrap()));
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
        self.until("the serve lock is free", |p| {
            let probe = fs::File::open(p.serve_lock()).unwrap();
            // SAFETY: a non-blocking probe on a descriptor this test owns.
            unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
        });
        self.ok(&["start"]);
    }

    fn stop_owner(&mut self) {
        self.ok(&["stop"]);
        self.hold_watch();
    }

    fn until(&self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(300);
        while !done(self) {
            assert!(Instant::now() < deadline, "never: {what}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&self.state)
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap();
        assert!(output.status.success(), "dispatch {args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// `dispatch hook claude` with `input`, and what it printed.
    fn claude(&self, input: &str) -> Value {
        let mut child = Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&self.state)
            .args(["hook", "claude"])
            .current_dir(&self.root)
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

    /// A worktree whose Claude Code session started, and its Work.
    fn session(&self, name: &str) -> (PathBuf, String) {
        let worktree = format!(".claude/worktrees/{name}");
        git(
            &self.root,
            &["worktree", "add", "--quiet", "-b", name, &worktree],
        );
        let worktree = fs::canonicalize(self.root.join(worktree)).unwrap();
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

    /// What a `PostToolUse` hook after a tool call in `cwd` gave the agent.
    fn tool(&self, cwd: &Path) -> Option<String> {
        let reply = self.claude(&payload(&[
            ("session_id", "d8e3f50b-7e08-4064-bae3-d75790d0d970".into()),
            ("cwd", cwd.to_str().unwrap().into()),
            ("hook_event_name", "PostToolUse".into()),
            ("tool_name", "Bash".into()),
            ("tool_input", serde_json::json!({"command": "ls"})),
            (
                "tool_response",
                serde_json::json!({"stdout": "", "stderr": "", "interrupted": false}),
            ),
        ]));
        let output = &reply["hookSpecificOutput"];
        (!output.is_null()).then(|| {
            assert_eq!(output["hookEventName"], "PostToolUse");
            output["additionalContext"].as_str().unwrap().to_owned()
        })
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

    fn sent(&self, id: &str) -> PathBuf {
        self.state.join("inbox/sent").join(format!("{id}.json"))
    }

    /// Whether the owner's view lists `id` with the patch it finished with.
    fn lists_frozen(&self, id: &str) -> bool {
        let run: Value = serde_json::from_slice(
            &fs::read(self.state.join("runs").join(id).join("metadata.json")).unwrap(),
        )
        .unwrap();
        let patch = fs::read(run["candidates"][0]["diff_path"].as_str().unwrap()).unwrap();
        let digest = hex::encode(Sha256::digest(patch));
        let Some(view) = fs::read_dir(self.state.join("watchers"))
            .unwrap()
            .filter_map(|entry| Some(entry.ok()?.path()))
            .find(|path| path.to_string_lossy().ends_with(".interactions.json"))
            .and_then(|path| serde_json::from_slice::<Value>(&fs::read(path).ok()?).ok())
        else {
            return false;
        };
        view["participants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["run_id"] == id && p["delta_sha256"] == digest.as_str())
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&self.state)
            .arg("stop")
            .current_dir(&self.root)
            .output();
    }
}

/// Whether `line` is `rest` after run `id` as Dispatch shortens it: its
/// shortest unique prefix, at least eight characters.
fn names(line: &str, id: &str, rest: &str) -> bool {
    line.strip_suffix(rest)
        .is_some_and(|short| short.len() >= 8 && id.starts_with(short))
}

/// A JSON object with `fields` in this order, as the runtimes send them.
fn payload(fields: &[(&str, Value)]) -> String {
    let fields: Vec<String> = fields
        .iter()
        .map(|(name, value)| format!("{}: {value}", Value::from(*name)))
        .collect();
    format!("{{{}}}", fields.join(", "))
}

#[test]
fn two_sessions_on_one_declaration_hear_of_each_other_and_of_the_landing() {
    let mut p = Project::new();
    let (wa, a) = p.session("a");
    let (wb, b) = p.session("b");
    fs::write(wa.join("src/lib.rs"), LIB.replace("    1\n", "    10\n")).unwrap();
    fs::write(wb.join("src/lib.rs"), LIB.replace("    1\n", "    20\n")).unwrap();

    p.start_owner();
    p.until("both Works have a note", |p| {
        [&a, &b]
            .iter()
            .all(|id| !p.events(id, "note.written").is_empty())
    });
    for (worktree, other) in [(&wb, &a), (&wa, &b)] {
        let heard = p.tool(&worktree.join("src")).expect("one note");
        let line = " (Claude Code): both change f (src/lib.rs)";
        assert!(heard.starts_with("Dispatch: other Work touches this Work ("));
        assert!(heard.lines().any(|l| names(l, other, line)), "{heard}");
        assert_eq!(p.tool(worktree), None, "heard once");
    }

    p.ok(&["finish", &a]);
    p.until("the owner lists A's frozen patch", |p| p.lists_frozen(&a));
    p.ok(&["accept", &a]);
    p.until("A is forgotten", |p| !p.sent(&a).exists());
    p.stop_owner();
    assert_eq!(
        p.events(&b, "note.written").len(),
        1,
        "B's window is not open"
    );

    // Ten minutes pass for B: its last note is moved back by that much.
    let mut kept: Value = serde_json::from_slice(&fs::read(p.sent(&b)).unwrap()).unwrap();
    let last: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(kept["sent"]["last_note_at"].clone()).unwrap();
    kept["sent"]["last_note_at"] = serde_json::json!(last - chrono::Duration::minutes(10));
    fs::write(p.sent(&b), kept.to_string()).unwrap();

    // A new owner, which hears of the landing only from what was recorded.
    p.start_owner();
    p.until("B has a second note", |p| {
        p.events(&b, "note.written").len() == 2
    });
    p.stop_owner();
    let sources: Value = serde_json::from_slice(
        &fs::read(
            p.state
                .join("projects")
                .join(key(p.root.to_str().unwrap()))
                .join("sources.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(sources[0]["landed_run"], a.as_str());
    let commit = sources[0]["commit"].as_str().unwrap();
    let heard = p.tool(&wb).expect("the landing note");
    let lines: Vec<&str> = heard.lines().skip(1).collect();
    assert_eq!(lines.len(), 2, "{heard}");
    let landed = " (Claude Code), landed: both change f (src/lib.rs)";
    assert!(names(lines[0], &a, landed), "{heard}");
    assert_eq!(
        lines[1],
        format!(
            "The source with this change is commit {} (refs/dispatch/source).",
            &commit[..8]
        )
    );
    assert_eq!(p.tool(&wa), None, "A, applied, hears nothing more");
}
