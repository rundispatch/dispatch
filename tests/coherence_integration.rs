//! The L2 integration oracle: when the source moved but the static layers see
//! no conflict, the run's `checks.verify` commands run against the merged tree
//! before anything is applied. Uses the built-in `fake-good` harness, which
//! creates one empty file, `dispatch-fake-good.txt`.
#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

use assert_cmd::cargo_bin_cmd;
use serde_json::Value;

struct Fixture {
    _temp: tempfile::TempDir,
    source: PathBuf,
    state: PathBuf,
    run_id: String,
}

impl Fixture {
    /// `config` is appended to a `dispatch.yml` that sets a 30 second timeout.
    fn new(config: &str) -> Self {
        Self::with_source(config, |_| {})
    }

    /// Like `new`, with a Git repository as the source: `setup` adds files
    /// before everything is committed.
    fn new_git(config: &str, setup: impl FnOnce(&std::path::Path)) -> Self {
        Self::with_source(config, |source| {
            setup(source);
            for args in [
                &["init", "--quiet"][..],
                &["add", "-A"],
                &[
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "initial",
                ],
            ] {
                let status = Command::new("git")
                    .arg("-C")
                    .arg(source)
                    .args(args)
                    .env_remove("GIT_DIR")
                    .env_remove("GIT_WORK_TREE")
                    .status()
                    .unwrap();
                assert!(status.success(), "git {args:?} failed");
            }
        })
    }

    fn with_source(config: &str, setup: impl FnOnce(&std::path::Path)) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let state = temp.path().join("state");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("original.txt"), "baseline\n").unwrap();
        fs::write(
            source.join("dispatch.yml"),
            format!("execution:\n  timeout_secs: 600\n{config}"),
        )
        .unwrap();
        setup(&source);
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

    fn run_dir(&self) -> PathBuf {
        self.state.join("runs").join(&self.run_id)
    }

    fn metadata(&self) -> Value {
        serde_json::from_slice(&fs::read(self.run_dir().join("metadata.json")).unwrap()).unwrap()
    }

    fn dispatch(&self, args: &[&str]) -> std::process::Output {
        cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&self.state)
            .args(args)
            .current_dir(&self.source)
            .output()
            .unwrap()
    }

    fn apply(&self) -> assert_cmd::assert::Assert {
        cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&self.state)
            .args(["accept", &self.run_id])
            .assert()
    }

    /// Names directly under the run directory that start with `prefix`.
    fn run_dir_entries(&self, prefix: &str) -> Vec<String> {
        fs::read_dir(self.run_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(prefix))
            .collect()
    }

    fn assert_no_scratch(&self) {
        assert_eq!(
            self.run_dir_entries("coherence-scratch-"),
            Vec::<String>::new()
        );
    }

    fn analysis(&self) -> Value {
        self.metadata()["coherence"]["validity"]["analysis"].clone()
    }
}

const VERIFY_NO_FORBIDDEN: &str = "checks:\n  verify:\n    - test ! -f forbidden.txt\n";

/// The world adds a file that only the merged tree's check objects to: the
/// check passes at the baseline and for the patch alone.
fn add_forbidden(fixture: &Fixture) {
    fs::write(
        fixture.source.join("forbidden.txt"),
        "added by someone else\n",
    )
    .unwrap();
}

#[test]
fn passing_check_on_the_merged_tree_applies_and_records_integration() {
    let fixture = Fixture::new("checks:\n  verify:\n    - test -f original.txt\n");
    fs::write(fixture.source.join("notes.txt"), "someone else's work\n").unwrap();

    let applied = fixture.apply().success();

    let stdout = String::from_utf8_lossy(&applied.get_output().stdout).into_owned();
    let logs = stdout
        .lines()
        .find_map(|line| {
            line.strip_prefix("Merged checks: passed on the current source with this patch; logs: ")
        })
        .unwrap_or_else(|| panic!("{stdout}"));
    assert!(PathBuf::from(logs).is_dir(), "{stdout}");
    assert!(fixture.source.join("dispatch-fake-good.txt").is_file());
    assert!(fixture.source.join("notes.txt").is_file());
    let metadata = fixture.metadata();
    assert_eq!(metadata["status"], "applied");
    assert_eq!(fixture.analysis(), "integration");
    assert_eq!(metadata["coherence"]["validity"]["decision"], "continue");
    fixture.assert_no_scratch();
    assert!(fixture.run_dir().join("coherence-checks").is_dir());
}

