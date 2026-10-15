//! `orchestrator::auto_apply`: a finished run is applied automatically only
//! when the evidence is complete for the exact world it names (see
//! `docs/plan-0.3-auto-apply-and-attach.md`, part 5). There is no CLI surface
//! yet (that is a later work package), so every test calls the library
//! function directly.
//!
//! `Fixture` (a `fake-good` native run) is modeled on
//! `tests/coherence_accept.rs` and covers the eligibility rows, the
//! Legacy/Compatible/Blocked gate rows and the human accept path.
//! `AllocationFixture`, an immediate-delivery profile-bound run modeled on
//! `tests/profile_selection.rs`, covers the post-hoc review guard.
#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path, path::PathBuf, process::Command};

use assert_cmd::cargo_bin_cmd;
use dispatch::{
    orchestrator::{self, ApplyOutcome},
    state::State,
};
use serde_json::Value;

fn db(state_dir: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(state_dir.join("dispatch.db")).unwrap()
}

fn event_count(state_dir: &Path, run_id: &str, kind: &str) -> i64 {
    db(state_dir)
        .query_row(
            "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_type = ?2",
            rusqlite::params![run_id, kind],
            |row| row.get(0),
        )
        .unwrap()
}

fn event_payload(state_dir: &Path, run_id: &str, kind: &str) -> Value {
    let payload: String = db(state_dir)
        .query_row(
            "SELECT payload_json FROM events WHERE run_id = ?1 AND event_type = ?2 \
             ORDER BY sequence DESC LIMIT 1",
            rusqlite::params![run_id, kind],
            |row| row.get(0),
        )
        .unwrap();
    serde_json::from_str(&payload).unwrap()
}

fn feedback_count(state_dir: &Path, run_id: &str) -> i64 {
    db(state_dir)
        .query_row(
            "SELECT COUNT(*) FROM goal_feedback_revisions WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn metadata(state_dir: &Path, run_id: &str) -> Value {
    let path = state_dir.join("runs").join(run_id).join("metadata.json");
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

/// A Git source with the sole `fake-good` comparison-mode result on it.
/// `fake-good` creates one empty file, `dispatch-fake-good.txt`.
struct Fixture {
    _temp: tempfile::TempDir,
    source: PathBuf,
    state_dir: PathBuf,
    run_id: String,
}

impl Fixture {
    /// `extra_config` is extra YAML appended to `dispatch.yml` (`checks:`,
    /// `coherence:`, ...).
    fn new(extra_config: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("original.txt"), "baseline\n").unwrap();
        fs::write(
            source.join("dispatch.yml"),
            format!("execution:\n  timeout_secs: 600\n{extra_config}"),
        )
        .unwrap();
        for args in [
            &["init", "--quiet"][..],
            &["add", "-A"],
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "--quiet",
                "-m",
                "i",
            ],
        ] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&source)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let output = cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&state_dir)
            .arg("run")
            .arg(&source)
            .arg("--allow-unsafe-local")
            .args([
                "--task",
                "Create the fake artifact.",
                "--agent",
                "fake-good",
            ])
            .assert()
            .get_output()
            .clone();
        let run_id = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("RUN "))
            .expect("run output includes an ID")
            .to_owned();
        Self {
            _temp: temp,
            source,
            state_dir,
            run_id,
        }
    }

    fn state(&self) -> State {
        State {
            root: self.state_dir.clone(),
        }
    }

    fn metadata(&self) -> Value {
        metadata(&self.state_dir, &self.run_id)
    }

    fn auto_apply(&self) -> anyhow::Result<ApplyOutcome> {
        orchestrator::auto_apply(&self.state(), &self.run_id)
    }

    fn fake_file(&self) -> PathBuf {
        self.source.join("dispatch-fake-good.txt")
    }

    /// `dispatch accept <id>`: a human review that applies the result
    /// (`apply_locked` with `ApplyAuthority::Human`).
    fn apply_cli(&self) -> assert_cmd::assert::Assert {
        cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&self.state_dir)
            .args(["accept", &self.run_id])
            .assert()
    }
}

#[test]
fn unmoved_world_verified_applies_automatically() {
    let fixture = Fixture::new("checks:\n  verify: ['true']\n");

    let outcome = fixture.auto_apply().unwrap();
    assert!(
        matches!(outcome, ApplyOutcome::Applied { validity: None, .. }),
        "{outcome:?}"
    );

    let metadata = fixture.metadata();
    assert_eq!(metadata["outcome"]["applied_by"], "auto_apply");
    assert_eq!(metadata["outcome"]["review"], "pending");
    assert_eq!(metadata["outcome"]["application"], "applied");

    let payload = event_payload(&fixture.state_dir, &fixture.run_id, "result.applied");
    assert_eq!(payload["applied_by"], "auto_apply");
    assert_eq!(
        event_count(&fixture.state_dir, &fixture.run_id, "review.accepted"),
        0
    );
    assert_eq!(
        event_count(&fixture.state_dir, &fixture.run_id, "review.rejected"),
        0
    );
    assert_eq!(feedback_count(&fixture.state_dir, &fixture.run_id), 0);
    assert!(fixture.fake_file().is_file());
}

