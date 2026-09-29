//! Claude-owned print protocol and included-only invocation contract.
use std::{collections::BTreeSet, fs, path::Path, time::Duration};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{HarnessRunRequest, HarnessTelemetry};
use crate::{
    config::{HarnessConfig, ResourceProfile},
    executor::{CommandSpec, ExecutionRequest, ExecutionStatus, Executor},
};

/// Explicit local owner assertion, never provider-observed quota or a cash guarantee.
/// The containing profile binds model, effort, funding, pool and authorization epoch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionEvidence {
    pub contract_version: u32,
    pub cli_version: String,
    pub executable_sha256: String,
    /// SHA256 of JSON [email, orgId] returned by supported `auth status`.
    pub account_sha256: String,
    pub checked_at: DateTime<Utc>,
    pub valid_until: DateTime<Utc>,
    pub print_mode_included: bool,
    pub usage_credits_disabled: bool,
    pub unmanaged_account: bool,
}

impl SubscriptionEvidence {
    fn validate_shape(&self) -> Result<()> {
        ensure!(
            self.contract_version == 1,
            "unsupported Claude invocation contract"
        );
        ensure!(
            !self.cli_version.trim().is_empty(),
            "Claude CLI version evidence missing"
        );
        for value in [&self.executable_sha256, &self.account_sha256] {
            ensure!(
                value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit()),
                "Claude evidence requires SHA256 identities"
            );
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        self.validate_shape()?;
        ensure!(
            self.checked_at <= Utc::now()
                && self.valid_until > Utc::now()
                && self.valid_until > self.checked_at
                && (self.valid_until - self.checked_at) <= chrono::TimeDelta::hours(24),
            "Claude funding evidence expired or invalid (maximum 24 hours)"
        );
        ensure!(
            self.print_mode_included && self.usage_credits_disabled && self.unmanaged_account,
            "Claude print inclusion, disabled usage credits and unmanaged account must be affirmatively validated"
        );
        Ok(())
    }
}

pub fn validate_profile(profile: &ResourceProfile) -> Result<()> {
    // An absent proof means unavailable, not malformed active configuration.
    if let Some(evidence) = &profile.claude_subscription {
        evidence.validate_shape()?;
    }
    ensure!(
        !matches!(
            profile.model.as_str(),
            "opus" | "sonnet" | "haiku" | "default" | "opusplan" | "fable" | "best"
        ) && !profile.model.contains(['[', ']', ',', '/']),
        "Claude requires a validated fixed model ID; aliases, context suffixes and compositions are unsupported"
    );
    ensure!(
        profile
            .effort
            .as_deref()
            .is_none_or(|e| EFFORTS.contains(&e)),
        "unsupported Claude effort; no substitution is permitted"
    );
    ensure!(
        profile.runtime == "local",
        "Claude subscription contract currently requires the local runtime"
    );
    Ok(())
}

pub fn eligibility(profile: &ResourceProfile) -> Result<()> {
    validate_profile(profile)?;
    profile
        .claude_subscription
        .as_ref()
        .context("Claude subscription funding evidence is missing")?
        .validate()
}

const TOOLS: &str = "Bash,Read,Edit,Write,Glob,Grep";

/// Efforts Claude Code accepts with `--effort`.
pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];

/// Fixed model IDs setup offers. Claude Code cannot list models, so this is
/// a suggestion; setup always also offers typing another fixed ID.
const MODELS: &[&str] = &[
    "claude-sonnet-5",
    "claude-opus-5-5",
    "claude-fable-5-1",
    "claude-haiku-4-5-20251001",
];

pub fn models() -> Vec<super::ModelOption> {
    MODELS
        .iter()
        .map(|id| super::ModelOption {
            id: (*id).to_owned(),
            efforts: EFFORTS.iter().map(|e| (*e).to_owned()).collect(),
            default_effort: Some("medium".into()),
        })
        .collect()
}

fn controls() -> Vec<String> {
    ["--setting-sources", "", "--settings",
        r#"{"disableAllHooks":true,"fastMode":false,"fallbackModel":[],"enabledPlugins":{},"disableClaudeAiConnectors":true,"syncClaudeAiSkills":false,"syncClaudeAiPlugins":false,"autoMemoryEnabled":false,"remoteControlAtStartup":false,"env":{"CLAUDE_CODE_DISABLE_TERMINAL_TITLE":"1"}}"#,
        "--strict-mcp-config", "--mcp-config", "{\"mcpServers\":{}}",
        "--disable-slash-commands", "--no-session-persistence",
        "--permission-mode", "dontAsk", "--tools", TOOLS, "--allowedTools", TOOLS,
        // End variadic tool options before an auth subcommand can be consumed.
        "--no-chrome"]
        .into_iter().map(str::to_owned).collect()
}

