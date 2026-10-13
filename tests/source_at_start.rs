//! Where the integration source differed from attached Work's S0 when the
//! Work began (`docs/plan-0.4.12.md` §4 B): observed once, in both
//! directions, by `attach` itself or by the project owner after a runtime's
//! registration, and never again. A workspace Dispatch makes begins from the
//! current source and gets no observation. The project (`root`) ignores
//! `build/`; a test plays each agent with direct edits.
#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

use serde_json::Value;

const TWO: &str = "pub fn f() -> i32 {\n    2\n}\n";

struct Project {
    temp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
}

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
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

impl Project {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn f() -> i32 {\n    1\n}\n").unwrap();
        fs::write(root.join(".gitignore"), "build/\n").unwrap();
        fs::write(root.join("dispatch.yml"), "coherence:\n  poll_secs: 1\n").unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
        Self {
            root: fs::canonicalize(&root).unwrap(),
            state: temp.path().join("state"),
            temp,
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

    fn ok(&self, args: &[&str]) -> String {
        let output = self.dispatch(args);
        assert!(
            output.status.success(),
            "dispatch {args:?}: {}{}",
            text(&output),
            String::from_utf8_lossy(&output.stderr)
        );
        text(&output)
    }

    /// A linked worktree of `root` on a new branch made from `base`.
    fn worktree(&self, name: &str, base: &str) -> PathBuf {
        let path = self.temp.path().join(name);
        git(
            &self.root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                name,
                path.to_str().unwrap(),
                base,
            ],
        );
        fs::canonicalize(path).unwrap()
    }

    /// `dispatch attach --workspace <workspace> --root <root>`: its Work ID.
    fn attach_to(&self, workspace: &Path, root: &Path) -> String {
        self.attach_banner(workspace, root).0
    }

    /// The Work ID, and the banner's `Source` line with its continuations.
    fn attach_banner(&self, workspace: &Path, root: &Path) -> (String, String) {
        let out = self.ok(&[
            "attach",
            "--workspace",
            workspace.to_str().unwrap(),
            "--root",
            root.to_str().unwrap(),
        ]);
        let id = out
            .lines()
            .find_map(|line| line.strip_prefix("ATTACHED "))
            .expect("attach prints ATTACHED <id>")
            .to_owned();
        let mut lines = out.lines().skip_while(|line| !line.starts_with("Source "));
        let mut banner: Vec<&str> = lines.next().into_iter().collect();
        banner.extend(lines.take_while(|line| line.starts_with("    ")));
        (id, banner.join("\n"))
    }

    fn attach(&self, workspace: &Path) -> String {
        self.attach_to(workspace, &self.root)
    }

    /// Work A, attached in its own worktree, changes `src/lib.rs` and is
    /// accepted: the root holds A's change, uncommitted.
    fn accepted_a(&self) -> String {
        let wt = self.worktree("a", "HEAD");
        let a = self.attach(&wt);
        fs::write(wt.join("src/lib.rs"), TWO).unwrap();
        self.ok(&["finish", &a]);
        self.ok(&["accept", &a]);
        assert_eq!(
            fs::read_to_string(self.root.join("src/lib.rs")).unwrap(),
            TWO
        );
        assert_eq!(git(&self.root, &["status", "--porcelain"]), "M src/lib.rs");
        a
    }

