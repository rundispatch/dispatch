//! Acceptance against a source that moved after the run's snapshot: harmless
//! drift no longer blocks apply, invalidating drift still does, and strict mode
//! keeps the old any-drift refusal. Uses the built-in `fake-good` harness, which
//! creates one empty file, `dispatch-fake-good.txt`.
#![cfg(unix)]

use std::{fs, path::PathBuf, process::Command};

use assert_cmd::cargo_bin_cmd;
use serde_json::Value;

struct Fixture {
    _temp: tempfile::TempDir,
    source: PathBuf,
    state: PathBuf,
    run_id: String,
}

impl Fixture {
    fn new(git: bool, extra_config: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let state = temp.path().join("state");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("original.txt"), "baseline\n").unwrap();
        fs::write(
            source.join("dispatch.yml"),
            format!("execution:\n  timeout_secs: 30\n{extra_config}"),
        )
        .unwrap();
        if git {
            fs::write(source.join(".gitignore"), "target/\n").unwrap();
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
        }
        let output = cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&state)
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
            .success()
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
            state,
            run_id,
        }
    }

    fn metadata(&self) -> Value {
        let path = self
            .state
            .join("runs")
            .join(&self.run_id)
            .join("metadata.json");
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    /// A refused accept records no human review: the review stays pending,
    /// with no review revision and no `review.accepted` event.
    fn assert_review_still_pending(&self) {
        assert_eq!(self.metadata()["outcome"]["review"], "pending");
        let db = rusqlite::Connection::open(self.state.join("dispatch.db")).unwrap();
        let count =
            |sql: &str| -> i64 { db.query_row(sql, [&self.run_id], |row| row.get(0)).unwrap() };
        assert_eq!(
            count("SELECT COUNT(*) FROM goal_feedback_revisions WHERE run_id = ?1"),
            0
        );
        assert_eq!(
            count(
                "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_type = 'review.accepted'"
            ),
            0
        );
    }

    /// Another run of the same task from the current source.
    fn another_run(&self) -> String {
        let output = cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&self.state)
            .arg("run")
            .arg(&self.source)
            .arg("--allow-unsafe-local")
            .args([
                "--task",
                "Create the fake artifact.",
                "--agent",
                "fake-good",
            ])
            .assert()
            .success()
            .get_output()
            .clone();
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("RUN "))
            .expect("run output includes an ID")
            .to_owned()
    }

    fn apply(&self) -> assert_cmd::assert::Assert {
        cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&self.state)
            .args(["accept", &self.run_id])
            .assert()
    }
}

#[test]
fn unrelated_source_edit_no_longer_blocks_apply() {
    let fixture = Fixture::new(false, "");
    fs::write(fixture.source.join("notes.txt"), "someone else's work\n").unwrap();
    fs::write(fixture.source.join("original.txt"), "edited elsewhere\n").unwrap();

    fixture.apply().success();

    assert!(fixture.source.join("dispatch-fake-good.txt").is_file());
    assert_eq!(
        fs::read_to_string(fixture.source.join("original.txt")).unwrap(),
        "edited elsewhere\n"
    );
    assert_eq!(fixture.metadata()["status"], "applied");
}

#[test]
fn ignored_build_output_is_not_drift() {
    let fixture = Fixture::new(true, "");
    fs::create_dir_all(fixture.source.join("target")).unwrap();
    fs::write(
        fixture.source.join("target/out.bin"),
        "built after the run\n",
    )
    .unwrap();

    fixture.apply().success();

    assert!(fixture.source.join("dispatch-fake-good.txt").is_file());
    assert_eq!(fixture.metadata()["status"], "applied");
}

