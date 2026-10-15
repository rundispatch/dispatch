#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use assert_cmd::cargo_bin_cmd;
use rusqlite::Connection;
use serde_json::Value;

fn fixture(root: &Path) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    let source = root.join("source");
    let state = root.join("state");
    fs::create_dir_all(source.join("src"))?;
    fs::create_dir_all(&state)?;
    fs::write(source.join("src/lib.rs"), "pub fn original() {}\n")?;
    fs::write(source.join("src/main.rs"), "fn main() {}\n")?;
    let agent = root.join("codex");
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
         printf '%s\\n' \"$@\" > invoked-argv.txt\n\
         printf 'allocated\\n' > allocated.txt\n\
         printf '{\"type\":\"result\",\"model\":\"observed-model\",\"reasoning_effort\":\"high\"}\\n'\n",
    )?;
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755))?;
    fs::write(
        source.join("dispatch.yml"),
        format!(
            "execution:\n  timeout_secs: 600\nchecks:\n  verify:\n    - test -f allocated.txt\nharnesses:\n  codex:\n    executable: \"{}\"\n",
            agent.display()
        ),
    )?;
    fs::write(
        state.join("resources.yml"),
        "version: 1\nallocation_enabled: true\nprofiles:\n  - provider: openai\n    funding_source: chatgpt-plus\n    harness: codex\n    model: configured-model\n    effort: high\n    service_mode: standard\n    runtime: local\n    pool: chatgpt-codex\n    tier: strong\n    included: true\n    no_overage_verified: true\n    codex_account: {\"account_sha256\":\"cc6d96611cffa9f02c3626f0b9ee897dc171e2d540a5cae349d4ec316104997b\",\"checked_at\":\"2026-01-01T00:00:00Z\"}\n",
    )?;
    Ok((source, state))
}

