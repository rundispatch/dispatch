//! `dispatch attach` (foreign form) and `dispatch finish`: attached external
//! work as a `RunRecord` with `mode: Attached` (see
//! `docs/plan-0.3-auto-apply-and-attach.md` parts 6 and 14). The fixture is a
//! Git repository (`root`) with a `.gitignore`'d `build/out.bin` and a
//! `checks.verify` command, plus a linked worktree (`workspace`) on a new
//! branch that a test edits directly, playing the role of an already-running
//! agent Dispatch did not launch.
#![cfg(unix)]

use std::{fs, path::PathBuf, process::Command};

use assert_cmd::cargo_bin_cmd;
use serde_json::Value;

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    state: PathBuf,
}

/// The root's working tree may already differ from S0 (the merge-base commit)
/// when work is attached: an equal whole-tree fingerprint at apply time then
/// says nothing about S0, so the gate must evaluate rather than take the
/// unmoved-world shortcut, and a moved-world apply stores its validity.
#[test]
fn auto_apply_of_attached_work_evaluates_even_when_the_root_did_not_move_since_attach() {
    use dispatch::{orchestrator::ApplyOutcome, state::State};
    let fixture = Fixture::new(true);
    // Uncommitted edit in the root, disjoint from what the worktree will touch,
    // present before the attach: S0 (HEAD) already differs from the root.
    fs::write(fixture.root.join("src/other.rs"), "pub fn g() {}\n").unwrap();
    let id = fixture.attach(&["--auto-apply"]);
    fs::write(
        fixture.workspace.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    fixture.dispatch(&["finish", &id]).success();

    let outcome = dispatch::orchestrator::auto_apply(
        &State {
            root: fixture.state.clone(),
        },
        &id,
    )
    .unwrap();
    match outcome {
        ApplyOutcome::Applied { validity, .. } => {
            let validity = validity.expect("a moved world stores its validity");
            assert!(validity.world_changed);
            assert_eq!(validity.analysis, dispatch::AnalysisLevel::Integration);
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    let metadata = fixture.metadata(&id);
    assert_eq!(metadata["outcome"]["applied_by"], "auto_apply");
    assert_eq!(
        metadata["coherence"]["validity"]["analysis"], "integration",
        "{}",
        metadata["coherence"]
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("src/lib.rs")).unwrap(),
        "pub fn f() -> i32 {\n    2\n}\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("src/other.rs")).unwrap(),
        "pub fn g() {}\n"
    );
}

/// `explain` on attached work has no selection to explain; it shows where S0
/// came from, and the verdict, and never fails for the common case of an
/// unmoved root.
#[test]
fn explain_shows_attachment_provenance_and_verdict() {
    let fixture = Fixture::new(true);
    let id = fixture.attach(&[]);
    let assert = fixture.dispatch(&["explain", &id]).success();
    let out = Fixture::stdout(&assert);
    assert!(out.contains("Attached work"), "{out}");
    assert!(out.contains("merge base"), "{out}");
    assert!(out.contains("confidence: full"), "{out}");
    assert!(!out.contains("no single-agent selection"), "{out}");

    fs::write(
        fixture.workspace.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    3\n}\n",
    )
    .unwrap();
    fixture.dispatch(&["finish", &id]).success();
    let assert = fixture.dispatch(&["explain", &id]).success();
    let out = Fixture::stdout(&assert);
    assert!(out.contains("finished: explicit"), "{out}");
    assert!(out.contains("CONTINUE"), "{out}");
}

impl Fixture {
    /// `with_checks`: whether `root`'s `dispatch.yml` configures
    /// `checks.verify`. Every test but the local-authority defense-in-depth
    /// one wants checks configured, matching the shared fixture description.
    fn new(with_checks: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn f() -> i32 {\n    1\n}\n").unwrap();
        fs::write(root.join(".gitignore"), "build/\n").unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::write(root.join("build/out.bin"), b"original build output\n").unwrap();
        if with_checks {
            fs::write(root.join("dispatch.yml"), "checks:\n  verify: ['true']\n").unwrap();
        }
        git(&root, &["init", "--quiet"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);

        let workspace = temp.path().join("wt");
        git(
            &root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "agent-branch",
                workspace.to_str().unwrap(),
            ],
        );

        let state = temp.path().join("state");
        Self {
            _temp: temp,
            root: fs::canonicalize(root).unwrap(),
            workspace: fs::canonicalize(workspace).unwrap(),
            state,
        }
    }

    fn dispatch(&self, args: &[&str]) -> assert_cmd::assert::Assert {
        cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&self.state)
            .args(args)
            .assert()
    }

    /// `dispatch attach --workspace <workspace> --root <root>
    /// --allow-unsafe-local <extra...>`, returning the new run's ID.
    fn attach(&self, extra: &[&str]) -> String {
        let mut args = vec![
            "attach",
            "--workspace",
            self.workspace.to_str().unwrap(),
            "--root",
            self.root.to_str().unwrap(),
            "--allow-unsafe-local",
        ];
        args.extend_from_slice(extra);
        let assert = self.dispatch(&args).success();
        Self::stdout(&assert)
            .lines()
            .find_map(|line| line.strip_prefix("ATTACHED "))
            .expect("attach prints ATTACHED <id>")
            .to_owned()
    }

    fn metadata_path(&self, id: &str) -> PathBuf {
        self.state.join("runs").join(id).join("metadata.json")
    }

    fn metadata(&self, id: &str) -> Value {
        serde_json::from_slice(&fs::read(self.metadata_path(id)).unwrap()).unwrap()
    }

    fn baseline_path(&self, id: &str) -> PathBuf {
        self.state.join("runs").join(id).join("baseline")
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.state.join("dispatch.db")).unwrap()
    }

    fn event_count(&self, id: &str, kind: &str) -> i64 {
        self.db()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_type = ?2",
                rusqlite::params![id, kind],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn goal_feedback_count(&self, id: &str) -> i64 {
        self.db()
            .query_row(
                "SELECT COUNT(*) FROM goal_feedback_revisions WHERE run_id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// Defense in depth only: directly edits the committed projection so a
    /// finished attach carries `environment.unsafe_local: false` despite
    /// checks being configured, a combination `dispatch attach` itself never
    /// produces (it always requires `--allow-unsafe-local` up front when
    /// checks are configured). Regression coverage for the same guard inside
    /// `dispatch finish`.
    fn clear_unsafe_local(&self, id: &str) {
        let mut metadata = self.metadata(id);
        metadata["environment"]["unsafe_local"] = Value::Bool(false);
        fs::write(
            self.metadata_path(id),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        let db = self.db();
        let projection: String = db
            .query_row(
                "SELECT run_projection_json FROM runs WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        let mut projection: Value = serde_json::from_str(&projection).unwrap();
        projection["environment"]["unsafe_local"] = Value::Bool(false);
        db.execute(
            "UPDATE runs SET run_projection_json = ?1, unsafe_local = 0 WHERE id = ?2",
            (serde_json::to_string(&projection).unwrap(), id),
        )
        .unwrap();
    }

    fn stdout(assert: &assert_cmd::assert::Assert) -> String {
        String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
    }

    fn stderr(assert: &assert_cmd::assert::Assert) -> String {
        String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
    }
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
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
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// The edits an already-running agent made in the worktree before `finish`:
/// change `src/lib.rs`, add `src/new.rs`, and regenerate the ignored
/// `build/out.bin` (which must never appear in Δ).
fn edit_workspace(workspace: &std::path::Path) {
    fs::write(
        workspace.join("src/lib.rs"),
        "pub fn f() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    fs::write(workspace.join("src/new.rs"), "pub fn g() {}\n").unwrap();
    fs::create_dir_all(workspace.join("build")).unwrap();
    fs::write(
        workspace.join("build/out.bin"),
        b"regenerated build output\n",
    )
    .unwrap();
}

#[test]
fn foreign_attach_records_merge_base_baseline() {
    let f = Fixture::new(true);
    let root_head = git(&f.root, &["rev-parse", "HEAD"]);

    let id = f.attach(&[]);

    let metadata = f.metadata(&id);
    assert_eq!(metadata["mode"], "attached");
    assert_eq!(
        metadata["attachment"]["provenance"]["git_merge_base"]["commit"],
        root_head
    );
    assert_eq!(metadata["attachment"]["confidence"], "full");
    assert_eq!(metadata["attachment"]["capabilities"]["integrate"], false);

    let listing = git(
        &f.baseline_path(&id),
        &["ls-tree", "-r", "--name-only", "HEAD"],
    );
    assert!(!listing.contains("build/"), "{listing}");
}

#[test]
fn attach_refusals() {
    let f = Fixture::new(true);

    // Workspace equals root.
    let assert = f
        .dispatch(&[
            "attach",
            "--workspace",
            f.root.to_str().unwrap(),
            "--root",
            f.root.to_str().unwrap(),
            "--allow-unsafe-local",
        ])
        .failure();
    assert!(
        Fixture::stderr(&assert).contains("attach needs a separate worktree; run git worktree add"),
        "{}",
        Fixture::stderr(&assert)
    );

    // A workspace from another repository.
    let other = f._temp.path().join("other");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("file.txt"), "x\n").unwrap();
    git(&other, &["init", "--quiet"]);
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "--quiet", "-m", "initial"]);
    let assert = f
        .dispatch(&[
            "attach",
            "--workspace",
            other.to_str().unwrap(),
            "--root",
            f.root.to_str().unwrap(),
            "--allow-unsafe-local",
        ])
        .failure();
    assert!(
        Fixture::stderr(&assert)
            .contains("workspace belongs to a different repository than the integration root"),
        "{}",
        Fixture::stderr(&assert)
    );

    // A plain directory without a command.
    let plain = f._temp.path().join("plain");
    fs::create_dir_all(&plain).unwrap();
    let assert = f
        .dispatch(&[
            "attach",
            "--workspace",
            plain.to_str().unwrap(),
            "--root",
            f.root.to_str().unwrap(),
            "--allow-unsafe-local",
        ])
        .failure();
    assert!(
        Fixture::stderr(&assert)
            .contains("a plain directory can be attached only by wrapping the agent command"),
        "{}",
        Fixture::stderr(&assert)
    );

    // Checks configured without --allow-unsafe-local.
    let assert = f
        .dispatch(&[
            "attach",
            "--workspace",
            f.workspace.to_str().unwrap(),
            "--root",
            f.root.to_str().unwrap(),
        ])
        .failure();
    assert!(
        Fixture::stderr(&assert)
            .contains("finish runs your checks.verify on the host; pass --allow-unsafe-local"),
        "{}",
        Fixture::stderr(&assert)
    );

    // A second attach of the same workspace.
    let id = f.attach(&[]);
    let assert = f
        .dispatch(&[
            "attach",
            "--workspace",
            f.workspace.to_str().unwrap(),
            "--root",
            f.root.to_str().unwrap(),
            "--allow-unsafe-local",
        ])
        .failure();
    assert!(
        Fixture::stderr(&assert).contains(&format!("workspace already attached as {id}")),
        "{}",
        Fixture::stderr(&assert)
    );
}

#[test]
fn finish_collects_delta_runs_checks_and_becomes_ready() {
    let f = Fixture::new(true);
    let id = f.attach(&[]);
    edit_workspace(&f.workspace);

    let assert = f.dispatch(&["finish", &id]).success();
    assert!(
        Fixture::stdout(&assert).contains("Ready for review"),
        "{}",
        Fixture::stdout(&assert)
    );

    let metadata = f.metadata(&id);
    assert_eq!(metadata["status"], "ready_for_evaluation");
    assert_eq!(metadata["outcome"]["verification"], "passed");
    let mut changed = metadata["candidates"][0]["diff_stats"]["changed_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    changed.sort();
    assert_eq!(
        changed,
        vec!["src/lib.rs".to_owned(), "src/new.rs".to_owned()]
    );

    let assert = f.dispatch(&["check", &id]).success();
    assert!(
        Fixture::stdout(&assert).contains("Coherence: CONTINUE"),
        "{}",
        Fixture::stdout(&assert)
    );
}

/// A build leaves ignored output behind (large files, links out of the
/// tree); it can never enter Δ, so it must not stop the work from finishing.
#[test]
fn finish_succeeds_after_a_build_left_ignored_output() {
    let f = Fixture::new(true);
    let id = f.attach(&[]);
    edit_workspace(&f.workspace);
    let build = f.workspace.join("build");
    fs::create_dir_all(&build).unwrap();
    fs::File::create(build.join("huge.bin"))
        .unwrap()
        .set_len(600 * 1024 * 1024)
        .unwrap();
    std::os::unix::fs::symlink("/usr/bin/env", build.join("env")).unwrap();

    f.dispatch(&["finish", &id]).success();
    let metadata = f.metadata(&id);
    assert_eq!(metadata["status"], "ready_for_evaluation", "{metadata}");
    assert!(
        !metadata["candidates"][0]["diff_stats"]["changed_files"]
            .to_string()
            .contains("build/"),
        "{metadata}"
    );
}

#[test]
fn finish_then_root_moves_then_check_refreshes() {
    let f = Fixture::new(true);
    let id = f.attach(&[]);
    edit_workspace(&f.workspace);
    f.dispatch(&["finish", &id]).success();

    // The root moves underneath the finished work: the same line is edited
    // to a third, conflicting value.
    fs::write(f.root.join("src/lib.rs"), "pub fn f() -> i32 {\n    3\n}\n").unwrap();

    let assert = f.dispatch(&["check", &id]).success();
    let stdout = Fixture::stdout(&assert);
    assert!(stdout.contains("Coherence: REFRESH"), "{stdout}");
    assert!(stdout.contains("patch_conflict"), "{stdout}");

    // Attached work has no Dispatch task to refresh: the advice says so.
    assert!(!stdout.contains("dispatch refresh"), "{stdout}");
    assert!(stdout.contains("run your agent again"), "{stdout}");
    // Accepting stale attached work is refused and records no human review.
    let refused = f.dispatch(&["accept", &id]).failure();
    let stderr = String::from_utf8_lossy(&refused.get_output().stderr).into_owned();
    assert!(!stderr.contains("dispatch refresh"), "{stderr}");
    assert_eq!(f.metadata(&id)["outcome"]["review"], "pending");
    assert_eq!(f.goal_feedback_count(&id), 0);
    assert_eq!(f.event_count(&id, "review.accepted"), 0);
}

#[test]
fn accept_records_review_and_applies() {
    let f = Fixture::new(true);
    let id = f.attach(&[]);
    edit_workspace(&f.workspace);
    f.dispatch(&["finish", &id]).success();

    f.dispatch(&["accept", &id]).success();

    assert_eq!(f.event_count(&id, "review.accepted"), 1);
    assert_eq!(f.goal_feedback_count(&id), 1);
    assert_eq!(
        fs::read_to_string(f.root.join("src/lib.rs")).unwrap(),
        "pub fn f() -> i32 {\n    2\n}\n"
    );
    assert_eq!(
        fs::read_to_string(f.root.join("src/new.rs")).unwrap(),
        "pub fn g() {}\n"
    );
    let metadata = f.metadata(&id);
    assert_eq!(metadata["outcome"]["applied_by"], "human");
}

#[test]
fn reject_records_review_only() {
    let f = Fixture::new(true);
    let id = f.attach(&[]);
    edit_workspace(&f.workspace);
    f.dispatch(&["finish", &id]).success();

    f.dispatch(&["reject", &id]).success();

    assert_eq!(f.event_count(&id, "review.rejected"), 1);
    assert_eq!(f.goal_feedback_count(&id), 1);
    assert!(!f.root.join("src/new.rs").exists());
    assert_eq!(
        fs::read_to_string(f.root.join("src/lib.rs")).unwrap(),
        "pub fn f() -> i32 {\n    1\n}\n"
    );
}

#[test]
fn refresh_is_refused() {
    let f = Fixture::new(true);
    let id = f.attach(&[]);
    edit_workspace(&f.workspace);
    f.dispatch(&["finish", &id]).success();

    let assert = f.dispatch(&["refresh", &id]).failure();
    assert!(
        Fixture::stderr(&assert)
            .contains("attached work has no Dispatch task to refresh; finish or reject it"),
        "{}",
        Fixture::stderr(&assert)
    );
}

#[test]
fn finish_without_local_authority_is_refused_when_checks_are_configured() {
    let f = Fixture::new(true);
    let id = f.attach(&[]);
    edit_workspace(&f.workspace);
    // Every attach with checks configured requires --allow-unsafe-local up
    // front, so `run.environment.unsafe_local` is always true on a real
    // attach; this directly reaches for the guard finish keeps anyway.
    f.clear_unsafe_local(&id);

    let assert = f.dispatch(&["finish", &id]).failure();
    assert!(
        Fixture::stderr(&assert)
            .contains("finish runs your checks.verify on the host; pass --allow-unsafe-local"),
        "{}",
        Fixture::stderr(&assert)
    );

    // Nothing was persisted: the run is still active.
    let metadata = f.metadata(&id);
    assert_eq!(metadata["outcome"]["lifecycle"], "working");
}

/// Outside strict mode, attaching never walks the root's ignored build
/// output: its whole-tree fingerprint is not taken, because nothing reads it.
/// An unreadable ignored directory, which a walk could not get through, proves
/// it. Strict mode still fingerprints the root (`docs/plan-0.4.10.md` §15).
#[test]
fn attach_outside_strict_mode_never_walks_ignored_build_output() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new(true);
    let sealed = fixture.root.join("build/sealed");
    fs::create_dir_all(&sealed).unwrap();
    fs::write(sealed.join("artifact.o"), b"object\n").unwrap();
    fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000)).unwrap();

    let id = fixture.attach(&[]);
    assert_eq!(
        fixture.metadata(&id)["source_fingerprint"],
        "not fingerprinted (attached work outside strict mode)"
    );

    fs::write(
        fixture.root.join("dispatch.yml"),
        "checks:\n  verify: ['true']\ncoherence:\n  accept: strict\n",
    )
    .unwrap();
    git(&fixture.root, &["commit", "--quiet", "-am", "strict"]);
    let strict = fixture.workspace.parent().unwrap().join("wt-strict");
    git(
        &fixture.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "strict",
            strict.to_str().unwrap(),
        ],
    );
    fixture
        .dispatch(&[
            "attach",
            "--workspace",
            strict.to_str().unwrap(),
            "--root",
            fixture.root.to_str().unwrap(),
            "--allow-unsafe-local",
        ])
        .failure();
    fs::set_permissions(&sealed, fs::Permissions::from_mode(0o700)).unwrap();
}