    fn metadata(&self, id: &str) -> Value {
        let path = self.state.join("runs").join(id).join("metadata.json");
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn at_start(&self, id: &str) -> Value {
        self.metadata(id)["attachment"]["source_at_start"].clone()
    }

    fn observed(&self, id: &str) -> Value {
        let at_start = self.at_start(id);
        assert!(at_start["observed"].is_object(), "{at_start}");
        at_start["observed"].clone()
    }

    fn observations(&self, id: &str) -> i64 {
        rusqlite::Connection::open(self.state.join("dispatch.db"))
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_type = 'attach.source_observed'",
                [id],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// Replace the recorded observation in both projections, as an older
    /// Dispatch (`None`) or an owner gone before it looked would leave it.
    fn set_at_start(&self, id: &str, value: Option<Value>) {
        let edit = |run: &mut Value| {
            let attachment = run["attachment"].as_object_mut().unwrap();
            match &value {
                Some(value) => attachment.insert("source_at_start".into(), value.clone()),
                None => attachment.remove("source_at_start"),
            };
        };
        let path = self.state.join("runs").join(id).join("metadata.json");
        let mut metadata = self.metadata(id);
        edit(&mut metadata);
        fs::write(path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let db = rusqlite::Connection::open(self.state.join("dispatch.db")).unwrap();
        let projection: String = db
            .query_row(
                "SELECT run_projection_json FROM runs WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        let mut projection: Value = serde_json::from_str(&projection).unwrap();
        edit(&mut projection);
        db.execute(
            "UPDATE runs SET run_projection_json = ?1 WHERE id = ?2",
            (serde_json::to_string(&projection).unwrap(), id),
        )
        .unwrap();
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = self.dispatch(&["stop"]);
    }
}

/// The `source at start` lines of `status`, and nothing else.
fn at_start_lines(status: &str) -> String {
    let mut lines = status
        .lines()
        .skip_while(|line| !line.starts_with("  source at start:"));
    let mut shown: Vec<&str> = lines.next().into_iter().collect();
    shown.extend(lines.take_while(|line| line.starts_with("    ")));
    shown.join("\n")
}

/// Accepted, uncommitted A, then a managed launch B: B's S0 holds A, its Δ
/// does not, and nothing is observed or said about the source at its start.
#[test]
fn a_managed_launch_after_accepted_uncommitted_work_begins_from_it_and_says_nothing() {
    let p = Project::new();
    p.accepted_a();
    let out = p.ok(&[
        "attach",
        "--root",
        p.root.to_str().unwrap(),
        "--agent",
        "fixture",
        "--",
        "/bin/sh",
        "-c",
        "printf 'b\\n' > b.txt",
    ]);
    let b = out
        .lines()
        .find_map(|line| line.strip_prefix("Attached work "))
        .and_then(|rest| rest.split_whitespace().next())
        .expect("the wrapper names its Work")
        .to_owned();
    let metadata = p.metadata(&b);
    let baseline = PathBuf::from(metadata["baseline_path"].as_str().unwrap());
    let commit = metadata["baseline_commit"].as_str().unwrap();
    assert_eq!(
        git(&baseline, &["show", &format!("{commit}:src/lib.rs")]),
        TWO.trim()
    );
    let patch = fs::read_to_string(p.state.join("runs").join(&b).join("delta.patch")).unwrap();
    assert!(
        patch.contains("b.txt") && !patch.contains("src/lib.rs"),
        "{patch}"
    );

    assert!(
        metadata["attachment"].get("source_at_start").is_none(),
        "{metadata}"
    );
    assert_eq!(p.observations(&b), 0);
    let status = p.ok(&["status", &b]);
    assert!(!status.contains("source at start"), "{status}");
}

/// An external worktree made from HEAD while accepted A sits uncommitted in
/// the root, beside an edit nobody recorded: both paths are named, only A's
/// as A's. Recorded once: the source moving later changes nothing.
#[test]
fn an_external_worktree_from_older_source_warns_once_naming_applied_work() {
    let p = Project::new();
    let a = p.accepted_a();
    fs::write(p.root.join("notes.txt"), "by hand\n").unwrap();
    let wt = p.worktree("b", "HEAD");
    let (b, banner) = p.attach_banner(&wt, &p.root);

    let observed = p.observed(&b);
    assert_eq!(observed["differing"], 2, "{observed}");
    // Said at once, in the same words `status` uses below.
    let head = git(&p.root, &["rev-parse", "HEAD"]);
    assert!(
        banner.starts_with("Source     differed from its S0 at 2 paths (observed ")
            && banner.contains(&format!(
                ", source HEAD {}): notes.txt, src/lib.rs (in applied Work {a})\n",
                &head[..8]
            ))
            && banner.ends_with(
                "\n           Work started with dispatch attach -- <agent> begins from the current source, including uncommitted changes."
            ),
        "{banner}"
    );
    assert_eq!(banner.lines().count(), 2, "{banner}");
    assert_eq!(
        observed["paths"],
        serde_json::json!([
            {"path": "notes.txt"},
            {"path": "src/lib.rs", "applied_work": a},
        ])
    );
    assert_eq!(
        observed["source_head"],
        git(&p.root, &["rev-parse", "HEAD"])
    );
    assert_eq!(p.observations(&b), 1);

    let status = p.ok(&["status", &b]);
    let shown = at_start_lines(&status);
    assert!(
        shown.starts_with("  source at start: differed from its S0 at 2 paths (observed "),
        "{status}"
    );
    assert!(
        shown.contains(&format!("notes.txt, src/lib.rs (in applied Work {a})")),
        "{shown}"
    );
    assert!(
        shown.contains("Work started with dispatch attach -- <agent> begins from the current source, including uncommitted changes."),
        "{shown}"
    );
    // A worktree you made gets no runtime's advice.
    assert!(
        !status.contains("Claude") && !status.contains("baseRef"),
        "{status}"
    );
    let explain = p.ok(&["explain", &b]);
    assert!(explain.contains(&shown), "{explain}");

    // Later movement is the verdict's business, never the start's.
    fs::write(p.root.join("notes.txt"), "edited again\n").unwrap();
    fs::write(p.root.join("later.txt"), "later\n").unwrap();
    fs::write(wt.join("b.txt"), "b\n").unwrap();
    p.ok(&["finish", &b]);
    p.ok(&["check", &b]);
    assert_eq!(p.observed(&b), observed);
    assert_eq!(p.observations(&b), 1);
    assert_eq!(at_start_lines(&p.ok(&["status", &b])), shown);
}

/// A local commit the remote has not seen is missing from a worktree made
/// from the cached remote branch; with no remote, a worktree made from HEAD
/// of a clean source is recorded as the same, which is not "not observed".
#[test]
fn a_commit_ahead_of_a_cached_remote_warns_and_a_clean_head_without_one_is_the_same() {
    let p = Project::new();
    let (same, banner) = p.attach_banner(&p.worktree("plain", "HEAD"), &p.root);
    assert!(
        banner.starts_with("Source     the same as its S0 (observed ") && !banner.contains('\n'),
        "{banner}"
    );
    assert_eq!(p.observed(&same)["differing"], 0);
    assert_eq!(p.observed(&same)["paths"], serde_json::json!([]));
    let status = p.ok(&["status", &same]);
    assert!(
        at_start_lines(&status).starts_with("  source at start: the same as its S0 (observed "),
        "{status}"
    );

    let origin = p.temp.path().join("origin.git");
    git(
        p.temp.path(),
        &[
            "clone",
            "--quiet",
            "--bare",
            p.root.to_str().unwrap(),
            "origin.git",
        ],
    );
    git(
        &p.root,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&p.root, &["fetch", "--quiet", "origin"]);
    let branch = git(&p.root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    fs::write(p.root.join("src/local.rs"), "pub fn local() {}\n").unwrap();
    git(&p.root, &["add", "src/local.rs"]);
    git(&p.root, &["commit", "--quiet", "-m", "local, not pushed"]);

    let fresh = p.attach(&p.worktree("fresh", &format!("origin/{branch}")));
    let observed = p.observed(&fresh);
    assert_eq!(observed["differing"], 1, "{observed}");
    assert_eq!(
        observed["paths"],
        serde_json::json!([{"path": "src/local.rs"}])
    );
    assert_eq!(
        observed["source_head"],
        git(&p.root, &["rev-parse", "HEAD"])
    );
}

/// The integration root may itself be a linked worktree.
#[test]
fn a_linked_integration_root_is_compared_like_any_other() {
    let p = Project::new();
    let integration = p.worktree("integration", "HEAD");
    fs::write(integration.join("src/lib.rs"), TWO).unwrap();
    let wt = p.worktree("agent", "HEAD");
    let id = p.attach_to(&wt, &integration);
    let observed = p.observed(&id);
    assert_eq!(
        observed["paths"],
        serde_json::json!([{"path": "src/lib.rs"}])
    );
    assert_eq!(
        observed["source_head"],
        git(&integration, &["rev-parse", "HEAD"])
    );
}

/// Accepted A reverted by hand before B begins: nothing differs.
#[test]
fn reverted_accepted_changes_leave_nothing_to_warn_about() {
    let p = Project::new();
    p.accepted_a();
    git(&p.root, &["checkout", "--quiet", "--", "src/lib.rs"]);
    let b = p.attach(&p.worktree("b", "HEAD"));
    assert_eq!(p.observed(&b)["differing"], 0);
}

/// Ignored build output and a runtime worktree nested in the root, each with
/// a sealed directory that any walk would fail on: the observation succeeds
/// and names none of it.
#[test]
fn ignored_and_nested_build_churn_is_neither_shown_nor_walked() {
    let p = Project::new();
    let nested = p.root.join(".claude/worktrees/x");
    git(
        &p.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "x",
            nested.to_str().unwrap(),
        ],
    );
    let sealed = [p.root.join("build/sealed"), nested.join("target/sealed")];
    for dir in &sealed {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("artifact.o"), b"object\n").unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).unwrap();
    }
    fs::write(p.root.join("build/out.bin"), b"churn\n").unwrap();
    fs::write(nested.join("src/lib.rs"), TWO).unwrap();

    let id = p.attach(&p.worktree("b", "HEAD"));
    let at_start = p.at_start(&id);
    for dir in &sealed {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert_eq!(at_start["observed"]["differing"], 0, "{at_start}");
}

/// An observation that fails records nothing and shows "not observed", and
/// nothing later fills it in. Work from before observations shows nothing.
#[test]
fn a_failed_observation_shows_not_observed_and_older_work_shows_nothing() {
    let p = Project::new();
    let locked = p.root.join("locked.txt");
    fs::write(&locked, "unreadable\n").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let (id, banner) = p.attach_banner(&p.worktree("b", "HEAD"), &p.root);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o600)).unwrap();

    assert_eq!(banner, "Source     not observed");
    assert_eq!(p.at_start(&id), "not_observed");
    assert_eq!(p.observations(&id), 0);
    for _ in 0..2 {
        let status = p.ok(&["status", &id]);
        assert_eq!(at_start_lines(&status), "  source at start: not observed");
    }
    assert_eq!(p.at_start(&id), "not_observed");

    p.set_at_start(&id, None);
    let status = p.ok(&["status", &id]);
    assert!(!status.contains("source at start"), "{status}");
    assert!(!p.ok(&["explain", &id]).contains("source at start"));
}

/// Runtime-registered Work: the hook replies without a word about the
/// source; the project owner observes it once published. The observation
/// survives an owner restart unchanged, and an owner that starts after Work
/// was published never fills in one that is missing.
#[test]
fn the_owner_observes_runtime_work_after_publication_and_only_then() {
    let p = Project::new();
    let worktree = p.root.join(".claude/worktrees/x");
    git(
        &p.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "worktree-x",
            worktree.to_str().unwrap(),
        ],
    );
    let worktree = fs::canonicalize(worktree).unwrap();
    p.ok(&["start"]);
    fs::write(p.root.join("notes.txt"), "uncommitted in the root\n").unwrap();

