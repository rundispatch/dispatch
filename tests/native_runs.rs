#![cfg(unix)]
use anyhow::{Context, Result};
use dispatch::{db::Database, state::State};
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    source: PathBuf,
    state: PathBuf,
}
fn executable(path: &Path, text: &str) -> Result<()> {
    fs::write(path, text)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}
impl Fixture {
    fn new(mode: &str) -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_owned();
        let source = root.join("source");
        let state = root.join("state");
        fs::create_dir_all(source.join("src"))?;
        fs::create_dir(&state)?;
        fs::write(source.join("src/lib.rs"), "// baseline\n")?;
        fs::write(source.join("result.txt"), "ok\n")?;
        fs::write(root.join("mode"), mode)?;
        let agent = root.join("codex");
        executable(
            &agent,
            &format!(
                r#"#!/bin/sh
if [ "$1" = '--version' ]; then echo 'codex fixture'; exit 0; fi
if [ "$1" = 'app-server' ]; then
while IFS= read -r line; do
case "$line" in
*'"id":0'*) printf '%s\n' '{{"id":0,"result":{{"userAgent":"fixture"}}}}' ;;
*'"id":1'*) printf '%s\n' '{{"id":1,"result":{{"account":{{"type":"chatgpt","planType":"plus","email":"fixture@example.invalid"}}}}}}' ;;
*'"id":2'*) printf '%s\n' '{{"id":2,"result":{{"rateLimitsByLimitId":{{}}}}}}'; exit 0 ;;
esac
done
exit 0
fi
model=''; previous=''
for arg in "$@"; do if [ "$previous" = '--model' ]; then model="$arg"; fi; previous="$arg"; done
printf '%s\n' "$model" >> '{root}/invocations'
count=$(wc -l < '{root}/invocations' | tr -d ' ')
mode=$(cat '{root}/mode')
if [ "$mode" = 'timeout' ]; then sleep 8; fi
if [ "$mode" = 'harness-failure' ]; then exit 9; fi
if [ "$mode" = 'checkpoint-crash' ] || [ "$mode" = 'checkpoint-then-complete' ] || [ "$mode" = 'clarify-always' ] || {{ [ "$mode" = 'clarify' ] && [ "$count" = '1' ]; }}; then
printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"dispatch_checkpoint\":{{\"version\":1,\"question\":\"Which label?\",\"choices\":[\"blue\",\"green\"]}}}}"}}}}'
if [ "$mode" = 'checkpoint-crash' ]; then exit 9; fi
if [ "$mode" = 'checkpoint-then-complete' ]; then printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"Done"}}}}'; fi
exit 0
fi
if [ "$mode" = 'malformed' ]; then printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"dispatch_checkpoint\":42}}"}}}}'; exit 0; fi
if [ "$mode" = 'success' ] || [ "$mode" = 'clarify' ] || {{ [ "$model" = 'strong-model' ] && [ "$mode" != 'both-fail' ]; }}; then
printf 'ok\n' > result.txt
printf '// delivered\n' > src/lib.rs
else
printf 'bad\n' > result.txt
printf '// failed-light\n' > src/lib.rs
fi
printf '{{"type":"result","model":"%s"}}\n' "$model"
"#,
                root = root.display()
            ),
        )?;
        let check = root.join("verify");
        executable(
            &check,
            &format!(
                r#"#!/bin/sh
if [ "$(cat result.txt)" = 'bad' ] && [ -f '{root}/gate' ]; then
: > '{root}/verification-waiting'
i=0
while [ ! -f '{root}/continue' ] && [ "$i" -lt 200 ]; do sleep 0.05; i=$((i+1)); done
fi
test "$(cat result.txt)" = 'ok'
"#,
                root = root.display()
            ),
        )?;
        fs::write(
            source.join("dispatch.yml"),
            format!(
                "execution:\n  timeout_secs: 60\nchecks:\n  verify: ['{}']\nharnesses:\n  codex:\n    executable: '{}'\n",
                check.display(),
                agent.display()
            ),
        )?;
        let profiles=[("light-model","low","light"),("strong-model","high","strong")].map(|(m,e,t)|format!("  - provider: openai\n    funding_source: chatgpt-plus\n    harness: codex\n    model: {m}\n    effort: {e}\n    runtime: local\n    service_mode: standard\n    pool: shared\n    provider_buckets: [codex]\n    tier: {t}\n    included: true\n    no_overage_verified: true\n    authorization_revision: 1\n    codex_account: {{\"account_sha256\":\"cc6d96611cffa9f02c3626f0b9ee897dc171e2d540a5cae349d4ec316104997b\",\"checked_at\":\"2026-01-01T00:00:00Z\"}}\n")).concat();
        fs::write(
            state.join("resources.yml"),
            format!("version: 1\nallocation_enabled: true\nprofiles:\n{profiles}"),
        )?;
        Ok(Self {
            _temp: temp,
            root,
            source,
            state,
        })
    }
    fn command(&self) -> Command {
        let mut c = Command::new(assert_cmd::cargo_bin!("dispatch"));
        c.arg("--state-dir").arg(&self.state);
        c
    }
    fn run(&self, extra: &[&str]) -> Result<Output> {
        Ok(self.run_command(extra).output()?)
    }
    fn run_command(&self, extra: &[&str]) -> Command {
        let mut c = self.command();
        c.arg("run")
            .arg(&self.source)
            .args([
                "--task",
                "Add tests in src/lib.rs",
                "--allow-unsafe-local",
                "--json",
            ])
            .args(extra);
        c
    }
    fn count(&self) -> usize {
        fs::read_to_string(self.root.join("invocations"))
            .unwrap_or_default()
            .lines()
            .count()
    }
    fn result(output: &Output) -> Result<Value> {
        serde_json::from_slice(&output.stdout).with_context(|| {
            format!(
                "stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }
    fn loaded(&self, id: &str) -> Result<dispatch::RunRecord> {
        State::discover(Some(self.state.clone()))?.load_run(id)
    }
    fn answer(&self, result: &Value, revision: &str) -> Result<Output> {
        Ok(self.answer_command(result, revision).output()?)
    }
    fn answer_command(&self, result: &Value, revision: &str) -> Command {
        let q = &result["execution"]["questions"][0];
        let mut command = self.command();
        command.args([
            "answer",
            result["run_id"].as_str().unwrap(),
            q["id"].as_str().unwrap(),
            "--revision",
            revision,
            "--answer",
            "blue",
            "--json",
        ]);
        command
    }
    fn checks(&self, checks: &[&str]) -> Result<()> {
        let path = self.source.join("dispatch.yml");
        let mut config: dispatch::config::Config =
            serde_yaml::from_str(&fs::read_to_string(&path)?)?;
        config.checks.verify = checks.iter().map(|c| (*c).to_owned()).collect();
        fs::write(path, serde_yaml::to_string(&config)?)?;
        Ok(())
    }
    fn assert_interrupted(&self, before: &dispatch::RunRecord) -> Result<()> {
        let after = self.loaded(&before.id)?;
        assert_eq!(after.outcome.lifecycle, dispatch::LifecycleState::Finished);
        assert_eq!(after.outcome.work_result, dispatch::WorkResult::Interrupted);
        assert_eq!(after.outcome.waiting_on, dispatch::WaitingOn::None);
        assert_eq!(after.outcome.verification, before.outcome.verification);
        assert_eq!(after.outcome.review, before.outcome.review);
        assert_eq!(after.outcome.application, before.outcome.application);
        assert_eq!(
            serde_json::to_value(&after.attempts)?,
            serde_json::to_value(&before.attempts)?
        );
        let policy = after.execution.as_ref().unwrap();
        let old_policy = before.execution.as_ref().unwrap();
        assert_eq!(policy.questions, old_policy.questions);
        assert_eq!(policy.deadline_at, old_policy.deadline_at);
        assert_eq!(policy.final_attempt_id, old_policy.final_attempt_id);
        for _ in 0..3 {
            assert_eq!(
                serde_json::to_value(self.loaded(&before.id)?)?,
                serde_json::to_value(&after)?
            );
        }
        assert_eq!(self.count(), 1);
        self.no_live_launches()
    }
    fn no_live_launches(&self) -> Result<()> {
        let db = rusqlite::Connection::open(self.state.join("dispatch.db"))?;
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM attempt_launches WHERE state IN ('intent', 'spawned', 'uncertain')",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        Ok(())
    }
}

#[test]
fn initial_success_selects_first_without_recovery() -> Result<()> {
    let f = Fixture::new("success")?;
    let output = f.run(&[])?;
    let r = Fixture::result(&output)?;
    assert!(output.status.success());
    assert_eq!(f.count(), 1);
    assert_eq!(r["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(r["execution"]["final_attempt_id"], r["attempts"][0]["id"]);
    assert_eq!(r["outcome"]["verification"], "passed");
    f.no_live_launches()
}

#[test]
fn no_suitable_stronger_route_and_fixed_model_do_not_escalate() -> Result<()> {
    for fixed in [false, true] {
        let f = Fixture::new("both-fail")?;
        if !fixed {
            let p = f.state.join("resources.yml");
            let s = fs::read_to_string(&p)?;
            let second = s.rfind("  - provider:").unwrap();
            fs::write(p, &s[..second])?;
        }
        let o = f.run(if fixed {
            &["--model", "light-model"]
        } else {
            &[]
        })?;
        assert_eq!(o.status.code(), Some(3));
        assert_eq!(f.count(), 1);
    }
    Ok(())
}

#[test]
fn baseline_failure_and_harness_failure_do_not_escalate() -> Result<()> {
    for baseline in [false, true] {
        let f = Fixture::new(if baseline {
            "both-fail"
        } else {
            "harness-failure"
        })?;
        if baseline {
            fs::write(f.source.join("result.txt"), "bad\n")?;
        }
        let o = f.run(&[])?;
        let r = Fixture::result(&o)?;
        assert!(!o.status.success());
        assert_eq!(f.count(), 1);
        assert_eq!(
            r["execution"]["failure"],
            if baseline {
                "baseline_infrastructure"
            } else {
                "harness_process"
            }
        );
    }
    Ok(())
}

#[test]
fn clarification_answer_is_durable_released_and_single_use() -> Result<()> {
    let f = Fixture::new("clarify")?;
    let o = f.run(&[])?;
    let r = Fixture::result(&o)?;
    assert_eq!(o.status.code(), Some(4));
    assert_eq!(r["outcome"]["lifecycle"], "waiting");
    assert_eq!(r["outcome"]["waiting_on"], "human");
    f.no_live_launches()?;
    let before = f.loaded(r["run_id"].as_str().unwrap())?;
    assert_eq!(
        serde_json::to_value(before.execution.as_ref().unwrap().questions.clone())?,
        r["execution"]["questions"]
    );
    assert!(!f.answer(&r, "0")?.status.success());
    assert_eq!(f.count(), 1);
    let answered = f.answer(&r, "1")?;
    let result = Fixture::result(&answered)?;
    assert!(answered.status.success(), "{result}");
    assert_eq!(f.count(), 2);
    assert_eq!(result["execution"]["questions"][0]["state"], "answered");
    assert_eq!(
        result["attempts"][1]["detail"]["reason"],
        "clarification_answer"
    );
    // Each attempt has its own launch record, cleaned up before the next.
    let launches: i64 = rusqlite::Connection::open(f.state.join("dispatch.db"))?.query_row(
        "SELECT COUNT(DISTINCT attempt_id) FROM attempt_launches WHERE run_id=?1 AND state='cleaned'",
        [result["run_id"].as_str().unwrap()],
        |row| row.get(0),
    )?;
    assert_eq!(launches, 2);
    assert!(!f.answer(&r, "1")?.status.success());
    assert_eq!(f.count(), 2);
    f.no_live_launches()
}

#[test]
fn cancelled_question_rejects_answers_and_late_events() -> Result<()> {
    let f = Fixture::new("clarify")?;
    let r = Fixture::result(&f.run(&[])?)?;
    let id = r["run_id"].as_str().unwrap();
    let q = r["execution"]["questions"][0]["id"].as_str().unwrap();
    let mut stale = f.loaded(id)?;
    let cancel = f
        .command()
        .args(["cancel", id, q, "--revision", "1", "--json"])
        .output()?;
    assert!(cancel.status.success());
    let cancelled = Fixture::result(&cancel)?;
    assert_eq!(cancelled["outcome"]["work_result"], "cancelled");
    assert_eq!(cancelled["execution"]["questions"][0]["state"], "cancelled");
    assert!(!f.answer(&r, "1")?.status.success());
    let current = f.loaded(id)?;
    stale.state_revision = current.state_revision;
    stale.outcome.lifecycle = dispatch::LifecycleState::Working;
    let mut db = Database::open(f.state.join("dispatch.db"))?;
    assert!(db.sync_run(&stale).is_err());
    assert!(
        db.commit_transition(
            &mut stale,
            dispatch::EventRecord {
                run_id: id.into(),
                event_type: "attempt.finished".into(),
                ..Default::default()
            }
        )
        .is_err()
    );
    assert_eq!(
        f.loaded(id)?.outcome.work_result,
        dispatch::WorkResult::Cancelled
    );
    assert_eq!(f.count(), 1);
    f.no_live_launches()
}

#[test]
fn checkpoint_continuation_cannot_create_third_invocation_and_malformed_stops() -> Result<()> {
    for mode in ["clarify-always", "malformed"] {
        let f = Fixture::new(mode)?;
        let first = Fixture::result(&f.run(&[])?)?;
        if mode == "malformed" {
            assert_eq!(first["execution"]["failure"], "unsupported_checkpoint");
            assert_eq!(f.count(), 1);
        } else {
            let second = Fixture::result(&f.answer(&first, "1")?)?;
            assert_eq!(second["execution"]["failure"], "invocation_limit");
            assert_eq!(f.count(), 2);
            assert_ne!(second["outcome"]["waiting_on"], "human");
        }
        f.no_live_launches()?;
    }
    Ok(())
}

#[test]
fn source_drift_blocks_apply_and_human_rejection_cannot_continue() -> Result<()> {
    let f = Fixture::new("recovery")?;
    let r = Fixture::result(&f.run(&[])?)?;
    let id = r["run_id"].as_str().unwrap();
    fs::write(f.source.join("src/lib.rs"), "// user edit\n")?;
    let accepted = f.command().args(["accept", id]).output()?;
    assert!(!accepted.status.success());
    assert_eq!(
        fs::read_to_string(f.source.join("src/lib.rs"))?,
        "// user edit\n"
    );
    assert_eq!(
        f.loaded(id)?.outcome.application,
        dispatch::ApplicationState::BlockedBySourceDrift
    );
    // A refused accept records no human review; the result stays pending.
    assert_eq!(f.loaded(id)?.outcome.review, dispatch::ReviewState::Pending);
    let g = Fixture::new("success")?;
    let success = Fixture::result(&g.run(&[])?)?;
    let id = success["run_id"].as_str().unwrap();
    assert!(g.command().args(["reject", id]).output()?.status.success());
    assert!(
        !g.command()
            .args([
                "answer",
                id,
                "invalid",
                "--revision",
                "1",
                "--answer",
                "try again"
            ])
            .output()?
            .status
            .success()
    );
    assert_eq!(g.count(), 1);
    Ok(())
}

/// The source moves while the goal waits on a question. Answering continues
/// the goal from its original snapshot; it does not end it with source drift.
/// The result is then judged against the moved source at accept.
#[test]
fn answering_after_the_source_moved_continues_the_goal() -> Result<()> {
    let f = Fixture::new("clarify")?;
    let r = Fixture::result(&f.run(&[])?)?;
    assert_eq!(r["execution"]["questions"][0]["state"], "pending", "{r}");
    // A run waiting for your answer says so, not "working".
    let history = f.command().args(["--plain", "history"]).output()?;
    let history = String::from_utf8_lossy(&history.stdout);
    assert!(history.contains(" question "), "{history}");
    fs::write(f.source.join("NOTES.md"), "a teammate's change\n")?;
    let answered = f.answer(&r, "1")?;
    let result = Fixture::result(&answered)?;
    assert!(answered.status.success(), "{result}");
    assert_eq!(f.count(), 2);
    assert_ne!(result["execution"]["failure"], "source_drift", "{result}");
    assert_eq!(result["outcome"]["work_result"], "ready", "{result}");
    Ok(())
}

#[test]
fn one_deadline_bounds_invocations_and_waiting_answers() -> Result<()> {
    let f = Fixture::new("timeout")?;
    let r = Fixture::result(&f.run(&["--timeout", "5"])?)?;
    assert_eq!(r["execution"]["failure"], "deadline");
    assert_eq!(f.count(), 1);
    f.no_live_launches()?;
    // Room for the first attempt to ask its question even on a loaded
    // machine; then wait out the run's own recorded deadline before answering.
    let g = Fixture::new("clarify")?;
    let r = Fixture::result(&g.run(&["--timeout", "30"])?)?;
    assert_eq!(r["execution"]["questions"][0]["state"], "pending", "{r}");
    let deadline: chrono::DateTime<chrono::Utc> =
        r["execution"]["deadline_at"].as_str().unwrap().parse()?;
    while chrono::Utc::now() <= deadline {
        thread::sleep(Duration::from_millis(200));
    }
    let answer = Fixture::result(&g.answer(&r, "1")?)?;
    assert_eq!(answer["execution"]["failure"], "deadline");
    assert_eq!(g.count(), 1);
    g.no_live_launches()
}

#[test]
fn question_commands_reject_wrong_identity_generation_and_local_actor() -> Result<()> {
    let f = Fixture::new("clarify")?;
    let r = Fixture::result(&f.run(&[])?)?;
    let other = Fixture::result(&f.run(&[])?)?;
    let id = r["run_id"].as_str().unwrap();
    let q = r["execution"]["questions"][0]["id"].as_str().unwrap();
    for (run, question, generation) in [
        (id, "wrong-question", "1"),
        (other["run_id"].as_str().unwrap(), q, "1"),
        (id, q, "2"),
    ] {
        let output = f
            .command()
            .args([
                "answer",
                run,
                question,
                "--revision",
                "1",
                "--generation",
                generation,
                "--answer",
                "blue",
                "--json",
            ])
            .output()?;
        assert!(!output.status.success());
    }
    assert_eq!(
        f.loaded(id)?.execution.unwrap().questions[0].state,
        dispatch::QuestionState::Pending
    );
    // Current local authority comes from persisted owner identity, not a CLI actor parameter.
    let mut run = f.loaded(id)?;
    run.execution.as_mut().unwrap().owner_uid = unsafe { libc::geteuid() }.wrapping_add(1);
    Database::open(f.state.join("dispatch.db"))?.sync_run(&run)?;
    let denied = f.answer(&r, "1")?;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("unauthorized local caller"));
    assert_eq!(f.count(), 2);
    f.no_live_launches()
}

#[test]
fn continuation_rechecks_configuration_and_shared_funding() -> Result<()> {
    for config_change in [false, true] {
        let f = Fixture::new("clarify")?;
        let first = Fixture::result(&f.run(&[])?)?;
        if config_change {
            let path = f.state.join("resources.yml");
            fs::write(
                &path,
                fs::read_to_string(&path)?
                    .replace("no_overage_verified: true", "no_overage_verified: false"),
            )?;
        } else {
            // The provider starts reporting paid credits while the question waits.
            let agent = f.root.join("codex");
            fs::write(
                &agent,
                fs::read_to_string(&agent)?.replace(
                    r#"{"rateLimitsByLimitId":{}}"#,
                    r#"{"rateLimitsByLimitId":{"codex":{"credits":{"hasCredits":true}}}}"#,
                ),
            )?;
        }
        let output = f.answer(&first, "1")?;
        let result = Fixture::result(&output)?;
        assert!(!output.status.success());
        assert_eq!(f.count(), 1);
        assert_eq!(result["execution"]["failure"], "authorization");
        assert_eq!(result["execution"]["questions"][0]["state"], "answered");
        f.no_live_launches()?;
    }
    Ok(())
}

#[test]
fn reload_repairs_missing_and_higher_stale_projections_without_replaying_work() -> Result<()> {
    for mode in ["success", "clarify"] {
        let f = Fixture::new(mode)?;
        let result = Fixture::result(&f.run(&[])?)?;
        let id = result["run_id"].as_str().unwrap();
        let state = State::discover(Some(f.state.clone()))?;
        let current = state.load_run(id)?;
        // Simulate process loss between SQLite commit and projection publication.
        fs::remove_file(state.metadata_path(id))?;
        fs::write(state.events_path(id), "incomplete\n")?;
        let repaired = state.load_run(id)?;
        assert_eq!(
            serde_json::to_value(dispatch::orchestrator::run_result(&repaired))?["attempts"],
            result["attempts"]
        );
        let mut stale = current.clone();
        stale.state_revision += 100;
        stale.outcome.lifecycle = dispatch::LifecycleState::Working;
        stale.execution.as_mut().unwrap().questions.clear();
        stale.attempts.clear();
        state.save_run(&stale)?;
        let output = f.command().args(["status", id, "--json"]).output()?;
        let status = Fixture::result(&output)?;
        assert_eq!(status["attempts"], result["attempts"]);
        assert_eq!(status["execution"], result["execution"]);
        assert_eq!(status["outcome"], result["outcome"]);
        assert_eq!(f.count(), if mode == "recovery" { 2 } else { 1 });
        f.no_live_launches()?;
    }
    Ok(())
}

#[test]
fn completed_attempt_evidence_and_rejection_cannot_be_rewritten() -> Result<()> {
    let f = Fixture::new("success")?;
    let result = Fixture::result(&f.run(&[])?)?;
    let id = result["run_id"].as_str().unwrap();
    let original = f.loaded(id)?;
    let mut db = Database::open(f.state.join("dispatch.db"))?;
    for remove in [false, true] {
        let mut stale = original.clone();
        if remove {
            stale.attempts.remove(0);
        } else {
            stale.attempts[0].detail.reason = Some("rewritten".into());
        }
        assert!(db.sync_run(&stale).is_err());
        assert!(
            db.commit_transition(
                &mut stale,
                dispatch::EventRecord {
                    run_id: id.into(),
                    event_type: "late.attempt".into(),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
    assert!(f.command().args(["reject", id]).output()?.status.success());
    let mut stale = original;
    stale.state_revision = f.loaded(id)?.state_revision;
    assert!(db.sync_run(&stale).is_err());
    assert!(
        db.commit_transition(
            &mut stale,
            dispatch::EventRecord {
                run_id: id.into(),
                event_type: "late.completion".into(),
                ..Default::default()
            }
        )
        .is_err()
    );
    assert_eq!(f.count(), 1);
    // Review creates no legacy routing observation.
    let conn = rusqlite::Connection::open(f.state.join("dispatch.db"))?;
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM routing_observations", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    Ok(())
}

#[test]
fn verification_infrastructure_failure_does_not_trigger_recovery() -> Result<()> {
    let f = Fixture::new("both-fail")?;
    executable(
        &f.root.join("verify"),
        "#!/bin/sh\nif [ \"$(cat result.txt)\" = 'bad' ]; then exit 127; fi\nexit 0\n",
    )?;
    let result = Fixture::result(&f.run(&[])?)?;
    assert_eq!(f.count(), 1);
    assert_eq!(
        result["execution"]["failure"],
        "verification_infrastructure"
    );
    assert_eq!(
        result["attempts"][0]["detail"]["failure"],
        "verification_infrastructure"
    );
    f.no_live_launches()
}

#[test]
fn delivery_applies_from_original_baseline_and_pending_question_is_not_reviewable() -> Result<()> {
    let f = Fixture::new("success")?;
    let result = Fixture::result(&f.run(&[])?)?;
    let id = result["run_id"].as_str().unwrap();
    assert!(f.command().args(["accept", id]).output()?.status.success());
    assert_eq!(
        fs::read_to_string(f.source.join("src/lib.rs"))?,
        "// delivered\n"
    );
    assert_eq!(
        f.loaded(id)?.outcome.application,
        dispatch::ApplicationState::Applied
    );
    let g = Fixture::new("clarify")?;
    let pending = Fixture::result(&g.run(&[])?)?;
    for command in ["accept", "reject"] {
        assert!(
            !g.command()
                .args([command, pending["run_id"].as_str().unwrap()])
                .output()?
                .status
                .success()
        );
    }
    assert_eq!(
        g.loaded(pending["run_id"].as_str().unwrap())?
            .execution
            .unwrap()
            .questions[0]
            .state,
        dispatch::QuestionState::Pending
    );
    Ok(())
}

#[test]
fn unavailable_baseline_tool_stops_before_model_invocation() -> Result<()> {
    let f = Fixture::new("success")?;
    executable(&f.root.join("verify"), "#!/bin/sh\nexit 127\n")?;
    let result = Fixture::result(&f.run(&[])?)?;
    assert_eq!(f.count(), 0);
    assert_eq!(result["execution"]["failure"], "baseline_infrastructure");
    f.no_live_launches()
}

#[test]
fn checkpoint_requires_clean_exit_and_a_final_agent_report() -> Result<()> {
    for mode in ["checkpoint-crash", "checkpoint-then-complete"] {
        let f = Fixture::new(mode)?;
        let output = f.run(&[])?;
        let result = Fixture::result(&output)?;
        assert!(!output.status.success());
        assert_eq!(f.count(), 1);
        assert!(
            result["execution"]["questions"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            result["execution"]["failure"],
            if mode == "checkpoint-crash" {
                "harness_process"
            } else {
                "unsupported_checkpoint"
            }
        );
        f.no_live_launches()?;
    }
    Ok(())
}

#[test]
fn signal_killed_verification_is_unknown_and_never_recovers() -> Result<()> {
    let f = Fixture::new("both-fail")?;
    // Kill the check shell itself, so there is no normal process exit code.
    f.checks(&["if [ \"$(cat result.txt)\" = bad ]; then kill -KILL $$; fi"])?;
    let result = Fixture::result(&f.run(&[])?)?;
    assert_eq!(f.count(), 1);
    assert_eq!(result["execution"]["failure"], "verification_unknown");
    assert_eq!(
        result["attempts"][0]["detail"]["failure"],
        "verification_unknown"
    );
    let checks = &result["attempts"][0]["detail"]["result"]["checks"];
    assert_eq!(checks[0]["status"], "failed");
    assert!(checks[0]["exit_code"].is_null());
    f.no_live_launches()
}

#[test]
fn any_infrastructure_or_unknown_check_blocks_other_target_failures() -> Result<()> {
    for extra in ["exit 127", "kill -KILL $$"] {
        let f = Fixture::new("both-fail")?;
        let second = format!("if [ \"$(cat result.txt)\" = bad ]; then {extra}; fi");
        f.checks(&["test \"$(cat result.txt)\" = ok", &second])?;
        let result = Fixture::result(&f.run(&[])?)?;
        assert_eq!(f.count(), 1);
        assert_eq!(
            result["execution"]["failure"],
            if extra == "exit 127" {
                "verification_infrastructure"
            } else {
                "verification_unknown"
            }
        );
        let checks = result["attempts"][0]["detail"]["result"]["checks"]
            .as_array()
            .unwrap();
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0]["exit_code"], 1);
        assert_eq!(checks[1]["status"], "failed");
        f.no_live_launches()?;
    }
    Ok(())
}

#[test]
fn only_a_valid_final_agent_message_can_checkpoint() -> Result<()> {
    let checkpoint = serde_json::json!({"type":"item.completed","item":{"type":"agent_message","text":serde_json::json!({"dispatch_checkpoint":{"version":1,"question":"Which label?","choices":[]}}).to_string()}});
    let malformed =
        serde_json::json!({"type":"item.completed","item":{"type":"agent_message","text":42}});
    let invalid = serde_json::json!({"type":"item.completed","item":{"type":"agent_message","text":"{\"dispatch_checkpoint\":42}"}});
    for (messages, exit, accepted) in [
        (vec![checkpoint.clone(), malformed.clone()], 0, false),
        (vec![checkpoint.clone(), invalid.clone()], 0, false),
        (vec![malformed.clone(), checkpoint.clone()], 0, true),
        (vec![invalid, checkpoint.clone()], 0, true),
        (vec![malformed], 0, false),
        (vec![checkpoint], 9, false),
    ] {
        let f = Fixture::new("success")?;
        let reports = messages
            .iter()
            .map(|m| format!("printf '%s\\n' '{}'\n", m))
            .collect::<String>();
        executable(
            &f.root.join("codex"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo fixture; exit 0; fi\nif [ \"$1\" = 'app-server' ]; then\nwhile IFS= read -r line; do\ncase \"$line\" in\n*'\"id\":0'*) printf '%s\\n' '{{\"id\":0,\"result\":{{\"userAgent\":\"fixture\"}}}}' ;;\n*'\"id\":1'*) printf '%s\\n' '{{\"id\":1,\"result\":{{\"account\":{{\"type\":\"chatgpt\",\"planType\":\"plus\",\"email\":\"fixture@example.invalid\"}}}}}}' ;;\n*'\"id\":2'*) printf '%s\\n' '{{\"id\":2,\"result\":{{\"rateLimitsByLimitId\":{{}}}}}}'; exit 0 ;;\nesac\ndone\nexit 0\nfi\necho call >> '{}'\n{reports}exit {exit}\n",
                f.root.join("invocations").display()
            ),
        )?;
        let result = Fixture::result(&f.run(&[])?)?;
        assert_eq!(result["outcome"]["waiting_on"] == "human", accepted);
        assert_eq!(
            result["execution"]["questions"].as_array().unwrap().len(),
            usize::from(accepted)
        );
        if !accepted {
            assert_eq!(
                result["execution"]["failure"],
                if exit == 0 {
                    "unsupported_checkpoint"
                } else {
                    "harness_process"
                }
            );
        }
        assert_eq!(f.count(), 1);
        f.no_live_launches()?;
    }
    Ok(())
}

// Fixtures block on a FIFO only after the relevant durable transition. The
// guard kills/reaps the owner even if a regression causes an assertion panic.
struct OwnedChild(std::process::Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn replace_with_fifo(path: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    fs::remove_file(path)?;
    let path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: the CString is a valid, terminated fixture path.
    anyhow::ensure!(
        unsafe { libc::mkfifo(path.as_ptr(), 0o600) } == 0,
        "mkfifo failed"
    );
    Ok(())
}
fn wait_until(mut ready: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready()? {
        anyhow::ensure!(
            Instant::now() < deadline,
            "fixture did not reach its durable boundary"
        );
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

// This subprocess holds the same run lock and Phase 2 process identity as a
// foreground supervisor. Stop at the committed attempt boundary without a
// timing race against the next synchronous orchestration instruction.
#[test]
fn committed_attempt_owner_fixture() -> Result<()> {
    use std::os::fd::AsRawFd;
    let Some(root) = std::env::var_os("DISPATCH_TEST_ABANDONED_STATE") else {
        return Ok(());
    };
    let id = std::env::var("DISPATCH_TEST_ABANDONED_RUN")?;
    let state = State::discover(Some(PathBuf::from(root)))?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(state.run_dir(&id).join(".operation.lock"))?;
    // SAFETY: the descriptor stays open for this fixture owner's lifetime.
    anyhow::ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "fixture owner lock failed"
    );
    let mut run = state.load_run(&id)?;
    run.status = dispatch::RunStatus::Running;
    run.completed_at = None;
    run.outcome.lifecycle = dispatch::LifecycleState::Working;
    run.outcome.work_result = dispatch::WorkResult::Pending;
    run.outcome.review = dispatch::ReviewState::NotRequested;
    let policy = run.execution.as_mut().unwrap();
    policy.supervisor = Some(dispatch::process::ProcessIdentity::current());
    policy.final_attempt_id = None;
    policy.failure = None;
    let attempt_id = run.attempts[0].id.clone();
    let mut db = Database::open(state.db_path())?;
    db.sync_run(&run)?;
    db.commit_transition(
        &mut run,
        dispatch::EventRecord {
            run_id: id.clone(),
            attempt_id: Some(attempt_id),
            event_type: "attempt.finished".into(),
            ..Default::default()
        },
    )?;
    state.save_run(&run)?;
    fs::write(state.run_dir(&id).join("fixture-committed"), "")?;
    loop {
        thread::park();
    }
}

#[test]
fn dead_owner_after_attempt_finish_is_repaired_but_live_owner_is_not() -> Result<()> {
    let f = Fixture::new("both-fail")?;
    let result = Fixture::result(&f.run(&[])?)?;
    let id = result["run_id"].as_str().unwrap();
    let mut child = OwnedChild(
        Command::new(std::env::current_exe()?)
            .args(["--exact", "committed_attempt_owner_fixture", "--nocapture"])
            .env("DISPATCH_TEST_ABANDONED_STATE", &f.state)
            .env("DISPATCH_TEST_ABANDONED_RUN", id)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    wait_until(|| {
        Ok(f.state
            .join("runs")
            .join(id)
            .join("fixture-committed")
            .exists())
    })?;
    let before = f.loaded(id)?;
    assert_eq!(before.outcome.lifecycle, dispatch::LifecycleState::Working);
    assert!(child.0.try_wait()?.is_none());
    assert!(before.attempts.iter().all(|a| a.completed_at.is_some()));
    f.no_live_launches()?;
    child.0.kill()?;
    child.0.wait()?;
    f.assert_interrupted(&before)
}

#[test]
fn dead_owner_after_answer_commit_is_repaired_without_replaying_or_reopening() -> Result<()> {
    let f = Fixture::new("clarify")?;
    let first = Fixture::result(&f.run(&[])?)?;
    let id = first["run_id"].as_str().unwrap();
    replace_with_fifo(&f.state.join("runs").join(id).join("config.snapshot.yml"))?;
    let mut child = OwnedChild(
        f.answer_command(&first, "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    wait_until(|| {
        let events = fs::read_to_string(f.state.join("runs").join(id).join("events.jsonl"))?;
        Ok(events
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|event| event["event_type"] == "question.resolved"))
    })?;
    let before = f.loaded(id)?;
    assert_eq!(
        before.outcome.lifecycle,
        dispatch::LifecycleState::Preparing
    );
    assert!(child.0.try_wait()?.is_none());
    f.no_live_launches()?;
    child.0.kill()?;
    child.0.wait()?;
    f.assert_interrupted(&before)?;
    assert!(!f.answer(&first, "1")?.status.success());
    assert_eq!(f.count(), 1);
    Ok(())
}

#[test]
fn answer_setup_error_records_interruption_and_preserves_the_committed_answer() -> Result<()> {
    let f = Fixture::new("clarify")?;
    let first = Fixture::result(&f.run(&[])?)?;
    let id = first["run_id"].as_str().unwrap();
    let before = f.loaded(id)?;
    fs::write(f.state.join("resources.yml"), "invalid: [\n")?;
    let output = f.answer(&first, "1")?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("failed to parse resource config"));
    let stopped = f.loaded(id)?;
    assert_eq!(
        stopped.execution.as_ref().unwrap().questions[0].state,
        dispatch::QuestionState::Answered
    );
    assert_eq!(
        stopped.execution.as_ref().unwrap().questions[0]
            .answer
            .as_deref(),
        Some("blue")
    );
    assert_eq!(
        serde_json::to_value(&stopped.attempts)?,
        serde_json::to_value(&before.attempts)?
    );
    f.assert_interrupted(&stopped)
}

#[test]
fn reload_requires_positive_owner_and_cleanup_evidence() -> Result<()> {
    for evidence in ["live", "missing", "legacy", "unresolved", "unlocked-dead"] {
        let f = Fixture::new("success")?;
        let result = Fixture::result(&f.run(&[])?)?;
        let id = result["run_id"].as_str().unwrap();
        let mut run = f.loaded(id)?;
        run.outcome.lifecycle = dispatch::LifecycleState::Working;
        run.outcome.work_result = dispatch::WorkResult::Pending;
        run.execution.as_mut().unwrap().supervisor = match evidence {
            "live" => Some(dispatch::process::ProcessIdentity::current()),
            "missing" | "legacy" => None,
            _ => Some(dispatch::process::process_identity(u32::MAX)),
        };
        let mut db = Database::open(f.state.join("dispatch.db"))?;
        db.sync_run(&run)?;
        if evidence == "unresolved" {
            // The agent's cleanup was never confirmed and it is still alive.
            let live = serde_json::to_string(&dispatch::process::ProcessIdentity::current())?;
            rusqlite::Connection::open(f.state.join("dispatch.db"))?.execute(
                "UPDATE attempt_launches SET state='uncertain', child_json=?2 WHERE run_id=?1",
                rusqlite::params![id, live],
            )?;
        }
        // Without a recorded supervisor, work is never presumed abandoned.
        if evidence == "unlocked-dead" {
            f.assert_interrupted(&run)?;
        } else {
            assert_eq!(
                f.loaded(id)?.outcome.lifecycle,
                dispatch::LifecycleState::Working
            );
        }
    }
    Ok(())
}

// Phase 4 exercises the same Phase 3 fixtures through the actual foreground UI.
#[test]
fn phase4_natural_goal_reaches_review_with_light_and_standard_profiles_only() -> Result<()> {
    let f = Fixture::new("success")?;
    let profiles = f.state.join("resources.yml");
    fs::write(
        &profiles,
        fs::read_to_string(&profiles)?
            .replace("strong-model", "standard-model")
            .replace("tier: strong", "tier: standard"),
    )?;
    fs::write(f.source.join("main.c"), "int ballRadius = 20;\n")?;
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/tui_session.py"
        ))
        .arg(assert_cmd::cargo_bin!("dispatch"))
        .arg(&f.source)
        .arg(&f.state)
        .arg("natural")
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(f.count(), 1);
    assert_eq!(
        fs::read_to_string(f.source.join("main.c"))?,
        "int ballRadius = 20;\n"
    );
    f.no_live_launches()
}

#[test]
fn phase4_pty_intent_answer_recovery_review_and_restoration() -> Result<()> {
    for (scenario, mode) in [
        ("accept", "success"),
        ("reject", "success"),
        ("clarify", "clarify"),
        ("drift", "success"),
        ("cancel", "timeout"),
        ("active-eof", "timeout"),
        ("hangup", "timeout"),
        ("plain", "success"),
        ("activity", "success"),
        ("eof", "success"),
        ("signal", "success"),
    ] {
        let f = Fixture::new(mode)?;
        if scenario == "activity" {
            let agent = f.root.join("codex");
            fs::write(
                &agent,
                fs::read_to_string(&agent)?.replace("count=$(wc", "sleep 2\ncount=$(wc"),
            )?;
        }
        let output = Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/tui_session.py"
            ))
            .arg(assert_cmd::cargo_bin!("dispatch"))
            .arg(&f.source)
            .arg(&f.state)
            .arg(scenario)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "PTY {scenario}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[tokio::test]
async fn phase4_cursor_observes_question_even_after_answer_and_is_read_only() -> Result<()> {
    let f = Fixture::new("clarify")?;
    let initial = Fixture::result(&f.run(&[])?)?;
    let id = initial["run_id"].as_str().unwrap();
    let resumed = f.answer(&initial, "1")?;
    assert!(resumed.status.success());
    let state = State::discover(Some(f.state.clone()))?;
    let metadata = fs::read(state.metadata_path(id))?;
    let mut out = Vec::new();
    assert!(
        dispatch::follow::events(
            &state,
            id,
            0,
            Some(dispatch::follow::Until::Attention),
            Duration::from_secs(1),
            &mut out
        )
        .await?
    );
    let records = String::from_utf8(out)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(
        records[records.len() - 2]["event"]["event_type"],
        "question.pending"
    );
    let cursor = records.last().unwrap()["after"].as_u64().unwrap();
    let mut rest = Vec::new();
    assert!(
        dispatch::follow::events(
            &state,
            id,
            cursor,
            Some(dispatch::follow::Until::Finished),
            Duration::from_secs(1),
            &mut rest
        )
        .await?
    );
    let records = String::from_utf8(rest)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert!(records[0]["event"]["sequence"].as_u64().unwrap() > cursor);
    assert_eq!(fs::read(state.metadata_path(id))?, metadata);
    Ok(())
}

#[test]
fn phase4_review_identity_rejects_cross_run_candidate_and_stale_commands() -> Result<()> {
    let f = Fixture::new("success")?;
    let first = Fixture::result(&f.run(&[])?)?;
    let second = Fixture::result(&f.run(&[])?)?;
    let a = f.loaded(first["run_id"].as_str().unwrap())?;
    let b = f.loaded(second["run_id"].as_str().unwrap())?;
    let state = State::discover(Some(f.state.clone()))?;
    let mut wrong = dispatch::presenter::review_command(&a)?;
    wrong.candidate_id = b.candidates[0].id.clone();
    assert!(dispatch::orchestrator::review_delivery(&state, &wrong, true).is_err());
    let target = dispatch::presenter::review_command(&a)?;
    assert!(dispatch::orchestrator::review_diff(&state, &target)?.contains("delivered"));
    dispatch::orchestrator::review_delivery(&state, &target, false)?;
    assert!(dispatch::orchestrator::review_delivery(&state, &target, true).is_err());
    assert_eq!(
        state.load_run(&b.id)?.outcome.review,
        dispatch::ReviewState::Pending
    );
    assert_eq!(
        fs::read_to_string(f.source.join("src/lib.rs"))?,
        "// baseline\n"
    );
    Ok(())
}

#[test]
fn phase4_two_foreground_sessions_keep_their_own_deliveries() -> Result<()> {
    let f = Fixture::new("success")?;
    let peer = f.root.join("peer");
    fs::create_dir_all(peer.join("src"))?;
    for path in ["src/lib.rs", "result.txt", "dispatch.yml"] {
        fs::copy(f.source.join(path), peer.join(path))?;
    }
    let launch = |source: &Path| -> Result<_> {
        Ok(Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/tui_session.py"
            ))
            .arg(assert_cmd::cargo_bin!("dispatch"))
            .arg(source)
            .arg(&f.state)
            .arg("concurrent")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?)
    };
    let first = launch(&f.source)?;
    let second = launch(&peer)?;
    for child in [first, second] {
        let output = child.wait_with_output()?;
        anyhow::ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(f.count(), 2);
    f.no_live_launches()
}

#[tokio::test]
async fn phase4_wait_timeout_does_not_answer_or_launch() -> Result<()> {
    let f = Fixture::new("clarify")?;
    let initial = Fixture::result(&f.run(&[])?)?;
    let id = initial["run_id"].as_str().unwrap();
    let state = State::discover(Some(f.state.clone()))?;
    let db = Database::open_read_only(state.db_path())?;
    let cursor = db.events_for_run(id)?.last().unwrap().sequence;
    let mut out = Vec::new();
    assert!(
        !dispatch::follow::events(
            &state,
            id,
            cursor,
            Some(dispatch::follow::Until::Finished),
            Duration::ZERO,
            &mut out
        )
        .await?
    );
    assert_eq!(serde_json::from_slice::<Value>(&out)?["timed_out"], true);
    assert_eq!(f.count(), 1);
    assert_eq!(f.loaded(id)?.outcome.waiting_on, dispatch::WaitingOn::Human);
    Ok(())
}

#[test]
fn phase4_follow_registration_racing_completion_cannot_lose_it() -> Result<()> {
    let f = Fixture::new("both-fail")?;
    fs::write(f.root.join("gate"), "")?;
    let worker = f
        .run_command(&[])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    wait_until(|| Ok(f.root.join("verification-waiting").exists()))?;
    let state = State::discover(Some(f.state.clone()))?;
    let run: dispatch::RunRecord =
        serde_json::from_slice(&fs::read(&state.list_metadata_paths()?[0])?)?;
    let cursor = Database::open_read_only(state.db_path())?
        .events_for_run(&run.id)?
        .last()
        .unwrap()
        .sequence;
    let follower = f
        .command()
        .args([
            "events",
            &run.id,
            "--after",
            &cursor.to_string(),
            "--until",
            "finished",
            "--timeout",
            "5",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Completion may commit before the follower even opens SQLite.
    fs::write(f.root.join("continue"), "")?;
    // Both pipes are drained while waiting: a child blocked writing into a
    // full pipe nobody reads would never exit.
    worker.wait_with_output()?;
    let output = follower.wait_with_output()?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records = String::from_utf8(output.stdout)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(records.last().unwrap()["reached"], true);
    assert!(
        records
            .iter()
            .any(|r| r["event"]["event_type"] == "run.finished")
    );
    let invalid = f
        .command()
        .args(["events", &run.id, "--after", "999999"])
        .output()?;
    assert!(!invalid.status.success());
    Ok(())
}
