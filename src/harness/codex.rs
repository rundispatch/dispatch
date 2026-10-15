//! Codex subscription funding. The adapter's preflight reads the account
//! through `codex app-server` and refuses to launch an included-only profile
//! when the provider reports anything other than the ChatGPT subscription the
//! user authorized at setup: another authentication, paid credits, a
//! non-standard service tier, another plan or another account. An identity it
//! cannot observe also refuses. No model is invoked.

use std::{path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

use crate::executor::safe_local_environment;

pub(super) fn parse_output(stdout: &str, _stderr: &str) -> super::HarnessTelemetry {
    let mut telemetry = super::parse_jsonl_telemetry(stdout);
    if let Some(usage) = telemetry
        .events
        .iter()
        .rev()
        .find(|event| event["type"] == "turn.completed")
        .map(|event| &event["usage"])
    {
        telemetry.usage_categories = [
            "input_tokens",
            "cached_input_tokens",
            "cache_write_input_tokens",
            "output_tokens",
            "reasoning_output_tokens",
        ]
        .into_iter()
        .filter_map(|key| usage[key].as_u64().map(|value| (key.into(), value)))
        .collect();
    }
    if let Some(failed) = telemetry
        .events
        .iter()
        .find(|event| event["type"] == "turn.failed")
    {
        let message = failed["error"]["message"]
            .as_str()
            .or_else(|| failed["message"].as_str())
            .unwrap_or("Codex turn failed");
        let nested = serde_json::from_str::<Value>(message).ok();
        telemetry.semantic_error = Some(
            nested
                .as_ref()
                .and_then(|value| value.pointer("/error/message").and_then(Value::as_str))
                .unwrap_or(message)
                .to_owned(),
        );
        telemetry.failure = Some(crate::FailureKind::HarnessProcess);
    }
    telemetry
}

/// The Codex account the user authorized for a profile, recorded by setup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountEvidence {
    /// sha256 of the account email (or id when no email is reported).
    pub account_sha256: String,
    pub checked_at: DateTime<Utc>,
}

/// What the provider reports about funding; `None` when not reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Funding {
    pub auth_mode: Option<String>,
    pub plan: Option<String>,
    pub credits_available: Option<bool>,
    pub service_tier: Option<String>,
    pub account_sha256: Option<String>,
}

impl Funding {
    pub fn from_probe(raw: &ProbeResponse) -> Self {
        let text = |value: &Value, pointer: &str| {
            value
                .pointer(pointer)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let limits = raw.limits.as_ref().unwrap_or(&Value::Null);
        let buckets = limits
            .pointer("/result/rateLimitsByLimitId")
            .and_then(Value::as_object)
            .map(|buckets| buckets.values().cloned().collect::<Vec<_>>())
            .or_else(|| {
                limits
                    .pointer("/result/rateLimits")
                    .filter(|value| !value.is_null())
                    .map(|value| vec![value.clone()])
            })
            .unwrap_or_default();
        let credits = buckets
            .iter()
            .filter_map(|bucket| {
                bucket
                    .pointer("/credits/hasCredits")
                    .and_then(Value::as_bool)
            })
            .collect::<Vec<_>>();
        Self {
            auth_mode: text(&raw.account, "/result/account/type"),
            plan: text(&raw.account, "/result/account/planType"),
            credits_available: (!credits.is_empty()).then(|| credits.iter().any(|c| *c)),
            service_tier: text(limits, "/result/serviceTier").or_else(|| {
                buckets
                    .iter()
                    .find_map(|bucket| bucket.get("serviceTier").and_then(Value::as_str))
                    .map(str::to_owned)
            }),
            account_sha256: text(&raw.account, "/result/account/email")
                .or_else(|| text(&raw.account, "/result/account/id"))
                .map(|value| hex::encode(Sha256::digest(value.as_bytes()))),
        }
    }
}

/// Why an included-only Codex profile must not launch, or `None` when the
/// observed funding is exactly the authorized subscription.
pub fn refusal(
    observed: &Funding,
    authorized: Option<&AccountEvidence>,
    funding_source: &str,
) -> Option<String> {
    if let Some(mode) = &observed.auth_mode
        && mode != "chatgpt"
    {
        return Some(format!("authentication changed to {mode}"));
    }
    if observed.credits_available == Some(true) {
        return Some("paid credits are now available".into());
    }
    if let Some(tier) = &observed.service_tier
        && !matches!(tier.as_str(), "standard" | "default")
    {
        return Some(format!("service tier changed to {tier}"));
    }
    if let Some(plan) = &observed.plan
        && funding_source.starts_with("chatgpt-")
        && !funding_source.starts_with(&format!("chatgpt-{plan}"))
    {
        return Some(format!("funding plan changed to {plan}"));
    }
    let Some(authorized) = authorized else {
        return Some("no Codex account evidence was recorded for this profile".into());
    };
    match &observed.account_sha256 {
        None => Some("the Codex account identity could not be observed".into()),
        Some(account) if *account != authorized.account_sha256 => {
            Some("the Codex account changed".into())
        }
        Some(_) => None,
    }
}

/// Probe the account and refuse on any funding difference. An unreadable
/// account is a refusal, never an assumption that nothing changed.
pub async fn preflight(
    executable: &Path,
    authorized: Option<&AccountEvidence>,
    funding_source: &str,
    timeout: Duration,
) -> Result<()> {
    // Only the Codex CLI itself is ever started with `app-server`: another
    // configured program could treat those arguments as a task.
    let probeable = executable
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "codex" | "codex.exe"));
    if !probeable {
        bail!(
            "Codex account cannot be read through {}; subscription-only launch refused",
            executable.display()
        );
    }
    let observed = match read_account(executable, timeout).await {
        Ok(raw) => Funding::from_probe(&raw),
        Err(error) => {
            bail!("Codex account could not be read ({error:#}); subscription-only launch refused")
        }
    };
    if let Some(reason) = refusal(&observed, authorized, funding_source) {
        bail!("Codex subscription check refused the launch: {reason}");
    }
    Ok(())
}