    let reply = hook(&p, &start_event("s1", "startup", &worktree));
    assert!(reply.contains("tracking this worktree"), "{reply}");
    for word in [
        "differ",
        "S0",
        "notes.txt",
        "attach --",
        "baseRef",
        "observed",
    ] {
        assert!(!reply.contains(word), "{reply}");
    }
    let id = only_run(&p);
    let observed = until(&p, &id, |at_start| at_start["observed"].is_object())["observed"].clone();
    assert_eq!(
        observed["paths"],
        serde_json::json!([{"path": "notes.txt"}])
    );

    let status = p.ok(&["status", &id]);
    let shown = at_start_lines(&status);
    assert!(shown.contains("differed from its S0 at 1 path"), "{shown}");
    let generic = shown
        .find("dispatch attach -- <agent>")
        .expect("the managed launch");
    let claude = shown
        .find("Claude Code: local commits reach a claude --worktree workspace only with worktree.baseRef: \"head\"")
        .expect("Claude's advice");
    assert!(generic < claude, "the managed launch comes first: {shown}");
    assert!(shown.contains("neither that setting nor a commit copies uncommitted changes"));

    // Restarted, the owner neither repeats nor recomputes it.
    p.ok(&["stop"]);
    fs::write(p.root.join("later.txt"), "later\n").unwrap();
    p.ok(&["start"]);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(p.observed(&id), observed);
    assert_eq!(p.observations(&id), 1);