fn local_command(executable: &Path, args: Vec<String>) -> CommandSpec {
    let mut command = CommandSpec::new(executable.to_string_lossy()).args(args);
    // Claude's macOS Keychain lookup requires USER even with HOME present.
    // Keep this adapter-owned; do not forward credentials or the whole environment.
    if let Ok(user) = std::env::var("USER") {
        command = command.env("USER", user);
    }
    command
}

/// User/project settings are deliberately excluded by setting-sources. CLAUDE.md
/// remains available. Managed/global configuration cannot silently override us.
fn validate_local_settings() -> Result<()> {
    ensure!(
        cfg!(any(target_os = "macos", target_os = "linux"))
            && std::env::var_os("WSL_DISTRO_NAME").is_none()
            && !fs::read_to_string("/proc/sys/kernel/osrelease")
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains("microsoft"),
        "Claude subscription contract supports unmanaged macOS/Linux hosts only"
    );
    let home = std::env::var_os("HOME").context("Claude subscription requires HOME")?;
    let home = Path::new(&home);
    for path in [
        home.join(".claude/managed-settings.json"),
        "/Library/Application Support/ClaudeCode/managed-settings.json".into(),
        "/etc/claude-code/managed-settings.json".into(),
    ] {
        ensure!(
            !path.exists(),
            "Claude managed settings require a separately supported contract"
        );
        ensure!(
            !path.with_file_name("managed-settings.d").exists(),
            "Claude managed settings directory is unsupported"
        );
        ensure!(
            !path.with_file_name("managed-mcp.json").exists(),
            "Claude managed MCP configuration is unsupported"
        );
    }
    for directory in [
        std::path::PathBuf::from("/Library/Managed Preferences"),
        home.join("Library/Managed Preferences"),
    ] {
        if !directory.exists() {
            continue;
        }
        ensure!(
            !directory.join("com.anthropic.claudecode.plist").exists(),
            "Claude MDM policy is unsupported"
        );
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            ensure!(
                !entry.path().join("com.anthropic.claudecode.plist").exists(),
                "Claude user MDM policy is unsupported"
            );
        }
    }
    // Global config is separate from setting-sources. Never execute helpers or
    // print values. Fail closed on known invocation-affecting global settings.
    let path = home.join(".claude.json");
    if path.exists() {
        let raw = fs::read(&path).context("cannot inspect Claude global configuration")?;
        ensure!(
            raw.len() <= 4 * 1024 * 1024,
            "Claude global configuration exceeds inspection bound"
        );
        let value: Value =
            serde_json::from_slice(&raw).context("invalid Claude global configuration")?;
        fn conflicting(value: &Value) -> bool {
            match value {
                Value::Object(map) => map.iter().any(|(key, value)| {
                    (matches!(
                        key.as_str(),
                        "apiKeyHelper"
                            | "env"
                            | "fastMode"
                            | "fallbackModel"
                            | "modelOverrides"
                            | "advisorModel"
                            | "agent"
                            | "forceLoginMethod"
                            | "forceLoginOrgUUID"
                            | "policyHelper"
                            | "forceLoginGatewayUrl"
                    ) && !value.is_null()
                        && value != &json!(false)
                        && value != &json!({})
                        && value != &json!([]))
                        || conflicting(value)
                }),
                Value::Array(values) => values.iter().any(conflicting),
                _ => false,
            }
        }
        ensure!(
            !conflicting(&value),
            "Claude global configuration contains conflicting invocation controls"
        );
    }
    Ok(())
}

pub fn validate_executable(executable: &Path, evidence: &SubscriptionEvidence) -> Result<()> {
    evidence.validate()?;
    validate_local_settings()?;
    let executable = which::which(executable).context("Claude executable unavailable")?;
    ensure!(
        hex::encode(Sha256::digest(fs::read(executable)?)) == evidence.executable_sha256,
        "Claude executable changed; revalidate the invocation contract"
    );
    Ok(())
}

