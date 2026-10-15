//! Hermes worktrees end to end, in a project watched by `dispatch start`.
//! The stand-in for Hermes is a shell the test gives one command at a time.
//! It does what Hermes Agent 0.18.2 does with `hermes chat --worktree`:
//! `.worktrees/hermes-<8 hex>` on branch `hermes/hermes-<same>`, locked
//! `hermes pid=<its pid>`; at the session's end it runs its
//! `on_session_finalize` hook as a child, then removes the worktree and its
//! branch unless it holds unpushed commits.
#![cfg(unix)]

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio},
    time::{Duration, Instant},
};

use serde_json::Value;

const ID: &str = "0a1b2c3d";

struct Project {
    temp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    home: PathBuf,
}

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

/// A Git repository at `dir` with one commit.
fn repository(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/lib.rs"), "pub fn f() -> i32 {\n    1\n}\n").unwrap();
    fs::write(dir.join("dispatch.yml"), "coherence:\n  poll_secs: 1\n").unwrap();
    git(dir, &["init", "--quiet"]);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "--quiet", "-m", "initial"]);
    fs::canonicalize(dir).unwrap()
}

impl Project {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = repository(&temp.path().join("project"));
        let home = temp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        Self {
            root,
            state: temp.path().join("state"),
            home: fs::canonicalize(home).unwrap(),
            temp,
        }
    }

    fn dispatch(&self, args: &[&str]) -> Output {
        Command::new(assert_cmd::cargo_bin!("dispatch"))
            .env("HOME", &self.home)
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
        let watchers = self.state.join("watchers");
        wait_until("the first scan", || {
            fs::read_dir(&watchers).is_ok_and(|entries| {
                entries.flatten().any(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .ends_with(".inventory.json")
                })
            })
        });
    }

    fn worktree(&self) -> PathBuf {
        self.root.join(".worktrees").join(format!("hermes-{ID}"))
    }

    fn runs_in(&self, worktree: &Path) -> Vec<Value> {
        let Ok(entries) = fs::read_dir(self.state.join("runs")) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| fs::read(entry.unwrap().path().join("metadata.json")).ok())
            .map(|bytes| serde_json::from_slice::<Value>(&bytes).unwrap())
            .filter(|run| run["attachment"]["workspace"].as_str() == worktree.to_str())
            .collect()
    }

    /// The one Work in `worktree`.
    fn run(&self, worktree: &Path) -> Value {
        let [run] = self.runs_in(worktree).try_into().unwrap();
        run
    }

    fn events(&self, id: &str) -> Vec<String> {
        fs::read_to_string(self.state.join("runs").join(id).join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["event_type"].to_string())
            .collect()
    }

    /// A real-shaped Hermes hook payload, as a file the stand-in pipes in.
    fn payload(&self, event: &str, reason: &str, cwd: &Path) -> PathBuf {
        let payload = serde_json::json!({
            "hook_event_name": event, "tool_name": null, "tool_input": null,
            "session_id": "20261015_134730_695362", "cwd": cwd,
            "extra": {"platform": "cli", "reason": reason,
                      "telemetry_schema_version": "hermes.observer.v1"},
        });
        let path = self.temp.path().join(format!("{event}-{reason}.json"));
        fs::write(&path, payload.to_string()).unwrap();
        path
    }

    /// What the stand-in runs for its hook: the payload on stdin, the
    /// worktree in `TERMINAL_CWD`.
    fn hook_line(&self, payload: &Path, worktree: &Path) -> String {
        format!(
            "cat '{}' | TERMINAL_CWD='{}' '{}' --state-dir '{}' hook hermes",
            payload.display(),
            worktree.display(),
            assert_cmd::cargo_bin!("dispatch").display(),
            self.state.display()
        )
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = self.dispatch(&["stop"]);
    }
}