#[test]
fn empty_delta_is_skipped_not_applied() {
    let fixture = Fixture::new("checks:\n  verify: ['true']\n");
    // An agent that produced no change: the candidate's patch is empty.
    let diff_path = fixture.metadata()["candidates"][0]["diff_path"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::write(&diff_path, b"").unwrap();

    let outcome = fixture.auto_apply().unwrap();
    assert!(
        matches!(&outcome, ApplyOutcome::Skipped { reason } if reason == "empty_delta"),
        "{outcome:?}"
    );
    let metadata = fixture.metadata();
    assert_eq!(metadata["outcome"]["application"], "not_applied");
    assert_eq!(metadata["outcome"]["review"], "pending");
    assert!(metadata["outcome"].get("applied_by").is_none());
    assert_eq!(
        event_count(&fixture.state_dir, &fixture.run_id, "auto_apply.skipped"),
        1
    );
    assert_eq!(
        event_count(&fixture.state_dir, &fixture.run_id, "result.applied"),
        0
    );
    assert!(!fixture.fake_file().exists());
}

#[test]
fn already_applied_run_is_not_ready_and_nothing_is_persisted() {
    let fixture = Fixture::new("checks:\n  verify: ['true']\n");
    assert!(matches!(
        fixture.auto_apply().unwrap(),
        ApplyOutcome::Applied { .. }
    ));
    let revision = fixture.metadata()["state_revision"].as_u64().unwrap();

    // A second attempt on the applied run is `not_ready` and, like `run_busy`,
    // writes nothing: the revision and the journal are untouched.
    let outcome = fixture.auto_apply().unwrap();
    assert!(
        matches!(&outcome, ApplyOutcome::Skipped { reason } if reason == "not_ready"),
        "{outcome:?}"
    );
    assert_eq!(
        fixture.metadata()["state_revision"].as_u64().unwrap(),
        revision
    );
    assert_eq!(
        event_count(&fixture.state_dir, &fixture.run_id, "auto_apply.skipped"),
        0
    );
}

#[test]
fn verification_not_configured_is_skipped() {
    let fixture = Fixture::new("");

    let outcome = fixture.auto_apply().unwrap();
    assert_eq!(
        outcome,
        ApplyOutcome::Skipped {
            reason: "verification_not_configured".into()
        }
    );
    assert_eq!(
        event_payload(&fixture.state_dir, &fixture.run_id, "auto_apply.skipped")["reason"],
        "verification_not_configured"
    );
    assert!(!fixture.fake_file().exists());
}

#[test]
fn verification_failed_is_skipped() {
    let fixture = Fixture::new("checks:\n  verify: ['false']\n");

    let outcome = fixture.auto_apply().unwrap();
    assert_eq!(
        outcome,
        ApplyOutcome::Skipped {
            reason: "verification_failed".into()
        }
    );
    assert!(!fixture.fake_file().exists());
}

#[test]
fn moved_world_unrelated_edit_without_integration_checks_is_blocked() {
    let fixture =
        Fixture::new("checks:\n  verify: ['true']\ncoherence:\n  integration_checks: false\n");
    fs::write(fixture.source.join("original.txt"), "edited elsewhere\n").unwrap();

    let outcome = fixture.auto_apply().unwrap();
    assert!(
        matches!(&outcome, ApplyOutcome::Blocked { reason, .. } if reason == "integration_evidence_missing"),
        "{outcome:?}"
    );

    let metadata = fixture.metadata();
    assert_eq!(
        metadata["outcome"]["application"],
        "blocked_by_source_drift"
    );
    assert_eq!(metadata["outcome"]["review"], "pending");
    assert!(!fixture.fake_file().exists());
    assert_eq!(
        fs::read_to_string(fixture.source.join("original.txt")).unwrap(),
        "edited elsewhere\n"
    );

    let run = fixture.state().load_run(&fixture.run_id).unwrap();
    assert!(dispatch::coherence::is_ready_unapplied(&run));
}

#[test]
fn moved_world_unrelated_edit_with_passing_integration_check_applies() {
    let fixture = Fixture::new("checks:\n  verify: ['true']\n");
    fs::write(fixture.source.join("original.txt"), "edited elsewhere\n").unwrap();

    let outcome = fixture.auto_apply().unwrap();
    let ApplyOutcome::Applied { validity, .. } = &outcome else {
        panic!("expected Applied, got {outcome:?}");
    };
    let validity = validity
        .as_ref()
        .expect("validity present for a moved world");
    assert_eq!(validity.analysis, dispatch::AnalysisLevel::Integration);

    let payload = event_payload(&fixture.state_dir, &fixture.run_id, "result.applied");
    assert_eq!(payload["coherence"]["analysis"], "integration");
    assert!(fixture.fake_file().is_file());
}

#[test]
fn conflicting_edit_is_blocked_with_refresh() {
    let fixture = Fixture::new("checks:\n  verify: ['true']\n");
    fs::write(fixture.fake_file(), "someone else wrote this first\n").unwrap();

    let outcome = fixture.auto_apply().unwrap();
    assert!(
        matches!(&outcome, ApplyOutcome::Blocked { reason, .. } if reason == "verdict_refresh"),
        "{outcome:?}"
    );
    let payload = event_payload(&fixture.state_dir, &fixture.run_id, "auto_apply.blocked");
    assert_eq!(payload["coherence"]["decision"], "refresh");
    assert_eq!(
        fs::read_to_string(fixture.fake_file()).unwrap(),
        "someone else wrote this first\n"
    );
}

#[test]
fn patch_already_present_is_blocked_with_stop() {
    let fixture = Fixture::new("checks:\n  verify: ['true']\n");
    fs::write(fixture.fake_file(), "").unwrap();

    let outcome = fixture.auto_apply().unwrap();
    assert!(
        matches!(&outcome, ApplyOutcome::Blocked { reason, .. } if reason == "verdict_stop"),
        "{outcome:?}"
    );
    let payload = event_payload(&fixture.state_dir, &fixture.run_id, "auto_apply.blocked");
    assert_eq!(payload["coherence"]["decision"], "stop");
}

#[test]
fn strict_mode_with_a_moved_world_is_blocked() {
    let fixture = Fixture::new("checks:\n  verify: ['true']\ncoherence:\n  accept: strict\n");
    fs::write(fixture.source.join("original.txt"), "edited elsewhere\n").unwrap();

    let outcome = fixture.auto_apply().unwrap();
    assert_eq!(
        outcome,
        ApplyOutcome::Blocked {
            reason: "strict_mode_drift".into(),
            validity: None,
        }
    );
    assert!(!fixture.fake_file().exists());
}

#[test]
fn run_lock_held_by_a_foreground_owner_skips_without_persisting() {
    use std::os::fd::AsRawFd;
    let fixture = Fixture::new("checks:\n  verify: ['true']\n");
    let lock_path = fixture
        .state_dir
        .join("runs")
        .join(&fixture.run_id)
        .join(".operation.lock");
    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    // SAFETY: the descriptor stays open for the duration of this test.
    assert_eq!(
        unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "test lock failed"
    );

    let outcome = fixture.auto_apply().unwrap();
    assert_eq!(
        outcome,
        ApplyOutcome::Skipped {
            reason: "run_busy".into()
        }
    );
    assert_eq!(
        event_count(&fixture.state_dir, &fixture.run_id, "auto_apply.skipped"),
        0
    );
    drop(lock_file);
}

/// A never-auto-applied result accepted by a human: `accept` applies it
/// through the `ApplyAuthority::Human` path.
#[test]
fn human_apply_still_records_human_authority_and_accepted_review() {
    let fixture = Fixture::new("checks:\n  verify: ['true']\n");

    fixture.apply_cli().success();

    let metadata = fixture.metadata();
    assert_eq!(metadata["outcome"]["applied_by"], "human");
    assert_eq!(metadata["outcome"]["review"], "accepted");
    assert!(fixture.fake_file().is_file());
}

/// An immediate-delivery allocation run: single profile, a shell fixture
/// harness that delivers on its first invocation, modeled on
/// `tests/profile_selection.rs`. Allocation mode is required here (unlike
/// `Fixture`) because `dispatch accept`/`dispatch reject` only work for an
/// allocation or routed run.
struct AllocationFixture {
    _temp: tempfile::TempDir,
    source: PathBuf,
    state_dir: PathBuf,
}

impl AllocationFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let state_dir = temp.path().join("state");
        fs::create_dir_all(source.join("src")).unwrap();
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(source.join("src/lib.rs"), "pub fn original() {}\n").unwrap();
        let agent = temp.path().join("codex");
        fs::write(
            &agent,
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then printf 'codex fixture 1.0\\n'; exit 0; fi\n\
             if [ \"$1\" = 'app-server' ]; then\n\
             while IFS= read -r line; do\n\
             case \"$line\" in\n\
             *'\"id\":0'*) printf '%s\\n' '{\"id\":0,\"result\":{\"userAgent\":\"fixture\"}}' ;;\n\
             *'\"id\":1'*) printf '%s\\n' '{\"id\":1,\"result\":{\"account\":{\"type\":\"chatgpt\",\"planType\":\"plus\",\"email\":\"fixture@example.invalid\"}}}' ;;\n\
             *'\"id\":2'*) printf '%s\\n' '{\"id\":2,\"result\":{\"rateLimitsByLimitId\":{}}}'; exit 0 ;;\n\
             esac\n\
             done\n\
             exit 0\n\
             fi\n\
             printf 'delivered\\n' > delivered.txt\n\
             printf '{\"type\":\"result\",\"model\":\"observed-model\"}\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            source.join("dispatch.yml"),
            format!(
                "execution:\n  timeout_secs: 600\nchecks:\n  verify:\n    - test -f delivered.txt\nharnesses:\n  codex:\n    executable: \"{}\"\n",
                agent.display()
            ),
        )
        .unwrap();
        fs::write(
            state_dir.join("resources.yml"),
            "version: 1\nallocation_enabled: true\nprofiles:\n  - provider: openai\n    funding_source: chatgpt-plus\n    harness: codex\n    model: configured-model\n    effort: high\n    service_mode: standard\n    runtime: local\n    pool: fixture\n    tier: strong\n    included: true\n    no_overage_verified: true\n    codex_account: {\"account_sha256\":\"cc6d96611cffa9f02c3626f0b9ee897dc171e2d540a5cae349d4ec316104997b\",\"checked_at\":\"2026-01-01T00:00:00Z\"}\n",
        )
        .unwrap();
        Self {
            _temp: temp,
            source,
            state_dir,
        }
    }

    fn run(&self) -> String {
        let output = cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&self.state_dir)
            .arg("run")
            .arg(&self.source)
            .args([
                "--task",
                "Deliver something.",
                "--allow-unsafe-local",
                "--json",
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let value: Value = serde_json::from_slice(&output).unwrap();
        value["run_id"].as_str().unwrap().to_owned()
    }

    fn state(&self) -> State {
        State {
            root: self.state_dir.clone(),
        }
    }
}

#[test]
fn post_hoc_accept_after_auto_apply_records_review_without_reapplying() {
    let fixture = AllocationFixture::new();
    let run_id = fixture.run();

    let outcome = orchestrator::auto_apply(&fixture.state(), &run_id).unwrap();
    assert!(
        matches!(outcome, ApplyOutcome::Applied { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        event_count(&fixture.state_dir, &run_id, "result.applied"),
        1
    );

    let assert = cargo_bin_cmd!("dispatch")
        .arg("--state-dir")
        .arg(&fixture.state_dir)
        .args(["accept", &run_id])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("already applied by auto-apply"),
        "unexpected stdout: {stdout}"
    );

    assert_eq!(
        event_count(&fixture.state_dir, &run_id, "review.accepted"),
        1
    );
    // No second application.
    assert_eq!(
        event_count(&fixture.state_dir, &run_id, "result.applied"),
        1
    );
    assert_eq!(feedback_count(&fixture.state_dir, &run_id), 1);
    let metadata = metadata(&fixture.state_dir, &run_id);
    assert_eq!(metadata["outcome"]["review"], "accepted");
    assert_eq!(metadata["outcome"]["application"], "applied");
    assert_eq!(metadata["outcome"]["applied_by"], "auto_apply");
    assert!(fixture.source.join("delivered.txt").is_file());
}

#[test]
fn post_hoc_reject_after_auto_apply_does_not_revert_the_source() {
    let fixture = AllocationFixture::new();
    let run_id = fixture.run();

    let outcome = orchestrator::auto_apply(&fixture.state(), &run_id).unwrap();
    assert!(
        matches!(outcome, ApplyOutcome::Applied { .. }),
        "{outcome:?}"
    );

    let assert = cargo_bin_cmd!("dispatch")
        .arg("--state-dir")
        .arg(&fixture.state_dir)
        .args(["reject", &run_id])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("Rejection does not revert the source"),
        "unexpected stdout: {stdout}"
    );

    assert_eq!(
        event_count(&fixture.state_dir, &run_id, "review.rejected"),
        1
    );
    let metadata = metadata(&fixture.state_dir, &run_id);
    assert_eq!(metadata["outcome"]["review"], "rejected");
    assert_eq!(metadata["outcome"]["application"], "applied");
    assert!(fixture.source.join("delivered.txt").is_file());
}