pub fn command(
    executable: &Path,
    config: &HarnessConfig,
    request: &HarnessRunRequest,
) -> Result<CommandSpec> {
    let mut args = vec![
        "-p".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
    ];
    if config.allocation_service_mode.is_some() {
        ensure!(
            config.extra_args.is_empty(),
            "Claude allocation requires extra_args to be empty"
        );
        ensure!(
            config.allocation_service_mode.as_deref() == Some("standard"),
            "Claude included-only execution requires standard service"
        );
        validate_executable(
            executable,
            config
                .claude_subscription
                .as_ref()
                .context("Claude funding evidence missing")?,
        )?;
        args.extend(controls());
    } else {
        // Legacy explicit execution still uses noninteractive bounded permissions.
        args.extend(
            [
                "--permission-mode",
                "dontAsk",
                "--tools",
                TOOLS,
                "--allowedTools",
                TOOLS,
            ]
            .map(str::to_owned),
        );
    }
    if let Some(model) = &config.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(effort) = &config.effort {
        ensure!(
            EFFORTS.contains(&effort.as_str()),
            "unsupported Claude effort"
        );
        args.extend(["--effort".into(), effort.clone()]);
    }
    if request.read_only {
        for flag in ["--tools", "--allowedTools"] {
            if let Some(index) = args.iter().position(|a| a == flag) {
                args[index + 1] = "Read,Glob,Grep".into();
            }
        }
    }
    args.extend(config.extra_args.clone());
    args.extend(["--".into(), request.prompt.clone()]);
    Ok(local_command(executable, args))
}

/// Read-only discovery uses the same cleared environment, settings controls and
/// supervisor as execution. No quota endpoint or model call is used.
pub async fn preflight(
    executable: &Path,
    evidence: &SubscriptionEvidence,
    executor: &Executor,
    request: &HarnessRunRequest,
) -> Result<Value> {
    ensure!(
        executor.config().forwarded_env.is_empty(),
        "Claude included-only execution excludes forwarded environment variables"
    );
    validate_executable(executable, evidence)?;
    // Only a version actually observed can differ. A probe that gives no answer
    // (for example it timed out under load) leaves the version unknown, and the
    // executable's bytes, checked above, already fix what it would report.
    let version = super::probe_version(executable).await.ok().flatten();
    if let Some(version) = &version {
        ensure!(
            version == &evidence.cli_version,
            "Claude CLI version changed; revalidate the invocation contract"
        );
    }
    let identity = discover_account(executable, executor, request).await?;
    ensure!(
        identity == evidence.account_sha256,
        "Claude subscription account changed; authorization must be revalidated"
    );

    // Retain only normalized nonsecret fields, never the private auth response.
    Ok(
        json!({"version":1,"auth_method":"claude.ai","account_sha256":identity,"cli_version":version,
        "quota":"unknown","funding_basis":"time_bound_user_assertion",
        "checked_at":evidence.checked_at,"valid_until":evidence.valid_until}),
    )
}

/// Supported read-only auth discovery, shared by setup and launch preflight.
/// The response never escapes this adapter; only a nonsecret identity digest does.
pub(crate) async fn discover_account(
    executable: &Path,
    executor: &Executor,
    request: &HarnessRunRequest,
) -> Result<String> {
    validate_local_settings()?;
    let temp = tempfile::tempdir()?;
    let mut args = controls();
    // --json belongs to auth status, so a misparsed command must fail rather
    // than treating the words "auth status" as an implicit model prompt.
    args.extend(["auth".into(), "status".into(), "--json".into()]);
    let mut probe = ExecutionRequest::new(
        local_command(executable, args),
        &request.workspace,
        temp.path().join("stdout"),
        temp.path().join("stderr"),
    );
    probe.timeout = Some(
        request
            .timeout
            .unwrap_or(Duration::from_secs(5))
            .min(Duration::from_secs(5)),
    );
    let result = executor
        .execute_with_cancel(probe, request.cancellation.clone())
        .await?;
    if result.status != ExecutionStatus::Succeeded {
        // No answer is no evidence about the account: refuse this launch only.
        return Err(super::PreflightInconclusive(
            "Claude did not confirm its account in time; nothing was launched; try again".into(),
        )
        .into());
    }
    let auth: Value = serde_json::from_str(&result.raw_stdout_lossy())
        .context("unsupported Claude auth-status protocol")?;
    ensure!(
        auth["loggedIn"] == true
            && auth["authMethod"] == "claude.ai"
            && auth["apiProvider"] == "firstParty",
        "Claude effective authentication is not the validated first-party subscription"
    );
    let email = auth["email"]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("Claude account identity unavailable")?;
    let org = auth["orgId"]
        .as_str()
        .context("Claude organization identity unavailable")?;
    let identity = hex::encode(Sha256::digest(serde_json::to_vec(&json!([email, org]))?));
    ensure!(
        auth.get("extraUsageEnabled") != Some(&json!(true)),
        "Claude usage credits are enabled; change them in the provider before revalidating"
    );
    Ok(identity)
}