#[test]
fn enabled_trial_allocation_reaches_argv_and_persists_identity() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (source, state) = fixture(temp.path())?;
    let output = cargo_bin_cmd!("dispatch")
        .args(["--state-dir"])
        .arg(&state)
        .arg("run")
        .arg(&source)
        .args([
            "--task",
            "Implement a new requested feature.",
            "--allow-unsafe-local",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let result: Value = serde_json::from_slice(&output)?;
    let run_id = result["run_id"].as_str().unwrap();
    assert_eq!(result["mode"], "native");
    assert_eq!(
        result["allocation"]["selected"]["requested_model"],
        "configured-model"
    );
    assert_eq!(result["attempts"][0]["requested_model"], "configured-model");
    assert_eq!(result["attempts"][0]["resolved_effort"], "high");
    assert_eq!(result["attempts"][0]["observed_model"], "observed-model");
    assert_eq!(result["attempts"][0]["resource"]["pool"], "chatgpt-codex");

    let metadata: Value = serde_json::from_slice(&fs::read(
        state.join("runs").join(run_id).join("metadata.json"),
    )?)?;
    let workspace = Path::new(
        metadata["candidates"][0]["workspace_path"]
            .as_str()
            .unwrap(),
    );
    let argv = fs::read_to_string(workspace.join("invoked-argv.txt"))?;
    assert!(argv.contains("--model\nconfigured-model"));
    assert!(argv.contains("model_reasoning_effort=\"high\""));

    let database = Connection::open(state.join("dispatch.db"))?;
    let routing: i64 = database.query_row(
        "SELECT COUNT(*) FROM routing_observations WHERE run_id = ?1",
        [run_id],
        |row| row.get(0),
    )?;
    let allocation: i64 = database.query_row(
        "SELECT COUNT(*) FROM allocation_decisions WHERE run_id = ?1",
        [run_id],
        |row| row.get(0),
    )?;
    assert_eq!((routing, allocation), (0, 1));

    let status: Value = serde_json::from_slice(
        &cargo_bin_cmd!("dispatch")
            .args(["--state-dir"])
            .arg(&state)
            .args(["status", run_id, "--json"])
            .assert()
            .success()
            .get_output()
            .stdout,
    )?;
    assert_eq!(
        status["allocation"]["selected"]["resolved_model"],
        "configured-model"
    );
    let human_status = cargo_bin_cmd!("dispatch")
        .args(["--state-dir"])
        .arg(&state)
        .current_dir(&source)
        .arg("status")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let human_status = String::from_utf8(human_status)?;
    assert!(human_status.contains("Profile: codex · configured-model · high"));
    assert!(!human_status.contains("tier"));
    assert!(human_status.contains("requested: configured-model"));

    let explanation = cargo_bin_cmd!("dispatch")
        .args(["--state-dir"])
        .arg(&state)
        .args(["explain", run_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let explanation = String::from_utf8(explanation)?;
    assert!(explanation.contains("Capability provenance"));
    assert!(explanation.contains("Configured profiles"));
    assert!(!explanation.contains("tier"));
    assert!(explanation.contains("user_validated_profile"));
    Ok(())
}

#[test]
fn allocation_review_is_append_only_and_safe_apply_is_unchanged() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (source, state) = fixture(temp.path())?;
    let output = cargo_bin_cmd!("dispatch")
        .args(["--state-dir"])
        .arg(&state)
        .arg("run")
        .arg(&source)
        .args([
            "--task",
            "Implement a new requested feature.",
            "--agent",
            "codex",
            "--model",
            "configured-model",
            "--allow-unsafe-local",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let result: Value = serde_json::from_slice(&output)?;
    let run_id = result["run_id"].as_str().unwrap();

    cargo_bin_cmd!("dispatch")
        .args(["--state-dir"])
        .arg(&state)
        .args(["reject", run_id, "--reason", "tests"])
        .assert()
        .success();
    cargo_bin_cmd!("dispatch")
        .args(["--state-dir"])
        .arg(&state)
        .args(["accept", run_id, "--reason", "correctness"])
        .assert()
        .success();

    assert_eq!(
        fs::read_to_string(source.join("allocated.txt"))?,
        "allocated\n"
    );
    let database = Connection::open(state.join("dispatch.db"))?;
    let revisions: Vec<(i64, String)> = {
        let mut statement = database.prepare(
            "SELECT revision, outcome FROM goal_feedback_revisions WHERE run_id = ?1 ORDER BY revision",
        )?;
        statement
            .query_map([run_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    assert_eq!(
        revisions,
        vec![(1, "rejected".into()), (2, "accepted".into())]
    );
    Ok(())
}

/// With profiles configured, `--agent` only narrows the choice: an agent
/// with no eligible profile is refused, never run without a funding contract.
#[test]
fn explicit_agent_without_an_eligible_profile_is_refused_not_run_unbound() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (source, state) = fixture(temp.path())?;
    let error = cargo_bin_cmd!("dispatch")
        .args(["--state-dir"])
        .arg(&state)
        .arg("run")
        .arg(&source)
        .args([
            "--task",
            "Implement a new requested feature.",
            "--agent",
            "claude",
            "--allow-unsafe-local",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let error = String::from_utf8(error)?;
    assert!(
        error.contains("no included, no-overage-verified resource profile is available")
            && error.contains("explicit harness constraint"),
        "{error}"
    );
    assert!(!state.join("runs").exists());
    assert!(!source.join("allocated.txt").exists());
    Ok(())
}

#[test]
fn fixed_project_model_conflict_fails_before_creating_a_run() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (source, state) = fixture(temp.path())?;
    let config_path = source.join("dispatch.yml");
    fs::write(
        &config_path,
        fs::read_to_string(&config_path)?.replace(
            "  codex:\n    executable:",
            "  codex:\n    model: fixed-model\n    executable:",
        ),
    )?;
    let error = cargo_bin_cmd!("dispatch")
        .args(["--state-dir"])
        .arg(&state)
        .arg("run")
        .arg(&source)
        .args([
            "--task",
            "Implement a new requested feature.",
            "--agent",
            "codex",
            "--model",
            "configured-model",
            "--allow-unsafe-local",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8(error)?.contains("conflicts with fixed"));
    assert!(!state.join("runs").exists());
    Ok(())
}

#[test]
fn unknown_model_and_invalid_effort_are_rejected_without_substitution() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (source, state) = fixture(temp.path())?;
    let error = cargo_bin_cmd!("dispatch")
        .args(["--state-dir"])
        .arg(&state)
        .arg("run")
        .arg(&source)
        .args([
            "--task",
            "Implement a new requested feature.",
            "--agent",
            "codex",
            "--model",
            "not-in-the-catalog",
            "--allow-unsafe-local",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8(error)?.contains("no included, no-overage-verified"));
    assert!(!state.join("runs").exists());

    cargo_bin_cmd!("dispatch")
        .args(["--state-dir"])
        .arg(&state)
        .arg("run")
        .arg(&source)
        .args([
            "--task",
            "Implement a new requested feature.",
            "--agent",
            "codex",
            "--effort",
            "unbounded",
        ])
        .assert()
        .code(2);
    Ok(())
}

#[test]
fn quota_failure_mid_attempt_stops_without_a_paid_or_model_fallback() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (source, state) = fixture(temp.path())?;
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
         printf '{\"type\":\"turn.failed\",\"message\":\"usage limit exceeded\",\"model\":\"configured-model\"}\\n'\n",
    )?;
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755))?;

    let output = cargo_bin_cmd!("dispatch")
        .args(["--state-dir"])
        .arg(&state)
        .arg("run")
        .arg(&source)
        .args([
            "--task",
            "Implement a new requested feature.",
            "--allow-unsafe-local",
            "--json",
        ])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let result: Value = serde_json::from_slice(&output)?;
    assert_eq!(result["mode"], "native");
    assert_eq!(result["outcome"]["work_result"], "failed");
    assert_eq!(result["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(result["attempts"][0]["outcome"], "failed");
    Ok(())
}

#[test]
fn allocation_rejects_inherited_extra_args_that_can_shadow_resource_controls() -> anyhow::Result<()>
{
    let temp = tempfile::tempdir()?;
    let (source, state) = fixture(temp.path())?;
    let config_path = source.join("dispatch.yml");
    let config = fs::read_to_string(&config_path)?;
    fs::write(
        &config_path,
        format!("{}    extra_args: [--fast]\n", config),
    )?;
    let error = cargo_bin_cmd!("dispatch")
        .args(["--state-dir"])
        .arg(&state)
        .arg("run")
        .arg(&source)
        .args([
            "--task",
            "Implement a new requested feature.",
            "--allow-unsafe-local",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8(error)?.contains("extra_args to be empty"));
    assert!(!state.join("runs").exists());
    Ok(())
}
