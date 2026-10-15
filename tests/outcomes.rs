#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use assert_cmd::cargo_bin_cmd;
use chrono::Utc;
use dispatch::{EventRecord, ReviewState, RunRecord, db::Database, state::State};
use serde_json::Value;

fn fixture(
    root: &Path,
    semantic_error: bool,
    check: &str,
) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    let source = root.join("source");
    let state = root.join("state");
    fs::create_dir_all(&source)?;
    fs::write(source.join("original.txt"), "original\n")?;
    let agent = root.join("cursor-fixture");
    let body = if semantic_error {
        "printf '{\"type\":\"turn.failed\",\"message\":\"provider rejected request\",\"model\":\"observed-model\"}\\n'\n"
    } else {
        "printf 'changed\\n' > result.txt\nprintf '{\"type\":\"result\",\"model\":\"observed-model\",\"reasoning_effort\":\"high\"}\\n'\n"
    };
    fs::write(
        &agent,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'fixture 1.0\\n'; exit 0; fi\n{body}"
        ),
    )?;
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755))?;
    fs::write(
        source.join("dispatch.yml"),
        format!(
            "execution:\n  timeout_secs: 600\nchecks:\n  verify:\n    - {check}\nharnesses:\n  cursor:\n    executable: \"{}\"\n    model: requested-model\n",
            agent.display()
        ),
    )?;
    Ok((source, state))
}

fn json_run(source: &Path, state: &Path) -> assert_cmd::Command {
    let mut command = cargo_bin_cmd!("dispatch");
    command
        .args(["--state-dir"])
        .arg(state)
        .arg("run")
        .arg(source)
        .args([
            "--task",
            "Exercise truthful Phase 0 outcomes.",
            "--agent",
            "cursor",
            "--allow-unsafe-local",
            "--json",
        ]);
    command
}

#[test]
fn json_result_is_pure_and_preserves_requested_and_observed_identity() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (source, state) = fixture(temp.path(), false, "test -f result.txt")?;
    let output = json_run(&source, &state)
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8(output.stdout)?;
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    let result: Value = serde_json::from_str(stdout.trim())?;
    assert_eq!(result["schema_version"], 1);
    assert_eq!(result["outcome"]["work_result"], "ready");
    assert_eq!(result["outcome"]["verification"], "passed");
    assert_eq!(result["outcome"]["phase"], "reviewing");
    assert_eq!(result["outcome"]["waiting_on"], "none");
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["attempts"][0]["requested_model"], "requested-model");
    assert_eq!(result["attempts"][0]["resolved_model"], "requested-model");
    assert_eq!(result["attempts"][0]["observed_model"], "observed-model");
    assert_eq!(result["attempts"][0]["observed_effort"], "high");
    Ok(())
}

#[test]
fn run_jsonl_stream_contains_only_ordered_events_and_one_result() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (source, state) = fixture(temp.path(), false, "test -f result.txt")?;
    let output = cargo_bin_cmd!("dispatch")
        .args(["--state-dir"])
        .arg(&state)
        .arg("run")
        .arg(&source)
        .args([
            "--task",
            "Exercise the Phase 0 event stream.",
            "--agent",
            "cursor",
            "--allow-unsafe-local",
            "--jsonl",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let values = String::from_utf8(output)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(values.last().unwrap()["type"], "result");
    assert!(
        values[..values.len() - 1]
            .iter()
            .enumerate()
            .all(|(index, value)| {
                value["type"] == "event" && value["event"]["sequence"] == index + 1
            })
    );
    Ok(())
}

#[test]
fn verification_failure_is_exit_three_and_unconfigured_is_explicit() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (source, state) = fixture(temp.path(), false, "test -f absent.txt")?;
    let output = json_run(&source, &state)
        .assert()
        .code(3)
        .get_output()
        .clone();
    let result: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["outcome"]["work_result"], "ready");
    assert_eq!(result["outcome"]["verification"], "failed");
    assert_eq!(result["exit_code"], 3);

    let temp = tempfile::tempdir()?;
    let (source, state) = fixture(temp.path(), false, "true")?;
    fs::write(
        source.join("dispatch.yml"),
        fs::read_to_string(source.join("dispatch.yml"))?
            .replace("  verify:\n    - true", "  verify: []"),
    )?;
    let output = json_run(&source, &state)
        .assert()
        .success()
        .get_output()
        .clone();
    let result: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["outcome"]["verification"], "not_configured");
    Ok(())
}

#[test]
fn semantic_harness_error_with_process_exit_zero_fails_the_attempt() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (source, state) = fixture(temp.path(), true, "true")?;
    let output = json_run(&source, &state)
        .assert()
        .code(1)
        .get_output()
        .clone();
    let result: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["outcome"]["work_result"], "failed");
    assert_eq!(result["outcome"]["verification"], "not_run");
    assert_eq!(result["attempts"][0]["outcome"], "failed");
    Ok(())
}

#[test]
fn jsonl_replay_is_ordered_and_repairs_a_missing_metadata_projection() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (source, state) = fixture(temp.path(), false, "test -f result.txt")?;
    let output = json_run(&source, &state)
        .assert()
        .success()
        .get_output()
        .clone();
    let result: Value = serde_json::from_slice(&output.stdout)?;
    let run_id = result["run_id"].as_str().unwrap();
    fs::remove_file(state.join("runs").join(run_id).join("metadata.json"))?;

    let replay = cargo_bin_cmd!("dispatch")
        .args(["--state-dir"])
        .arg(&state)
        .args(["status", run_id, "--jsonl"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let values = String::from_utf8(replay)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(values.last().unwrap()["type"], "result");
    let events = &values[..values.len() - 1];
    assert!(!events.is_empty());
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event["type"], "event");
        assert_eq!(event["event"]["protocol_version"], 1);
        assert_eq!(event["event"]["sequence"], index + 1);
    }
    let kinds = events
        .iter()
        .map(|event| event["event"]["event_type"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(
        kinds
            .windows(2)
            .any(|pair| pair == ["check.started", "check.finished"])
    );
    assert!(
        state
            .join("runs")
            .join(run_id)
            .join("metadata.json")
            .is_file()
    );
    Ok(())
}

#[test]
fn committed_transition_recovers_after_projection_write_failure() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (source, state) = fixture(temp.path(), false, "test -f result.txt")?;
    let output = json_run(&source, &state)
        .assert()
        .success()
        .get_output()
        .clone();
    let result: Value = serde_json::from_slice(&output.stdout)?;
    let run_id = result["run_id"].as_str().unwrap();
    let metadata = state.join("runs").join(run_id).join("metadata.json");
    let mut run: RunRecord = serde_json::from_slice(&fs::read(&metadata)?)?;
    fs::remove_file(&metadata)?;
    fs::create_dir(&metadata)?;

    run.outcome.review = ReviewState::Rejected;
    let database = Database::open(state.join("dispatch.db"))?;
    database.commit_transition(
        &mut run,
        EventRecord {
            run_id: run_id.into(),
            event_type: "review.rejected".into(),
            timestamp: Utc::now(),
            ..EventRecord::default()
        },
    )?;
    let projected_state = State {
        root: state.clone(),
    };
    assert!(projected_state.save_run(&run).is_err());
    fs::remove_dir(&metadata)?;

    let recovered = projected_state.load_run(run_id)?;
    assert_eq!(recovered.outcome.review, ReviewState::Rejected);
    assert!(metadata.is_file());
    Ok(())
}