#[test]
fn check_failing_only_on_the_merged_tree_blocks_apply() {
    let fixture = Fixture::new(VERIFY_NO_FORBIDDEN);
    add_forbidden(&fixture);

    let assert = fixture.apply().failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("stale"), "unexpected error: {stderr}");
    assert!(
        stderr.contains("test ! -f forbidden.txt") && stderr.contains("exit 1"),
        "unexpected error: {stderr}"
    );
    assert!(!stderr.contains("Merged checks: not run"), "{stderr}");
    assert!(!fixture.source.join("dispatch-fake-good.txt").exists());
    assert!(fixture.source.join("forbidden.txt").is_file());
    let metadata = fixture.metadata();
    assert_ne!(metadata["status"], "applied");
    assert_eq!(
        metadata["outcome"]["application"],
        "blocked_by_source_drift"
    );
    let validity = &metadata["coherence"]["validity"];
    assert_eq!(validity["decision"], "refresh");
    assert_eq!(validity["reasons"][0]["code"], "integration_check_failed");
    fixture.assert_no_scratch();

    // The logs named in the reason are kept as evidence.
    let detail = validity["reasons"][0]["detail"].as_str().unwrap();
    let log = detail.rsplit("logs: ").next().unwrap();
    assert!(
        PathBuf::from(log).starts_with(fixture.run_dir().join("coherence-checks")),
        "{detail}"
    );
    assert!(PathBuf::from(log).is_file(), "{detail}");

    // Every surface shows that refusal for this world, and refresh passes it
    // on to the new agent.
    let check = fixture.dispatch(&["check", &fixture.run_id, "--json"]);
    let shown: Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(shown["validity"]["decision"], "refresh", "{shown}");
    assert_eq!(
        shown["validity"]["reasons"][0]["code"],
        "integration_check_failed"
    );
    let human = String::from_utf8(fixture.dispatch(&["check", &fixture.run_id]).stdout).unwrap();
    assert!(human.contains("Coherence: REFRESH"), "{human}");
    assert!(human.contains("Next: dispatch refresh"), "{human}");
    let status = fixture.dispatch(&["status", &fixture.run_id, "--json"]);
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["coherence"]["decision"], "refresh", "{status}");
    let refreshed =
        fixture.dispatch(&["refresh", &fixture.run_id, "--allow-unsafe-local", "--json"]);
    let refreshed: Value = serde_json::from_slice(&refreshed.stdout).unwrap();
    let new_run = fixture
        .state
        .join("runs")
        .join(refreshed["run_id"].as_str().unwrap())
        .join("metadata.json");
    let new_run: Value = serde_json::from_slice(&fs::read(new_run).unwrap()).unwrap();
    assert!(
        new_run["task"]
            .as_str()
            .unwrap()
            .contains("check `test ! -f forbidden.txt` failed"),
        "{}",
        new_run["task"]
    );

    // Once the world moves again, the fresh verdict stands until the next
    // accept runs the checks on the new merged tree.
    fs::write(fixture.source.join("unrelated.txt"), "moved\n").unwrap();
    let check = fixture.dispatch(&["check", &fixture.run_id, "--json"]);
    let shown: Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(shown["validity"]["decision"], "continue", "{shown}");
}

/// `dispatch serve --json` on the fixture's source, killed on drop.
struct Serve(
    std::process::Child,
    std::io::Lines<std::io::BufReader<std::process::ChildStdout>>,
);

