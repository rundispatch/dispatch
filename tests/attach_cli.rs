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

    /// Another linked worktree of `root`, on its own branch.
    fn worktree(&self, name: &str) -> PathBuf {
        let path = self._temp.path().join(name);
        git(
            &self.root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                name,
                path.to_str().unwrap(),
            ],
        );
        fs::canonicalize(path).unwrap()
    }

    /// `attach` (as `attach`) of another worktree.
    fn attach_at(&self, workspace: &std::path::Path) -> String {
        let assert = self
            .dispatch(&[
                "attach",
                "--workspace",
                workspace.to_str().unwrap(),
                "--root",
                self.root.to_str().unwrap(),
                "--allow-unsafe-local",
            ])
            .success();
        Self::stdout(&assert)
            .lines()
            .find_map(|line| line.strip_prefix("ATTACHED "))
            .expect("attach prints ATTACHED <id>")
            .to_owned()
    }

    /// A command run in the root, as a person there types it: ID-less
    /// decisions take the source from the current directory.
    fn dispatch_in_root(&self, args: &[&str]) -> assert_cmd::assert::Assert {
        cargo_bin_cmd!("dispatch")
            .current_dir(&self.root)
            .arg("--state-dir")
            .arg(&self.state)
            .args(args)
            .assert()
    }

    fn events(&self, kind: &str) -> Vec<(String, Value)> {
        let db = self.db();
        let mut statement = db
            .prepare("SELECT run_id, payload_json FROM events WHERE event_type = ?1 ORDER BY id")
            .unwrap();
        statement
            .query_map([kind], |row| {
                let payload: String = row.get(1)?;
                Ok((row.get(0)?, serde_json::from_str(&payload).unwrap()))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn all_feedback(&self) -> i64 {
        self.db()
            .query_row("SELECT COUNT(*) FROM goal_feedback_revisions", [], |row| {
                row.get(0)
            })
            .unwrap()
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
            .contains("attached work has no Dispatch task to refresh; finish, reject or close it"),
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

const TWO: &str = "pub fn f() -> i32 {\n    2\n}\n";
const ONE: &str = "pub fn f() -> i32 {\n    1\n}\n";

/// The 0.4.12 release blocker (P0's R1): attach A, then B; finish B, then A.
/// A's summary names A in every command it suggests. With two results
/// waiting, an ID-less accept or reject decides nothing and names both, and
/// the Work still in progress, which does not count. Named, A is accepted,
/// not B. Once B is the only result waiting, an ID-less reject means B.
#[test]
fn finish_summaries_name_their_work_and_id_less_decisions_refuse_two_results() {
    let f = Fixture::new(true);
    let (wt_b, wt_c) = (f.worktree("wt-b"), f.worktree("wt-c"));
    let a = f.attach(&[]);
    let b = f.attach_at(&wt_b);
    let in_progress = f.attach_at(&wt_c);
    fs::write(f.workspace.join("src/lib.rs"), TWO).unwrap();
    fs::write(wt_b.join("src/b.rs"), "pub fn b() {}\n").unwrap();
    f.dispatch(&["finish", &b]).success();
    let summary = Fixture::stdout(&f.dispatch(&["finish", &a]).success());
    for command in ["diff", "accept", "reject"] {
        assert!(
            summary.contains(&format!("dispatch {command} {a}\n")),
            "{summary}"
        );
    }
    assert!(
        summary
            .lines()
            .filter(|line| line.contains("dispatch "))
            .all(|line| line.contains(&a)),
        "every suggested command names A: {summary}"
    );

    for command in ["accept", "reject"] {
        let stderr = Fixture::stderr(&f.dispatch_in_root(&[command]).failure());
        assert!(stderr.contains("2 results wait for review"), "{stderr}");
        for id in [&a, &b] {
            assert!(
                stderr.contains(&format!("dispatch {command} {id}")),
                "{stderr}"
            );
        }
        let (_, listed) = stderr.split_once("Still in progress").expect(&stderr);
        assert!(listed.contains(&in_progress), "{stderr}");
    }
    // Nothing changed: the source, the reviews and the events are as they were.
    assert_eq!(fs::read_to_string(f.root.join("src/lib.rs")).unwrap(), ONE);
    assert!(!f.root.join("src/b.rs").exists());
    assert_eq!(f.all_feedback(), 0);
    assert!(f.events("review.accepted").is_empty() && f.events("review.rejected").is_empty());
    for id in [&a, &b] {
        assert_eq!(f.metadata(id)["outcome"]["review"], "pending");
    }

    // Named, A is the Work accepted, and only A.
    f.dispatch_in_root(&["accept", &a]).success();
    assert_eq!(fs::read_to_string(f.root.join("src/lib.rs")).unwrap(), TWO);
    assert!(!f.root.join("src/b.rs").exists());
    assert_eq!(f.metadata(&b)["outcome"]["review"], "pending");
    let accepted: Vec<String> = f
        .events("review.accepted")
        .into_iter()
        .map(|e| e.0)
        .collect();
    assert_eq!(accepted, [a.as_str()]);

    // B is the only result waiting now; Work in progress does not count.
    f.dispatch_in_root(&["reject"]).success();
    assert_eq!(f.metadata(&b)["outcome"]["review"], "rejected");
    let rejected: Vec<String> = f
        .events("review.rejected")
        .into_iter()
        .map(|e| e.0)
        .collect();
    assert_eq!(rejected, [b.as_str()]);
    assert_eq!(f.metadata(&in_progress)["outcome"]["lifecycle"], "working");
}

/// With no result waiting, an ID-less decision says so and records nothing.
#[test]
fn id_less_decisions_with_no_result_waiting_say_so() {
    let f = Fixture::new(true);
    let a = f.attach(&[]);
    for command in ["accept", "reject"] {
        let stderr = Fixture::stderr(&f.dispatch_in_root(&[command]).failure());
        assert!(stderr.contains("no result waits for review"), "{stderr}");
        assert!(
            stderr.contains("Still in progress, not counted:"),
            "{stderr}"
        );
        assert!(stderr.contains(&a), "{stderr}");
    }
    assert_eq!(f.metadata(&a)["outcome"]["lifecycle"], "working");
    assert_eq!(f.all_feedback(), 0);
}

/// Explicit IDs keep unique, case-insensitive prefixes. A prefix two Work
/// items share refuses and changes nothing; whatever a prefix names is
/// recorded under its full ID, in every event.
#[test]
fn prefixes_resolve_to_full_ids_and_overlapping_prefixes_refuse() {
    let f = Fixture::new(true);
    let wt_b = f.worktree("wt-b");
    let a = f.attach(&[]);
    let b = f.attach_at(&wt_b);
    fs::write(f.workspace.join("src/lib.rs"), TWO).unwrap();
    fs::write(wt_b.join("src/b.rs"), "pub fn b() {}\n").unwrap();
    f.dispatch(&["finish", &a]).success();
    f.dispatch(&["finish", &b]).success();
    let shared: String = a
        .chars()
        .zip(b.chars())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x)
        .collect();
    assert!(
        !shared.is_empty(),
        "Work made seconds apart shares a prefix"
    );
    for args in [
        vec!["accept", shared.as_str()],
        vec!["close", shared.as_str(), "--reason", "already-present"],
    ] {
        let stderr = Fixture::stderr(&f.dispatch(&args).failure());
        assert!(stderr.contains("run prefix is ambiguous"), "{stderr}");
    }
    assert_eq!(f.all_feedback(), 0);
    assert!(f.events("work.closed").is_empty());

    let a_prefix = a[..shared.len() + 1].to_lowercase();
    let b_prefix = &b[..shared.len() + 1];
    f.dispatch(&[
        "close",
        b_prefix,
        "--reason",
        "superseded",
        "--replacement",
        &a_prefix,
    ])
    .success();
    let closed = f.events("work.closed");
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].0, b);
    assert_eq!(closed[0].1["replacement"], a.as_str());
    assert_eq!(
        f.metadata(&b)["outcome"]["closed"]["replacement"],
        a.as_str()
    );

    f.dispatch(&["accept", &a_prefix]).success();
    let accepted: Vec<String> = f
        .events("review.accepted")
        .into_iter()
        .map(|e| e.0)
        .collect();
    assert_eq!(accepted, [a.as_str()]);
    let feedback: String = f
        .db()
        .query_row("SELECT run_id FROM goal_feedback_revisions", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(feedback, a);
}

/// Unfinished Work has no result to reject: reject refuses and names close.
/// Close freezes its changes as finish would, without running checks,
/// records no review, and deletes nothing; closed, the Work is over. Work
/// with no changes closes the same way.
#[test]
fn closing_unfinished_work_freezes_its_changes_and_records_no_review() {
    let f = Fixture::new(true);
    let empty_wt = f.worktree("wt-empty");
    let a = f.attach(&[]);
    let empty = f.attach_at(&empty_wt);
    edit_workspace(&f.workspace);
    let stderr = Fixture::stderr(&f.dispatch(&["reject", &a]).failure());
    assert!(
        stderr.contains(&format!("dispatch close {a} --reason unfinished")),
        "{stderr}"
    );
    assert_eq!(f.metadata(&a)["outcome"]["lifecycle"], "working");
    // Presence is judged on a delivered result, so unfinished Work is
    // finished first.
    let stderr = Fixture::stderr(
        &f.dispatch(&["close", &a, "--reason", "already-present"])
            .failure(),
    );
    assert!(
        stderr.contains("it takes unfinished or superseded"),
        "{stderr}"
    );

    let out = Fixture::stdout(
        &f.dispatch(&[
            "close",
            &a,
            "--reason",
            "unfinished",
            "--explanation",
            "dropped the approach",
        ])
        .success(),
    );
    assert!(out.contains(&format!("Closed {a}: unfinished")), "{out}");
    let metadata = f.metadata(&a);
    let outcome = &metadata["outcome"];
    assert_eq!(
        (
            &outcome["lifecycle"],
            &outcome["work_result"],
            &outcome["review"],
            &outcome["application"]
        ),
        (
            &Value::from("finished"),
            &Value::from("cancelled"),
            &Value::from("not_requested"),
            &Value::from("not_applied")
        )
    );
    assert_eq!(outcome["closed"]["reason"], "unfinished");
    assert_eq!(outcome["closed"]["explanation"], "dropped the approach");
    // Frozen without its ignored build output, and without running checks.
    let patch = fs::read_to_string(f.state.join("runs").join(&a).join("delta.patch")).unwrap();
    assert!(
        patch.contains("+    2") && patch.contains("src/new.rs") && !patch.contains("build/"),
        "{patch}"
    );
    assert_eq!(metadata["candidates"][0]["checks"], serde_json::json!([]));
    assert!(
        f.workspace.join("src/new.rs").exists(),
        "the workspace is kept"
    );
    assert_eq!(fs::read_to_string(f.root.join("src/lib.rs")).unwrap(), ONE);
    assert_eq!(f.event_count(&a, "work.closed"), 1);
    assert_eq!(
        f.event_count(&a, "review.accepted") + f.event_count(&a, "review.rejected"),
        0
    );
    assert_eq!(f.goal_feedback_count(&a), 0);

    // Closed is over: it closes, finishes and is reviewed no more.
    for args in [
        vec!["close", a.as_str(), "--reason", "unfinished"],
        vec!["finish", a.as_str()],
        vec!["accept", a.as_str()],
        vec!["reject", a.as_str()],
    ] {
        f.dispatch(&args).failure();
    }
    assert_eq!(f.goal_feedback_count(&a), 0);
    let status = Fixture::stdout(&f.dispatch(&["status", &a]).success());
    for line in [
        "Status           closed",
        "Closed           unfinished",
        "dropped the approach",
    ] {
        assert!(status.contains(line), "{status}");
    }
    let history = Fixture::stdout(&f.dispatch(&["history"]).success());
    assert!(history.contains(" closed "), "{history}");

    f.dispatch(&["close", &empty, "--reason", "unfinished"])
        .success()
        .stdout(predicates::str::contains("it kept no changes"));
    assert_eq!(f.metadata(&empty)["outcome"]["work_result"], "cancelled");
}

/// A result whose whole patch is already in the source closes as already
/// present: check says to, half of the patch is not enough, and nothing is
/// recorded as a review. A person's earlier rejection is never closed over.
#[test]
fn already_present_closes_only_a_patch_wholly_in_the_source() {
    let f = Fixture::new(true);
    let wt_b = f.worktree("wt-b");
    let a = f.attach(&[]);
    let b = f.attach_at(&wt_b);
    edit_workspace(&f.workspace);
    fs::write(wt_b.join("src/b.rs"), "pub fn b() {}\n").unwrap();
    f.dispatch(&["finish", &a]).success();
    f.dispatch(&["finish", &b]).success();

    // Half of A lands by hand.
    fs::write(f.root.join("src/lib.rs"), TWO).unwrap();
    let stderr = Fixture::stderr(
        &f.dispatch(&["close", &a, "--reason", "already-present"])
            .failure(),
    );
    assert!(stderr.contains("is not in the source now"), "{stderr}");
    let stderr = Fixture::stderr(
        &f.dispatch(&["close", &a, "--reason", "unfinished"])
            .failure(),
    );
    assert!(
        stderr.contains("it takes already-present or superseded"),
        "{stderr}"
    );
    assert_eq!(f.metadata(&a)["outcome"]["review"], "pending");
    assert_eq!(f.event_count(&a, "work.closed"), 0);

    // The rest lands: STOP, and check says to close it.
    fs::write(f.root.join("src/new.rs"), "pub fn g() {}\n").unwrap();
    let check = Fixture::stdout(&f.dispatch(&["check", &a]).success());
    assert!(check.contains("Coherence: STOP"), "{check}");
    assert!(
        check.contains(&format!(
            "Next: dispatch close {a} --reason already-present"
        )),
        "{check}"
    );
    f.dispatch(&["close", &a, "--reason", "already-present"])
        .success();
    let metadata = f.metadata(&a);
    assert_eq!(metadata["outcome"]["work_result"], "ready");
    assert_eq!(metadata["outcome"]["review"], "not_requested");
    assert_eq!(metadata["outcome"]["application"], "not_applied");
    assert_eq!(metadata["outcome"]["closed"]["reason"], "already_present");
    let closed = f.events("work.closed");
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].1["by"], "human");
    assert!(closed[0].1["world_digest"].is_string(), "{:?}", closed[0]);
    assert_eq!(f.goal_feedback_count(&a), 0);
    let check = Fixture::stdout(&f.dispatch(&["check", &a]).success());
    assert!(
        check.contains("Closed: changes already present; nothing to do"),
        "{check}"
    );
    let status = Fixture::stdout(&f.dispatch(&["status", &a]).success());
    assert!(
        status.contains("Closed           changes already present"),
        "{status}"
    );

    // B was rejected; closing never sets a person's judgment aside.
    f.dispatch(&["reject", &b]).success();
    let stderr = Fixture::stderr(
        &f.dispatch(&["close", &b, "--reason", "already-present"])
            .failure(),
    );
    assert!(stderr.contains("already reviewed"), "{stderr}");
    assert_eq!(f.metadata(&b)["outcome"]["review"], "rejected");
    assert_eq!(f.all_feedback(), 1);
}