/// The raw `account/read` and `account/rateLimits/read` responses.
#[derive(Clone)]
pub struct ProbeResponse {
    pub account: Value,
    pub limits: Option<Value>,
    pub limits_error: Option<String>,
    pub source_version: String,
}

type AppServerLines = tokio::io::Lines<BufReader<tokio::process::ChildStdout>>;

/// Start `codex app-server` and complete its `initialize` handshake within
/// `stage_timeout`. Returns the child, its stdin and stdout lines, and the
/// `initialize` response.
async fn start_app_server(
    executable: &Path,
    stage_timeout: Duration,
) -> Result<(
    tokio::process::Child,
    tokio::process::ChildStdin,
    AppServerLines,
    Value,
)> {
    let mut command = Command::new(executable);
    command
        .args(["app-server", "--listen", "stdio://"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_clear()
        .kill_on_drop(true);
    for (name, value) in safe_local_environment() {
        command.env(name, value);
    }
    let mut child = command
        .spawn()
        .context("failed to start Codex app-server")?;
    let mut stdin = child.stdin.take().context("probe stdin unavailable")?;
    let stdout = child.stdout.take().context("probe stdout unavailable")?;
    let mut lines = BufReader::new(stdout).lines();
    let handshake = async {
        send(&mut stdin, json!({"method":"initialize","id":0,"params":{"clientInfo":{"name":"dispatch","title":"Dispatch","version":crate::VERSION}}})).await?;
        let initialized = response(&mut lines, 0).await?;
        send(&mut stdin, json!({"method":"initialized","params":{}})).await?;
        Ok::<_, anyhow::Error>(initialized)
    };
    let initialized = tokio::time::timeout(stage_timeout, handshake)
        .await
        .map_err(|_| anyhow::anyhow!("probe timeout before initialize response"))??;
    Ok((child, stdin, lines, initialized))
}

/// Read the Codex account and its rate limits through the supported
/// `codex app-server` JSON-RPC interface. No model is invoked.
pub async fn read_account(executable: &Path, stage_timeout: Duration) -> Result<ProbeResponse> {
    let (mut child, mut stdin, mut lines, initialized) =
        start_app_server(executable, stage_timeout).await?;
    let account_stage = async {
        send(
            &mut stdin,
            json!({"method":"account/read","id":1,"params":{"refreshToken":false}}),
        )
        .await?;
        response(&mut lines, 1).await
    };
    let account = tokio::time::timeout(stage_timeout, account_stage)
        .await
        .map_err(|_| anyhow::anyhow!("probe timeout before account response"))??;
    let source_version = initialized
        .pointer("/result/userAgent")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let limits_result = async {
        send(
            &mut stdin,
            json!({"method":"account/rateLimits/read","id":2,"params":{}}),
        )
        .await?;
        response(&mut lines, 2).await
    };
    let (limits, limits_error) = match tokio::time::timeout(stage_timeout, limits_result).await {
        Ok(Ok(value)) => (Some(value), None),
        Ok(Err(error)) => (None, Some(format!("rate-limit probe failure: {error:#}"))),
        Err(_) => (None, Some("rate-limit probe timeout".into())),
    };
    let _ = child.kill().await;
    let _ = child.wait().await;
    Ok(ProbeResponse {
        account,
        limits,
        limits_error,
        source_version,
    })
}

/// The models the installed Codex offers through `model/list`, visible ones
/// only, the default first, each with the efforts both Codex and Dispatch
/// accept. Setup offers these for selection; no model is invoked.
pub async fn list_models(
    executable: &Path,
    stage_timeout: Duration,
) -> Result<Vec<super::ModelOption>> {
    let (mut child, mut stdin, mut lines, _) = start_app_server(executable, stage_timeout).await?;
    let listed = async {
        send(
            &mut stdin,
            json!({"method":"model/list","id":1,"params":{}}),
        )
        .await?;
        response(&mut lines, 1).await
    };
    let listed = tokio::time::timeout(stage_timeout, listed).await;
    let _ = child.kill().await;
    let _ = child.wait().await;
    let listed = listed.map_err(|_| anyhow::anyhow!("model list timeout"))??;
    Ok(parse_models(&listed))
}

fn parse_models(listed: &Value) -> Vec<super::ModelOption> {
    let mut models = listed
        .pointer("/result/data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| m.get("hidden").and_then(Value::as_bool) != Some(true))
        .filter_map(|m| {
            let id = m.get("id").and_then(Value::as_str)?.to_owned();
            crate::config::validate_model(Some(&id)).ok()?;
            let efforts = m
                .get("supportedReasoningEfforts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|e| e.get("reasoningEffort").and_then(Value::as_str))
                .filter(|e| crate::config::validate_effort(Some(e)).is_ok())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let default_effort = m
                .get("defaultReasoningEffort")
                .and_then(Value::as_str)
                .filter(|e| efforts.iter().any(|x| x == e))
                .map(str::to_owned);
            let is_default = m.get("isDefault").and_then(Value::as_bool) == Some(true);
            Some((
                is_default,
                super::ModelOption {
                    id,
                    efforts,
                    default_effort,
                },
            ))
        })
        .collect::<Vec<_>>();
    // Stable: the default model first, then the order Codex listed them.
    models.sort_by_key(|(is_default, _)| !*is_default);
    models.into_iter().map(|(_, model)| model).collect()
}

async fn send(stdin: &mut tokio::process::ChildStdin, message: Value) -> Result<()> {
    stdin.write_all(message.to_string().as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

async fn response(lines: &mut AppServerLines, id: i64) -> Result<Value> {
    while let Some(line) = lines.next_line().await? {
        let value: Value = serde_json::from_str(&line).context("invalid app-server JSON")?;
        if value.get("id").and_then(Value::as_i64) == Some(id) {
            if let Some(error) = value.get("error") {
                bail!("app-server request {id} failed: {error}");
            }
            return Ok(value);
        }
    }
    bail!("app-server closed before response {id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str = "fixture@example.invalid";

    #[test]
    fn model_list_offers_visible_models_default_first_with_accepted_efforts() {
        // Shaped like codex-cli 0.155's `model/list` response.
        let effort = |e: &str| json!({"reasoningEffort": e, "description": ""});
        let listed = json!({"id": 1, "result": {"data": [
            {"id": "gpt-6-sol", "hidden": false, "isDefault": false,
             "supportedReasoningEfforts": [effort("low"), effort("medium")],
             "defaultReasoningEffort": "medium"},
            {"id": "gpt-6-astra", "hidden": false, "isDefault": true,
             "supportedReasoningEfforts": [effort("low"), effort("xhigh"), effort("max"), effort("ultra")],
             "defaultReasoningEffort": "ultra"},
            {"id": "internal-preview", "hidden": true, "isDefault": false,
             "supportedReasoningEfforts": [effort("low")]},
        ]}});
        let models = parse_models(&listed);
        let ids = models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids, ["gpt-6-astra", "gpt-6-sol"]);
        // Efforts Dispatch does not accept are never offered, and a default
        // that is not offered is not focused.
        assert_eq!(models[0].efforts, ["low", "xhigh"]);
        assert_eq!(models[0].default_effort, None);
        assert_eq!(models[1].default_effort.as_deref(), Some("medium"));
        assert!(parse_models(&json!({"id": 1, "result": {}})).is_empty());
    }

    fn evidence() -> AccountEvidence {
        AccountEvidence {
            account_sha256: hex::encode(Sha256::digest(ACCOUNT.as_bytes())),
            checked_at: Utc::now(),
        }
    }

    fn probe(account: Value, limits: Value) -> ProbeResponse {
        ProbeResponse {
            account: json!({ "result": { "account": account } }),
            limits: Some(json!({ "result": limits })),
            limits_error: None,
            source_version: "fixture".into(),
        }
    }

    fn authorized_account() -> Value {
        json!({"type": "chatgpt", "planType": "plus", "email": ACCOUNT})
    }

    fn standard_limits() -> Value {
        json!({"rateLimitsByLimitId": {"codex": {"credits": {"hasCredits": false}}}})
    }

    fn verdict(
        account: Value,
        limits: Value,
        evidence: Option<&AccountEvidence>,
    ) -> Option<String> {
        refusal(
            &Funding::from_probe(&probe(account, limits)),
            evidence,
            "chatgpt-plus",
        )
    }

    #[test]
    fn authorized_subscription_launches() {
        assert_eq!(
            verdict(authorized_account(), standard_limits(), Some(&evidence())),
            None
        );
    }

    // C1
    #[test]
    fn other_authentication_refuses() {
        let account = json!({"type": "apiKey", "planType": "plus", "email": ACCOUNT});
        let reason = verdict(account, standard_limits(), Some(&evidence())).unwrap();
        assert!(
            reason.contains("authentication changed to apiKey"),
            "{reason}"
        );
    }

    // C2
    #[test]
    fn available_paid_credits_refuse() {
        let limits = json!({"rateLimitsByLimitId": {
            "codex": {"credits": {"hasCredits": false}},
            "other": {"credits": {"hasCredits": true}}
        }});
        let reason = verdict(authorized_account(), limits, Some(&evidence())).unwrap();
        assert!(reason.contains("paid credits"), "{reason}");
    }

    // C3
    #[test]
    fn non_standard_service_tier_refuses() {
        let limits = json!({"serviceTier": "priority", "rateLimitsByLimitId": {}});
        let reason = verdict(authorized_account(), limits, Some(&evidence())).unwrap();
        assert!(
            reason.contains("service tier changed to priority"),
            "{reason}"
        );
    }

    // C4
    #[test]
    fn another_plan_refuses() {
        let account = json!({"type": "chatgpt", "planType": "pro", "email": ACCOUNT});
        let reason = verdict(account, standard_limits(), Some(&evidence())).unwrap();
        assert!(reason.contains("funding plan changed to pro"), "{reason}");
    }

    // C5
    #[test]
    fn another_account_refuses() {
        let account =
            json!({"type": "chatgpt", "planType": "plus", "email": "other@example.invalid"});
        let reason = verdict(account, standard_limits(), Some(&evidence())).unwrap();
        assert!(reason.contains("account changed"), "{reason}");
    }

    // Decision: an identity that cannot be observed refuses.
    #[test]
    fn unobservable_identity_refuses() {
        let account = json!({"type": "chatgpt", "planType": "plus"});
        let reason = verdict(account, standard_limits(), Some(&evidence())).unwrap();
        assert!(reason.contains("could not be observed"), "{reason}");
    }

    #[test]
    fn missing_evidence_refuses() {
        let reason = verdict(authorized_account(), standard_limits(), None).unwrap();
        assert!(reason.contains("no Codex account evidence"), "{reason}");
    }

    #[test]
    fn an_account_id_stands_in_for_a_missing_email() {
        let account = json!({"type": "chatgpt", "planType": "plus", "id": "account-a"});
        let evidence = AccountEvidence {
            account_sha256: hex::encode(Sha256::digest(b"account-a")),
            checked_at: Utc::now(),
        };
        assert_eq!(verdict(account, standard_limits(), Some(&evidence)), None);
    }

    #[tokio::test]
    async fn an_executable_that_is_not_codex_is_never_probed() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("ran");
        let fake = temp.path().join("codex-fixture");
        std::fs::write(&fake, format!("#!/bin/sh\n: > '{}'\n", marker.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let result = preflight(
            &fake,
            Some(&evidence()),
            "chatgpt-plus",
            Duration::from_secs(1),
        )
        .await;
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("cannot be read through"), "{error}");
        assert!(!marker.exists(), "the configured program must not run");
    }

    #[tokio::test]
    async fn an_unreadable_account_refuses() {
        let result = preflight(
            Path::new("/nonexistent/codex"),
            Some(&evidence()),
            "chatgpt-plus",
            Duration::from_secs(1),
        )
        .await;
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("could not be read"), "{error}");
    }
}