impl Serve {
    fn spawn(fixture: &Fixture) -> Self {
        let mut child = Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&fixture.state)
            .args(["serve", "--json", "--root"])
            .arg(&fixture.source)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let lines = std::io::BufRead::lines(std::io::BufReader::new(child.stdout.take().unwrap()));
        Self(child, lines)
    }

    /// Blocks until serve reports a world, which it does after its tick.
    fn world(&mut self) {
        for line in self.1.by_ref() {
            if line.unwrap().contains(r#""type":"world""#) {
                return;
            }
        }
        panic!("serve exited");
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    while !ready() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn the_owner_keeps_a_refusal_by_the_checks_until_the_world_moves() {
    let fixture = Fixture::new(&format!(
        "{VERIFY_NO_FORBIDDEN}coherence:\n  poll_secs: 1\n"
    ));
    add_forbidden(&fixture);
    fixture.apply().failure();

    let mut serve = Serve::spawn(&fixture);
    serve.world();
    // A few more ticks with the world unmoved leave the refusal in place.
    std::thread::sleep(std::time::Duration::from_secs(3));
    let validity = &fixture.metadata()["coherence"]["validity"];
    assert_eq!(
        validity["reasons"][0]["code"], "integration_check_failed",
        "{validity}"
    );

    fs::write(fixture.source.join("unrelated.txt"), "moved\n").unwrap();
    wait_for("the owner to record CONTINUE", || {
        fixture.metadata()["coherence"]["validity"]["decision"] == "continue"
    });
}

#[test]
fn the_owner_marks_a_ready_native_result_stale_when_the_world_moves() {
    let fixture = Fixture::new("coherence:\n  poll_secs: 1\n");
    let mut serve = Serve::spawn(&fixture);
    serve.world();
    // The world creates the file the result adds, with other content.
    fs::write(
        fixture.source.join("dispatch-fake-good.txt"),
        "someone else's\n",
    )
    .unwrap();
    wait_for("the owner to record REFRESH", || {
        fixture.metadata()["coherence"]["validity"]["decision"] == "refresh"
    });
    assert_eq!(fixture.metadata()["outcome"]["review"], "pending");
}

/// What accept printed about the merged checks.
fn merged_line(applied: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&applied.get_output().stdout)
        .lines()
        .find(|line| line.starts_with("Merged checks: "))
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn disabling_integration_checks_skips_the_oracle() {
    let fixture = Fixture::new(&format!(
        "{VERIFY_NO_FORBIDDEN}coherence:\n  integration_checks: false\n"
    ));
    add_forbidden(&fixture);

    let applied = fixture.apply().success();

    assert_eq!(
        merged_line(&applied),
        "Merged checks: not run — they are turned off (coherence.integration_checks: false)"
    );
    assert!(fixture.source.join("dispatch-fake-good.txt").is_file());
    assert_eq!(fixture.analysis(), "files_only");
    assert!(!fixture.run_dir().join("coherence-checks").exists());
    fixture.assert_no_scratch();
}

#[test]
fn no_verify_checks_means_nothing_to_run() {
    let fixture = Fixture::new("");
    add_forbidden(&fixture);

    let applied = fixture.apply().success();

    assert_eq!(
        merged_line(&applied),
        "Merged checks: not run — no checks are configured (checks.verify is empty)"
    );
    assert!(fixture.source.join("dispatch-fake-good.txt").is_file());
    assert_eq!(fixture.analysis(), "files_only");
    assert!(!fixture.run_dir().join("coherence-checks").exists());
    fixture.assert_no_scratch();
}

#[test]
fn unmoved_source_never_creates_a_scratch_tree() {
    let fixture = Fixture::new(VERIFY_NO_FORBIDDEN);

    let applied = fixture.apply().success();

    assert_eq!(
        merged_line(&applied),
        "Merged checks: not run — the source has not moved since the Work began, so the merged result is the Work's own tree"
    );
    assert!(fixture.source.join("dispatch-fake-good.txt").is_file());
    assert!(fixture.metadata()["coherence"].is_null());
    assert!(fixture.run_dir_entries("coherence-").is_empty());
}

#[test]
fn hanging_check_times_out_and_refreshes() {
    let fixture = Fixture::new("checks:\n  verify:\n    - if [ -f hang.txt ]; then sleep 60; fi\n");
    // The timeout applies to every verify command, including this one.
    let snapshot = fixture.run_dir().join("config.snapshot.yml");
    let text = fs::read_to_string(&snapshot).unwrap();
    assert!(text.contains("timeout_secs: 600"), "{text}");
    fs::write(
        &snapshot,
        text.replace("timeout_secs: 600", "timeout_secs: 2"),
    )
    .unwrap();
    fs::write(fixture.source.join("hang.txt"), "hang\n").unwrap();

    let assert = fixture.apply().failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("stale"), "unexpected error: {stderr}");
    assert!(stderr.contains("timed out"), "unexpected error: {stderr}");
    assert!(!fixture.source.join("dispatch-fake-good.txt").exists());
    assert_eq!(
        fixture.metadata()["coherence"]["validity"]["reasons"][0]["code"],
        "integration_check_failed"
    );
    fixture.assert_no_scratch();
}