/// Repair recovery (plan §4 D): the replacement is new Work, finished with
/// its changes kept, and neither rejected nor closed. Until there is one, the
/// old result stays open; closed as superseded, the old one no longer waits,
/// and the new one is the only result an ID-less decision can mean. Both
/// patches are kept.
#[test]
fn superseded_needs_a_finished_replacement_with_its_changes_kept() {
    let f = Fixture::new(true);
    let old = f.attach(&[]);
    edit_workspace(&f.workspace);
    f.dispatch(&["finish", &old]).success();
    let new_wt = f.worktree("wt-new");
    let new = f.attach_at(&new_wt);
    let empty_wt = f.worktree("wt-empty");
    let empty = f.attach_at(&empty_wt);
    f.dispatch(&["finish", &empty]).success();

    let (old_id, new_id, empty_id) = (old.as_str(), new.as_str(), empty.as_str());
    for (args, says) in [
        (
            vec!["close", old_id, "--reason", "superseded"],
            "--replacement names",
        ),
        (
            vec![
                "close",
                old_id,
                "--reason",
                "already-present",
                "--replacement",
                new_id,
            ],
            "and only with it",
        ),
        (
            vec![
                "close",
                old_id,
                "--reason",
                "superseded",
                "--replacement",
                old_id,
            ],
            "cannot supersede itself",
        ),
        (
            vec![
                "close",
                old_id,
                "--reason",
                "superseded",
                "--replacement",
                new_id,
            ],
            "has no finished result",
        ),
        (
            vec![
                "close",
                old_id,
                "--reason",
                "superseded",
                "--replacement",
                empty_id,
            ],
            "kept no changes",
        ),
    ] {
        let stderr = Fixture::stderr(&f.dispatch(&args).failure());
        assert!(stderr.contains(says), "{args:?}: {stderr}");
    }
    assert_eq!(f.metadata(&old)["outcome"]["review"], "pending");
    assert_eq!(f.event_count(&old, "work.closed"), 0);
    // The empty result is judged on its own.
    f.dispatch(&["reject", &empty]).success();

    fs::write(new_wt.join("src/lib.rs"), "pub fn f() -> i32 {\n    3\n}\n").unwrap();
    f.dispatch(&["finish", &new]).success();

    // A replacement must be a live result: finished with its changes kept,
    // yet rejected by a person, or itself closed, it replaces nothing.
    let finished = |name: &str| {
        let workspace = f.worktree(name);
        let id = f.attach_at(&workspace);
        fs::write(workspace.join(format!("src/{name}.rs")), "pub fn s() {}\n").unwrap();
        f.dispatch(&["finish", &id]).success();
        id
    };
    let (rejected, closed) = (finished("rejected"), finished("closed"));
    f.dispatch(&["reject", &rejected]).success();
    f.dispatch(&[
        "close",
        &closed,
        "--reason",
        "superseded",
        "--replacement",
        &new,
    ])
    .success();
    for (replacement, says) in [
        (&rejected, "was itself rejected"),
        (&closed, "was itself closed"),
    ] {
        let stderr = Fixture::stderr(
            &f.dispatch(&[
                "close",
                &old,
                "--reason",
                "superseded",
                "--replacement",
                replacement,
            ])
            .failure(),
        );
        assert!(stderr.contains(says), "{stderr}");
    }
    assert_eq!(f.metadata(&old)["outcome"]["review"], "pending");
    assert_eq!(f.event_count(&old, "work.closed"), 0);

    f.dispatch_in_root(&["accept"]).failure();
    f.dispatch(&[
        "close",
        &old,
        "--reason",
        "superseded",
        "--replacement",
        &new,
        "--explanation",
        "repaired in a new worktree",
    ])
    .success();
    let closed = &f.metadata(&old)["outcome"];
    assert_eq!(closed["closed"]["reason"], "superseded");
    assert_eq!(closed["closed"]["replacement"], new.as_str());
    assert_eq!(closed["review"], "not_requested");
    let patch =
        |id: &str| fs::read_to_string(f.state.join("runs").join(id).join("delta.patch")).unwrap();
    assert!(patch(&old).contains("+    2"), "{}", patch(&old));
    assert!(patch(&new).contains("+    3"), "{}", patch(&new));

    // Only the replacement waits now.
    f.dispatch_in_root(&["accept"]).success();
    assert_eq!(f.metadata(&new)["outcome"]["application"], "applied");
    assert_eq!(f.metadata(&old)["outcome"]["application"], "not_applied");
    assert_eq!(
        fs::read_to_string(f.root.join("src/lib.rs")).unwrap(),
        "pub fn f() -> i32 {\n    3\n}\n"
    );
    assert_eq!(f.goal_feedback_count(&old), 0);
}