pub fn parse_output(output: &str, expected: Option<&str>) -> HarnessTelemetry {
    let mut telemetry = HarnessTelemetry::default();
    let mut models = BTreeSet::new();
    let mut terminals = Vec::new();
    let mut invalid = false;
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            invalid = true;
            continue;
        };
        if event["type"] == "assistant" {
            if let Some(model) = event["message"]["model"].as_str() {
                models.insert(model.to_owned());
            }
            if event["message"]["error"].is_string() || event["error"].is_string() {
                invalid = true;
            }
        }
        if event["type"] == "system"
            && event["subtype"] == "init"
            && let Some(model) = event["model"].as_str()
        {
            models.insert(model.to_owned());
        }
        if event["type"] == "result" {
            if let Some(usage) = event["modelUsage"].as_object() {
                models.extend(usage.keys().cloned());
            }
            terminals.push(telemetry.events.len());
        }
        telemetry.events.push(event);
    }
    if models.len() == 1 {
        telemetry.model = models.iter().next().cloned();
    }
    // All identities remain inspectable even when the single-model field is unknown.
    let mismatch = expected.is_some_and(|value| models.iter().any(|model| model != value));
    if invalid
        || terminals.len() != 1
        || terminals
            .last()
            .is_none_or(|i| *i + 1 != telemetry.events.len())
    {
        telemetry.semantic_error = Some(
            "Claude protocol: missing, malformed, repeated or nonfinal terminal result".into(),
        );
    } else {
        let result = &telemetry.events[terminals[0]];
        if result["is_error"] != false
            || result["subtype"] != "success"
            || !result["result"].is_string()
        {
            let label = failure_label(result);
            telemetry.failure = match label {
                "capacity" => Some(crate::FailureKind::CapacityAdmission),
                "authentication" | "funding" => Some(crate::FailureKind::Authorization),
                _ => Some(crate::FailureKind::HarnessProcess),
            };
            telemetry.semantic_error =
                Some(format!("Claude unsuccessful terminal result ({label})"));
        } else if result["permission_denials"]
            .as_array()
            .is_some_and(|v| !v.is_empty())
        {
            telemetry.semantic_error =
                Some("Claude permission denied; headless tools could not complete".into());
        }
        // Only final aggregate totals, never assistant deltas or per-model totals.
        let usage = &result["usage"];
        telemetry.tokens = [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ]
        .into_iter()
        .try_fold(0u64, |sum, field| sum.checked_add(usage[field].as_u64()?));
        telemetry.token_semantics = telemetry.tokens.map(|_| "claude_final_input_output_cache_read_cache_creation_tokens; nominal_usd_is_not_cash_charge".into());
        // total_cost_usd is a nominal estimate; Candidate cost is transaction cost.
        telemetry.cost_usd = None;
    }
    if mismatch {
        telemetry.failure = Some(crate::FailureKind::Authorization);
        telemetry.semantic_error = Some(
            "Claude model substitution or mixed execution outside the authorized configuration"
                .into(),
        );
    }
    telemetry.usage_categories = usage_categories(&telemetry.events);
    telemetry
}

fn failure_label(result: &Value) -> &'static str {
    let text = format!("{} {}", result["errors"], result["result"]).to_lowercase();
    if text.contains("rate_limit") || text.contains("rate limit") || text.contains("usage limit") {
        "capacity"
    } else if text.contains("auth") {
        "authentication"
    } else if text.contains("billing") || text.contains("credit") {
        "funding"
    } else if text.contains("model") {
        "unsupported model"
    } else if text.contains("permission") {
        "permission"
    } else {
        "provider error"
    }
}