#[test]
fn a_run_without_local_execution_authority_is_never_verified() {
    let fixture = Fixture::new("");
    // Grant checks after the fact: the run recorded no unsafe-local
    // acknowledgement, so accept time must not run them.
    let snapshot = fixture.run_dir().join("config.snapshot.yml");
    let text = fs::read_to_string(&snapshot).unwrap();
    assert!(text.contains("verify: []"), "{text}");
    fs::write(
        &snapshot,
        text.replace("verify: []", "verify:\n  - 'false'"),
    )
    .unwrap();
    add_forbidden(&fixture);

    let applied = fixture.apply().success();

    assert_eq!(
        merged_line(&applied),
        "Merged checks: not run — this Work has no recorded permission to run commands on the host (--allow-unsafe-local)"
    );
    assert!(fixture.source.join("dispatch-fake-good.txt").is_file());
    assert_eq!(fixture.analysis(), "files_only");
    assert!(!fixture.run_dir().join("coherence-checks").exists());
    fixture.assert_no_scratch();
}

/// Bytes of ignored build output written into the source after the run.
const BIG_IGNORED_BYTES: usize = 20 * 1024 * 1024;

/// Ignored output that appears after the run never reaches the scratch tree,
/// while the world still moves through an untracked file that does.
#[test]
fn scratch_tree_of_a_git_source_omits_ignored_build_output() {
    let fixture = Fixture::new_git(
        "checks:\n  verify:\n    - test ! -e target/big.bin && test -f original.txt\n",
        |source| fs::write(source.join(".gitignore"), "target/\n").unwrap(),
    );
    fs::create_dir_all(fixture.source.join("target")).unwrap();
    fs::write(
        fixture.source.join("target/big.bin"),
        vec![7_u8; BIG_IGNORED_BYTES],
    )
    .unwrap();
    fs::write(fixture.source.join("notes.txt"), "someone else's work\n").unwrap();

    fixture.apply().success();

    assert!(fixture.source.join("dispatch-fake-good.txt").is_file());
    assert!(fixture.source.join("target/big.bin").is_file());
    assert_eq!(fixture.analysis(), "integration");
    fixture.assert_no_scratch();
}

/// Execute bits and symlinks in a Git source reach the scratch tree.
#[test]
fn scratch_tree_of_a_git_source_keeps_modes_and_symlinks() {
    let fixture = Fixture::new_git(
        "checks:\n  verify:\n    - test -x script.sh && test -L link && test \"$(readlink link)\" = script.sh && test -f nested/file.txt\n",
        |source| {
            fs::write(source.join("script.sh"), "#!/bin/sh\n").unwrap();
            fs::set_permissions(source.join("script.sh"), fs::Permissions::from_mode(0o755))
                .unwrap();
            fs::create_dir_all(source.join("nested")).unwrap();
            fs::write(source.join("nested/file.txt"), "x\n").unwrap();
            std::os::unix::fs::symlink("script.sh", source.join("link")).unwrap();
        },
    );
    add_forbidden(&fixture);

    fixture.apply().success();

    assert!(fixture.source.join("dispatch-fake-good.txt").is_file());
    assert_eq!(fixture.analysis(), "integration");
    fixture.assert_no_scratch();
}

#[test]
fn scratch_tree_of_a_directory_source_keeps_modes_and_symlinks() {
    let fixture = Fixture::with_source(
        "checks:\n  verify:\n    - test -x script.sh && test -L link && test -f nested/file.txt\n",
        |source| {
            fs::write(source.join("script.sh"), "#!/bin/sh\n").unwrap();
            fs::set_permissions(source.join("script.sh"), fs::Permissions::from_mode(0o755))
                .unwrap();
            std::os::unix::fs::symlink("script.sh", source.join("link")).unwrap();
            fs::create_dir_all(source.join("nested")).unwrap();
            fs::write(source.join("nested/file.txt"), "x\n").unwrap();
        },
    );
    add_forbidden(&fixture);

    fixture.apply().success();

    assert!(fixture.source.join("dispatch-fake-good.txt").is_file());
    assert_eq!(fixture.analysis(), "integration");
    fixture.assert_no_scratch();
}