/// A worktree that vanished with nothing recorded yet: while an owner watches
/// the project it keeps the changes it last saw on its next tick, so close
/// refuses and changes nothing. With nothing watching, nobody followed the
/// worktree: close goes ahead and says no changes were kept. The test holds
/// the project's serve lock itself, which is how a running owner reads, so
/// no owner tick can race it.
#[test]
fn closing_work_whose_worktree_vanished_waits_for_a_watching_owner() {
    use sha2::{Digest, Sha256};
    use std::os::fd::AsRawFd;
    let f = Fixture::new(true);
    let id = f.attach(&[]);
    edit_workspace(&f.workspace);
    fs::remove_dir_all(&f.workspace).unwrap();

    let locks = f.state.join("locks");
    fs::create_dir_all(&locks).unwrap();
    let key = hex::encode(Sha256::digest(f.root.to_string_lossy().as_bytes()));
    let serve_lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(locks.join(format!("serve-{key}.lock")))
        .unwrap();
    // SAFETY: an exclusive lock on a descriptor this test owns; dropping it
    // releases the lock.
    assert_eq!(
        unsafe { libc::flock(serve_lock.as_raw_fd(), libc::LOCK_EX) },
        0
    );
    let close = ["close", id.as_str(), "--reason", "workspace-removed"];
    let stderr = Fixture::stderr(&f.dispatch(&close).failure());
    assert!(
        stderr.contains("has not yet kept the changes it last saw"),
        "{stderr}"
    );
    let outcome = f.metadata(&id)["outcome"].clone();
    assert_eq!(outcome["lifecycle"], "working");
    assert!(outcome.get("closed").is_none(), "{outcome}");
    assert_eq!(f.event_count(&id, "work.closed"), 0);

    drop(serve_lock);
    let out = Fixture::stdout(&f.dispatch(&close).success());
    assert!(
        out.contains(
            "no changes were kept, because its workspace vanished while nothing watched the project"
        ),
        "{out}"
    );
    let outcome = f.metadata(&id)["outcome"].clone();
    assert_eq!(outcome["work_result"], "cancelled");
    assert_eq!(outcome["closed"]["reason"], "workspace_removed");
    assert_eq!(f.event_count(&id, "work.closed"), 1);
}

/// Close and finish of the same Work at once: the run's lock lets one win,
/// the other refuses, and either way its changes are frozen.
#[test]
fn concurrent_close_and_finish_never_lose_the_changes() {
    let f = Fixture::new(true);
    let id = f.attach(&[]);
    edit_workspace(&f.workspace);
    let spawn = |args: &[&str]| {
        Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&f.state)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let close = spawn(&["close", &id, "--reason", "unfinished"]);
    let finish = spawn(&["finish", &id]);
    let (close, finish) = (
        close.wait_with_output().unwrap(),
        finish.wait_with_output().unwrap(),
    );
    assert_ne!(
        close.status.success(),
        finish.status.success(),
        "exactly one wins: {close:?} {finish:?}"
    );
    let outcome = f.metadata(&id)["outcome"].clone();
    if close.status.success() {
        assert_eq!(outcome["work_result"], "cancelled");
        assert_eq!(outcome["closed"]["reason"], "unfinished");
    } else {
        assert_eq!(outcome["work_result"], "ready");
        assert!(outcome.get("closed").is_none());
    }
    let patch = fs::read_to_string(f.state.join("runs").join(&id).join("delta.patch")).unwrap();
    assert!(patch.contains("+    2"), "{patch}");
}