/// The stand-in Hermes process: a shell reading commands from the test.
struct Hermes {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Hermes {
    fn new(p: &Project, dir: &Path) -> Self {
        let mut child = Command::new("sh")
            .arg("-s")
            .current_dir(dir)
            .env("HOME", &p.home)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Run `script` in the stand-in: whether it succeeded, and its output.
    fn run(&mut self, script: &str) -> (bool, String) {
        write!(self.stdin, "{{\n{script}\n}} 2>&1\necho \"__done $?\"\n").unwrap();
        self.stdin.flush().unwrap();
        let mut output = String::new();
        loop {
            let mut line = String::new();
            assert_ne!(self.stdout.read_line(&mut line).unwrap(), 0, "{output}");
            if let Some(status) = line.strip_prefix("__done ") {
                return (status.trim() == "0", output);
            }
            output.push_str(&line);
        }
    }

    fn ok(&mut self, script: &str) -> String {
        let (ok, output) = self.run(script);
        assert!(ok, "{script}: {output}");
        output
    }

    /// Make the worktree as Hermes does, in the repository it runs in.
    fn make_worktree(&mut self) {
        self.ok(&format!(
            "grep -qx '.worktrees/' .gitignore 2>/dev/null || echo '.worktrees/' >> .gitignore\n\
             git worktree add --quiet .worktrees/hermes-{ID} -b hermes/hermes-{ID}\n\
             git worktree lock --reason \"hermes pid=$$\" .worktrees/hermes-{ID}"
        ));
    }

    /// Remove the worktree as Hermes does when it holds no unpushed commits.
    fn remove_worktree(&mut self) {
        self.ok(&format!(
            "git worktree unlock .worktrees/hermes-{ID}\n\
             git worktree remove .worktrees/hermes-{ID} --force\n\
             git branch -D hermes/hermes-{ID}"
        ));
    }

    /// The session's end: the shell exits, and with it the process the lock names.
    fn exit(mut self) {
        drop(self.stdin);
        self.child.wait().unwrap();
    }
}

/// Wait for the stand-in's worktree to become Work with the stand-in
/// recorded as its running agent; its id.
fn discovered(p: &Project, hermes: &Hermes) -> String {
    let worktree = p.worktree();
    wait_until("the Hermes worktree's Work", || {
        p.runs_in(&worktree)
            .first()
            .is_some_and(|run| run["attachment"]["agent_process"]["pid"] == hermes.pid())
    });
    p.run(&worktree)["id"].as_str().unwrap().to_owned()
}

#[test]
fn a_hermes_worktree_becomes_work_whose_agent_runs_and_then_is_idle() {
    let p = Project::new();
    p.watch();
    let base = git(&p.root, &["rev-parse", "HEAD"]);
    let mut hermes = Hermes::new(&p, &p.root);
    hermes.make_worktree();
    let id = discovered(&p, &hermes);

    let run = p.run(&p.worktree());
    let attachment = &run["attachment"];
    assert_eq!(attachment["agent"], "hermes");
    assert_eq!(attachment["workspace_owner"], "runtime");
    assert_eq!(
        attachment["provenance"],
        serde_json::json!({"discovered": {"commit": base}})
    );
    assert_eq!(run["task"], format!("hermes session in hermes-{ID}"));
    let session = &attachment["sessions"][0];
    assert_eq!(session["provider"], "hermes");
    assert_eq!(session["session_id"], format!("hermes-{ID}"));
    assert_eq!(session["source"], "discovered");
    assert_eq!(session["ended_at"], Value::Null);
    let status = text(&p.dispatch(&["status", &id]));
    assert!(
        status.contains(&format!("agent running (pid {})", hermes.pid())),
        "{status}"
    );
    assert!(!status.contains("not observable"), "{status}");
    assert_eq!(state_word(&status), "working", "{status}");

    hermes.exit();
    // Liveness records the end, then lets go of the agent: wait for both.
    wait_until("the session's end", || {
        let run = p.run(&p.worktree());
        run["attachment"]["sessions"][0]["end_reason"] == "process_gone"
            && run["attachment"]["agent_process"].is_null()
    });
    let run = p.run(&p.worktree());
    assert_eq!(run["outcome"]["lifecycle"], "working");
    let status = text(&p.dispatch(&["status", &id]));
    assert!(!status.contains("agent running"), "{status}");
    assert_eq!(state_word(&status), "idle", "{status}");
}

/// The Work's state, as `dispatch status` shows it.
fn state_word(status: &str) -> &str {
    let line = status.lines().find(|line| line.starts_with("Status "));
    line.and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
}

/// The 0.4.14 trial's loss: Hermes removed its worktree, uncommitted change
/// and all. Its `on_session_finalize` hook keeps the exact Δ first.
#[test]
fn the_finalize_hook_keeps_the_exact_changes_before_hermes_removes_the_worktree() {
    let p = Project::new();
    p.watch();
    let mut hermes = Hermes::new(&p, &p.root);
    hermes.make_worktree();
    let id = discovered(&p, &hermes);
    let worktree = p.worktree();
    fs::write(worktree.join("notes.txt"), "two\n").unwrap();

    let payload = p.payload("on_session_finalize", "shutdown", &p.root);
    let output = hermes.ok(&p.hook_line(&payload, &worktree));
    assert_eq!(output, "", "the hook prints nothing on success");
    let run = p.run(&worktree);
    assert_eq!(
        run["attachment"]["workspace_removed"]["exact"], true,
        "{run}"
    );
    let patch = fs::read_to_string(p.state.join("runs").join(&id).join("delta.patch")).unwrap();
    assert!(patch.contains("+two"), "{patch}");

    hermes.remove_worktree();
    hermes.exit();
    assert!(!worktree.exists());
    let run = p.run(&worktree);
    assert_eq!(run["attachment"]["workspace_removed"]["exact"], true);
    assert_eq!(run["outcome"]["lifecycle"], "working");
}

/// With no remote-tracking refs Hermes counts no commit as unpushed, so it
/// removes even committed work: that is kept exactly too.
#[test]
fn a_committed_change_without_remotes_is_kept_exactly() {
    let p = Project::new();
    p.watch();
    let mut hermes = Hermes::new(&p, &p.root);
    hermes.make_worktree();
    let id = discovered(&p, &hermes);
    let worktree = p.worktree();
    fs::write(
        worktree.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    git(&worktree, &["commit", "--quiet", "-am", "agent"]);

    let payload = p.payload("on_session_finalize", "shutdown", &p.root);
    hermes.ok(&p.hook_line(&payload, &worktree));
    assert_eq!(
        p.run(&worktree)["attachment"]["workspace_removed"]["exact"],
        true
    );
    let patch = fs::read_to_string(p.state.join("runs").join(&id).join("delta.patch")).unwrap();
    assert!(patch.contains("+    2"), "{patch}");
    hermes.remove_worktree();
    hermes.exit();
}

/// With an unpushed commit Hermes keeps the worktree: only the session's
/// end is recorded, and the worktree is left as it is.
#[test]
fn an_unpushed_commit_keeps_the_worktree_and_ends_only_the_session() {
    let p = Project::new();
    git(&p.root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    p.watch();
    let mut hermes = Hermes::new(&p, &p.root);
    hermes.make_worktree();
    let id = discovered(&p, &hermes);
    let worktree = p.worktree();
    fs::write(worktree.join("notes.txt"), "kept\n").unwrap();
    git(&worktree, &["add", "-A"]);
    git(&worktree, &["commit", "--quiet", "-m", "agent"]);
    let head = git(&worktree, &["rev-parse", "HEAD"]);

    let payload = p.payload("on_session_finalize", "shutdown", &p.root);
    hermes.ok(&p.hook_line(&payload, &worktree));
    let run = p.run(&worktree);
    assert_eq!(run["attachment"]["workspace_removed"], Value::Null, "{run}");
    assert_eq!(run["attachment"]["sessions"][0]["end_reason"], "shutdown");
    assert_eq!(run["outcome"]["lifecycle"], "working");
    assert!(
        p.events(&id)
            .contains(&"\"runtime.session_ended\"".to_owned())
    );
    assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&worktree, &["status", "--porcelain"]), "");
    let listed = git(&p.root, &["worktree", "list", "--porcelain"]);
    assert!(
        listed.contains(&format!("locked hermes pid={}", hermes.pid())),
        "{listed}"
    );
    hermes.exit();
}

/// Hook input is untrusted, and `on_session_finalize` is not always the end:
/// none of these touches the Work.
#[test]
fn any_other_payload_or_worktree_is_silent_and_records_nothing() {
    let p = Project::new();
    p.watch();
    let mut hermes = Hermes::new(&p, &p.root);
    hermes.make_worktree();
    let id = discovered(&p, &hermes);
    let worktree = p.worktree();
    fs::write(worktree.join("notes.txt"), "two\n").unwrap();

    // A Hermes-shaped worktree of another repository.
    let other = repository(&p.temp.path().join("other"));
    let mut elsewhere = Hermes::new(&p, &other);
    elsewhere.make_worktree();
    let theirs = other.join(".worktrees").join(format!("hermes-{ID}"));
    // A worktree of this repository, but not Hermes's.
    git(
        &p.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "mine",
            ".worktrees/mine",
        ],
    );
    let mine = p.root.join(".worktrees/mine");