    // Missing when an owner starts after publication: it stays missing.
    p.ok(&["stop"]);
    p.set_at_start(&id, Some(Value::from("not_observed")));
    p.ok(&["start"]);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(p.at_start(&id), "not_observed");
    assert_eq!(
        at_start_lines(&p.ok(&["status", &id])),
        "  source at start: not observed"
    );
    p.ok(&["stop"]);
}

/// A session resumed in a worktree whose earlier Work was closed without
/// landing begins new Work whose S0 holds changes the source lacks: the
/// reverse direction, with no author named.
#[test]
fn a_resumed_session_after_closed_unlanded_work_warns_of_the_reverse() {
    let p = Project::new();
    let worktree = p.root.join(".claude/worktrees/x");
    git(
        &p.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "worktree-x",
            worktree.to_str().unwrap(),
        ],
    );
    let worktree = fs::canonicalize(worktree).unwrap();
    p.ok(&["start"]);
    hook(&p, &start_event("s1", "startup", &worktree));
    let first = only_run(&p);
    until(&p, &first, |at_start| {
        at_start["observed"]["differing"] == 0
    });

    fs::write(worktree.join("src/lib.rs"), TWO).unwrap();
    hook(
        &p,
        &serde_json::json!({
            "hook_event_name": "SessionEnd", "session_id": "s1",
            "reason": "prompt_input_exit", "cwd": worktree,
        }),
    );
    p.ok(&["close", &first, "--reason", "unfinished"]);
    assert_eq!(
        fs::read_to_string(p.root.join("src/lib.rs")).unwrap(),
        "pub fn f() -> i32 {\n    1\n}\n"
    );

    hook(&p, &start_event("s1", "resume", &worktree));
    let second = run_ids(&p)
        .into_iter()
        .find(|id| *id != first)
        .expect("the resumed session is new Work");
    let observed =
        until(&p, &second, |at_start| at_start["observed"].is_object())["observed"].clone();
    assert_eq!(
        observed["paths"],
        serde_json::json!([{"path": "src/lib.rs"}])
    );
    p.ok(&["stop"]);
}

fn start_event(session: &str, source: &str, cwd: &Path) -> Value {
    serde_json::json!({
        "hook_event_name": "SessionStart", "session_id": session, "source": source,
        "cwd": cwd, "transcript_path": "/dev/null", "model": "claude-sonnet-5",
    })
}

fn hook(p: &Project, event: &Value) -> String {
    use std::io::Write;
    let mut child = Command::new(assert_cmd::cargo_bin!("dispatch"))
        .arg("--state-dir")
        .arg(&p.state)
        .args(["hook", "claude"])
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
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    text(&output)
}

fn run_ids(p: &Project) -> Vec<String> {
    fs::read_dir(p.state.join("runs"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

fn only_run(p: &Project) -> String {
    let ids = run_ids(p);
    assert_eq!(ids.len(), 1, "{ids:?}");
    ids.into_iter().next().unwrap()
}

/// Wait for the owner to record Work `id`'s source at start until `ready`.
fn until(p: &Project, id: &str, ready: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let at_start = p.at_start(id);
        if ready(&at_start) {
            return at_start;
        }
        assert!(Instant::now() < deadline, "never observed: {at_start}");
        std::thread::sleep(Duration::from_millis(200));
    }
}