/// Terminal aggregates only. Categories and the normalized total are alternate views, never additive.
pub(crate) fn usage_categories(events: &[Value]) -> std::collections::BTreeMap<String, u64> {
    let results: Vec<_> = events.iter().filter(|e| e["type"] == "result").collect();
    if results.len() != 1 {
        return std::collections::BTreeMap::new();
    }
    [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ]
    .into_iter()
    .filter_map(|key| {
        results[0]["usage"][key]
            .as_u64()
            .map(|value| (key.into(), value))
    })
    .collect()
}

pub fn checkpoint(
    events: &[Value],
) -> Option<std::result::Result<crate::CheckpointReport, String>> {
    let result = events.last()?;
    if result["type"] != "result" || result["is_error"] != false || result["subtype"] != "success" {
        return None;
    }
    let text = result["result"].as_str()?;
    if !text.contains("dispatch_checkpoint") {
        return None;
    }
    // Envelope validation is shared; provider message selection stays here.
    super::checkpoint_envelope(text).map(|result| {
        result.and_then(|report| {
            if report
                .category
                .as_deref()
                .is_some_and(|category| category != "factual")
            {
                Err("unsupported Claude checkpoint category".into())
            } else {
                Ok(report)
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn terminal() -> Value {
        json!({"type":"result","subtype":"success","is_error":false,"result":"Done",
            "usage":{"input_tokens":10,"output_tokens":4,"cache_read_input_tokens":6,"cache_creation_input_tokens":2},
            "total_cost_usd":0.42,"modelUsage":{"fixed-model":{"inputTokens":10,"outputTokens":4}}})
    }
    #[test]
    fn claude_terminal_totals_preserve_cache_units_and_nominal_cost() {
        let output = format!(
            "{}\n{}\n",
            json!({"type":"assistant","message":{"model":"fixed-model","usage":{"input_tokens":999}}}),
            terminal()
        );
        let parsed = parse_output(&output, Some("fixed-model"));
        assert_eq!(parsed.tokens, Some(22));
        assert_eq!(parsed.cost_usd, None);
        assert_eq!(parsed.model.as_deref(), Some("fixed-model"));
        assert_eq!(parsed.effort, None);
        assert!(parsed.semantic_error.is_none());
        assert_eq!(parsed.events.last().unwrap()["total_cost_usd"], 0.42);
    }
    #[test]
    fn claude_malformed_contradictory_or_nonfinal_results_fail_closed() {
        let good = terminal().to_string();
        for output in [String::new(), "{\"type\":\"result\"".into(), format!("{good}\n{good}"),
            format!("{good}\nnot-json"), format!("{good}\n{{\"type\":\"assistant\"}}"),
            json!({"type":"result","subtype":"error_during_execution","is_error":true,"result":"Done"}).to_string()] {
            assert!(parse_output(&output, None).semantic_error.is_some(), "{output}");
        }
    }
    #[test]
    fn claude_mixed_models_are_not_collapsed_and_unknown_stays_unknown() {
        let mut result = terminal();
        result["modelUsage"]["other-model"] = json!({});
        let parsed = parse_output(&result.to_string(), Some("fixed-model"));
        assert_eq!(parsed.model, None);
        assert!(parsed.semantic_error.unwrap().contains("substitution"));
        result.as_object_mut().unwrap().remove("modelUsage");
        result.as_object_mut().unwrap().remove("usage");
        let parsed = parse_output(&result.to_string(), Some("fixed-model"));
        assert_eq!(
            (parsed.model, parsed.tokens, parsed.effort),
            (None, None, None)
        );
    }
    #[test]
    fn claude_only_successful_final_result_can_supply_checkpoint() {
        let mut result = terminal();
        result["result"] = json!(json!({"dispatch_checkpoint":{"version":1,"question":"Which label?","choices":[],"category":"factual"}}).to_string());
        assert!(checkpoint(&[result.clone()]).unwrap().is_ok());
        result["result"] = json!("{\"dispatch_checkpoint\":");
        assert!(checkpoint(&[result.clone()]).unwrap().is_err());
        result["is_error"] = json!(true);
        assert!(checkpoint(&[result.clone()]).is_none());
        assert!(
            checkpoint(&[
                json!({"type":"assistant","message":{"content":[{"text":"dispatch_checkpoint"}]}}),
                terminal()
            ])
            .is_none()
        );
    }
}