    let finalize = p.payload("on_session_finalize", "shutdown", &p.root);
    let finalize_other = p.payload("on_session_finalize", "shutdown_other", &other);
    fs::write(
        &finalize_other,
        fs::read_to_string(&finalize_other)
            .unwrap()
            .replace("shutdown_other", "shutdown"),
    )
    .unwrap();
    // What a hook could record; the owner's own ticks go on meanwhile.
    let recorded = || {
        let run = p.run(&worktree);
        let events: Vec<String> = p
            .events(&id)
            .into_iter()
            .filter(|kind| kind.contains("runtime.") || kind.contains("workspace."))
            .collect();
        (
            run["attachment"]["sessions"].clone(),
            run["attachment"]["workspace_removed"].clone(),
            events,
        )
    };
    let before = recorded();
    assert_eq!(before.1, Value::Null);
    for (payload, terminal_cwd) in [
        (&finalize, &theirs),
        (&finalize_other, &worktree),
        (&finalize, &mine),
        (&finalize, &p.root),
        (&p.payload("on_session_end", "shutdown", &p.root), &worktree),
        // `/new`: the worktree and its process live on.
        (
            &p.payload("on_session_finalize", "session_boundary", &p.root),
            &worktree,
        ),
    ] {
        let output = hermes.ok(&p.hook_line(payload, terminal_cwd));
        assert_eq!(output, "", "{payload:?} {terminal_cwd:?}");
    }
    // Not run by the process that locked the worktree, which removes it.
    let output = elsewhere.ok(&p.hook_line(&finalize, &worktree));
    assert_eq!(output, "");
    let direct = Command::new(assert_cmd::cargo_bin!("dispatch"))
        .arg("--state-dir")
        .arg(&p.state)
        .args(["hook", "hermes"])
        .env("TERMINAL_CWD", &worktree)
        .stdin(fs::File::open(&finalize).unwrap())
        .output()
        .unwrap();
    assert!(direct.status.success(), "{}", text(&direct));
    assert_eq!(text(&direct), "");
    assert_eq!(recorded(), before);
    assert!(p.runs_in(&theirs).is_empty() && p.runs_in(&mine).is_empty());

