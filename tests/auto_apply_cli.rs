//! `run --auto-apply` / `refresh --auto-apply`: the CLI surface over
//! `orchestrator::auto_apply` (see `docs/plan-0.3-auto-apply-and-attach.md`,
//! parts 5.2, 5.4 and 5.6). Uses the built-in `fake-good` harness, which
//! creates one empty file, `dispatch-fake-good.txt`.
#![cfg(unix)]

use std::{fs, path::PathBuf, process::Command};

use assert_cmd::cargo_bin_cmd;
use serde_json::Value;

/// A Git source with no run yet: each test decides how to invoke `run`.
struct Fixture {
    _temp: tempfile::TempDir,
    source: PathBuf,
    state: PathBuf,
}

impl Fixture {
    /// `extra_config` is extra YAML appended to `dispatch.yml` (`checks:`, ...).
    fn new(extra_config: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let state = temp.path().join("state");
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
        Self {
            _temp: temp,
            source,
            state,
        }
    }

    fn dispatch(&self, args: &[&str]) -> assert_cmd::assert::Assert {
        cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&self.state)
            .current_dir(&self.source)
            .args(args)
            .assert()
    }

    /// `run` against this fixture's source with the fake-good harness, plus
    /// whatever extra flags the test wants (`--auto-apply`, `--json`, ...).
    fn run(&self, extra: &[&str]) -> assert_cmd::assert::Assert {
        let mut args = vec![
            "run",
            self.source.to_str().unwrap(),
            "--allow-unsafe-local",
            "--task",
            "Create the fake artifact.",
            "--agent",
            "fake-good",
        ];
        args.extend_from_slice(extra);
        self.dispatch(&args)
    }

    fn metadata(&self, id: &str) -> Value {
        let path = self.state.join("runs").join(id).join("metadata.json");
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn fake_file(&self) -> PathBuf {
        self.source.join("dispatch-fake-good.txt")
    }

    fn stdout(assert: &assert_cmd::assert::Assert) -> String {
        String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
    }

    fn run_id(stdout: &str) -> String {
        stdout
            .lines()
            .find_map(|line| line.strip_prefix("RUN "))
            .expect("run output includes an ID")
            .trim()
            .to_owned()
    }
}

#[test]
fn run_auto_apply_applies_on_a_fresh_source() {
    let fixture = Fixture::new("checks:\n  verify: ['true']\n");

    let assert = fixture.run(&["--auto-apply"]).success();

    let stdout = Fixture::stdout(&assert);
    assert!(stdout.contains("Auto-applied Candidate"), "{stdout}");
    assert!(fixture.fake_file().exists());
    let run_id = Fixture::run_id(&stdout);
    let metadata = fixture.metadata(&run_id);
    assert_eq!(metadata["outcome"]["applied_by"], "auto_apply");
    assert_eq!(metadata["outcome"]["review"], "pending");
}

#[test]
fn run_auto_apply_json_prints_exactly_one_object() {
    let fixture = Fixture::new("checks:\n  verify: ['true']\n");

    let assert = fixture.run(&["--auto-apply", "--json"]).success();

    let stdout = Fixture::stdout(&assert);
    let value: Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be exactly one JSON object");
    assert_eq!(value["auto_apply"]["outcome"], "applied");
    assert_eq!(value["outcome"]["applied_by"], "auto_apply");
}

#[test]
fn run_auto_apply_jsonl_streams_the_applied_event_then_a_final_summary_line() {
    let fixture = Fixture::new("checks:\n  verify: ['true']\n");

    let assert = fixture.run(&["--auto-apply", "--jsonl"]).success();

    let stdout = Fixture::stdout(&assert);
    let values: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let applied_event = values
        .iter()
        .find(|value| value["type"] == "event" && value["event"]["event_type"] == "result.applied")
        .unwrap_or_else(|| panic!("no result.applied event in {stdout}"));
    assert_eq!(
        applied_event["event"]["payload"]["applied_by"],
        "auto_apply"
    );
    let last = values.last().unwrap();
    assert_eq!(last["type"], "auto_apply");
    assert_eq!(last["auto_apply"]["outcome"], "applied");
}

#[test]
fn run_auto_apply_without_verify_checks_is_exit_six_and_leaves_the_source_alone() {
    let fixture = Fixture::new("");

    let assert = fixture.run(&["--auto-apply"]).code(6);

    let stdout = Fixture::stdout(&assert);
    assert!(
        stdout.contains(
            "Not applied automatically: verification_not_configured. Review with dispatch check"
        ),
        "{stdout}"
    );
    assert!(!fixture.fake_file().exists());
    let run_id = Fixture::run_id(&stdout);
    assert_eq!(fixture.metadata(&run_id)["status"], "ready_for_evaluation");
}

#[test]
fn run_auto_apply_when_verification_fails_keeps_todays_exit_code_and_prints_nothing_extra() {
    let fixture = Fixture::new("checks:\n  verify: ['false']\n");

    let assert = fixture.run(&["--auto-apply"]).code(3);

    let stdout = Fixture::stdout(&assert);
    assert!(!stdout.contains("Auto-applied"), "{stdout}");
    assert!(!stdout.contains("Not applied automatically"), "{stdout}");
}

#[test]
fn run_without_the_flag_is_unchanged() {
    let fixture = Fixture::new("checks:\n  verify: ['true']\n");

    let assert = fixture.run(&[]).success();

    let stdout = Fixture::stdout(&assert);
    assert!(!stdout.contains("Auto-applied"), "{stdout}");
    assert!(!stdout.contains("Not applied automatically"), "{stdout}");
    assert!(!fixture.fake_file().exists());
}

#[test]
fn refresh_auto_apply_reports_whichever_outcome_happened() {
    let fixture = Fixture::new("checks:\n  verify: ['true']\n");
    let old = Fixture::run_id(&Fixture::stdout(&fixture.run(&[]).success()));

    // Someone else's edit lands on the source before refresh: the same
    // conflict shape as `tests/coherence_cli.rs`'s `conflicting_edit`.
    fs::write(
        fixture.source.join("dispatch-fake-good.txt"),
        "someone else wrote this first\n",
    )
    .unwrap();

    let assert = fixture.dispatch(&["refresh", &old, "--auto-apply", "--allow-unsafe-local"]);
    let output = assert.get_output().clone();
    let code = output.status.code().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        code == 0 || code == 6,
        "unexpected exit code {code}: {stdout}"
    );
    if code == 0 {
        assert!(stdout.contains("Auto-applied Candidate"), "{stdout}");
    } else {
        assert!(stdout.contains("Not applied automatically:"), "{stdout}");
    }
}