#[test]
fn conflicting_source_change_blocks_apply_and_leaves_source_unchanged() {
    let fixture = Fixture::new(false, "");
    let occupied = fixture.source.join("dispatch-fake-good.txt");
    fs::write(&occupied, "someone else wrote this first\n").unwrap();

    let assert = fixture.apply().failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("stale"), "unexpected error: {stderr}");
    assert_eq!(
        fs::read_to_string(&occupied).unwrap(),
        "someone else wrote this first\n"
    );
    let metadata = fixture.metadata();
    assert_ne!(metadata["status"], "applied");
    assert_eq!(
        metadata["outcome"]["application"],
        "blocked_by_source_drift"
    );
    assert_eq!(metadata["coherence"]["validity"]["decision"], "refresh");
    assert_eq!(
        metadata["coherence"]["validity"]["reasons"][0]["code"],
        "patch_conflict"
    );
    // The refusal names the file, says the merged checks never ran, and keeps
    // Git's whole complaint beside the run.
    let reason = &metadata["coherence"]["validity"]["reasons"][0];
    assert_eq!(reason["path"], "dispatch-fake-good.txt");
    assert!(
        reason["detail"]
            .as_str()
            .unwrap()
            .contains("patch-conflict.txt"),
        "{reason}"
    );
    let full = fs::read_to_string(
        fixture
            .state
            .join("runs")
            .join(&fixture.run_id)
            .join("patch-conflict.txt"),
    )
    .unwrap();
    assert!(
        full.contains("error: dispatch-fake-good.txt: already exists in working directory"),
        "{full}"
    );
    assert!(
        stderr.contains("Merged checks: not run — accept refuses REFRESH work before them"),
        "{stderr}"
    );
    fixture.assert_review_still_pending();
    // Still the latest pending result: a bare accept finds it again.
    let bare = cargo_bin_cmd!("dispatch")
        .arg("--state-dir")
        .arg(&fixture.state)
        .arg("accept")
        .current_dir(&fixture.source)
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&bare.get_output().stderr).into_owned();
    assert!(stderr.contains("stale"), "unexpected error: {stderr}");
}

#[test]
fn patch_already_present_in_source_stops() {
    let fixture = Fixture::new(false, "");
    fs::write(fixture.source.join("dispatch-fake-good.txt"), "").unwrap();

    let assert = fixture.apply().failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("STOP"), "unexpected error: {stderr}");
    assert_eq!(
        fixture.metadata()["coherence"]["validity"]["decision"],
        "stop"
    );
    fixture.assert_review_still_pending();
}

#[test]
fn stop_names_the_run_that_already_landed_the_change() {
    let fixture = Fixture::new(false, "");
    let second = fixture.another_run();
    fixture.apply().success();

    let check = cargo_bin_cmd!("dispatch")
        .arg("--state-dir")
        .arg(&fixture.state)
        .args(["check", &second, "--json"])
        .output()
        .unwrap();
    let shown: Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(shown["validity"]["decision"], "stop", "{shown}");
    assert_eq!(shown["landed_by"], fixture.run_id.as_str(), "{shown}");

    let accept = cargo_bin_cmd!("dispatch")
        .arg("--state-dir")
        .arg(&fixture.state)
        .args(["accept", &second])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&accept.get_output().stderr).into_owned();
    assert!(
        stderr.contains(&format!("(landed by run {})", fixture.run_id)),
        "unexpected error: {stderr}"
    );
}

#[test]
fn strict_mode_keeps_the_any_drift_refusal() {
    let fixture = Fixture::new(false, "coherence:\n  accept: strict\n");
    fs::write(fixture.source.join("notes.txt"), "unrelated\n").unwrap();

    let assert = fixture.apply().failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("source has changed since this run was created"),
        "unexpected error: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("Merged checks: not run — accept is strict (coherence.accept: strict)"),
        "{stdout}"
    );
    assert!(!fixture.source.join("dispatch-fake-good.txt").exists());
    assert_eq!(
        fixture.metadata()["outcome"]["application"],
        "blocked_by_source_drift"
    );
    assert!(fixture.metadata()["coherence"].is_null());
}

/// A frozen configuration that cannot be read makes accept strict, and
/// accept says that is why the merged checks did not run.
#[test]
fn an_unreadable_configuration_makes_accept_strict_and_says_so() {
    let fixture = Fixture::new(false, "");
    fs::write(
        fixture
            .state
            .join("runs")
            .join(&fixture.run_id)
            .join("config.snapshot.yml"),
        "coherence: [not a map",
    )
    .unwrap();

    let assert = fixture.apply().success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains(
            "Merged checks: not run — the run's frozen configuration cannot be read, so accept is strict"
        ),
        "{stdout}"
    );
    assert!(fixture.source.join("dispatch-fake-good.txt").is_file());
}

#[test]
fn unmoved_source_applies_exactly_as_before() {
    let fixture = Fixture::new(false, "");

    fixture.apply().success();

    assert!(fixture.source.join("dispatch-fake-good.txt").is_file());
    assert!(fixture.metadata()["coherence"].is_null());
}