    // Unreadable input fails the hook, with the reason.
    let (ok, output) = hermes.run(&format!(
        "echo '{{not json' | TERMINAL_CWD='{}' '{}' --state-dir '{}' hook hermes",
        worktree.display(),
        assert_cmd::cargo_bin!("dispatch").display(),
        p.state.display()
    ));
    assert!(!ok && output.contains("unreadable hook input"), "{output}");
    assert_eq!(recorded(), before);
    elsewhere.exit();
    hermes.exit();
}

/// Hermes's lock is fail-soft: a worktree it never locked names no process,
/// so once it is past its first moments it is listed, never tracked.
#[test]
fn a_hermes_worktree_without_its_lock_is_listed_not_tracked() {
    let p = Project::new();
    p.watch();
    let mut hermes = Hermes::new(&p, &p.root);
    hermes.ok(&format!(
        "git worktree add --quiet .worktrees/hermes-{ID} -b hermes/hermes-{ID}"
    ));
    let worktree = p.worktree();
    let made = std::time::SystemTime::now() - Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(worktree.join(".git"))
        .unwrap()
        .set_modified(made)
        .unwrap();
    let watchers = p.state.join("watchers");
    wait_until("the worktree listed", || {
        fs::read_dir(&watchers).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with(".inventory.json")
                && fs::read_to_string(entry.path())
                    .unwrap_or_default()
                    .contains(&format!("hermes-{ID}"))
        })
    });
    assert!(p.runs_in(&worktree).is_empty());
    hermes.exit();
}
