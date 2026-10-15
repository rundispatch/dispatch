use std::{collections::BTreeMap, fs, path::Path};

use serde_json::Value;

use super::HarnessTelemetry;

pub(super) fn final_result(stdout: &str) -> std::result::Result<String, String> {
    let text = stdout.trim();
    if text.is_empty() {
        return Err("missing Hermes final response".into());
    }
    if text.len() > 32768 {
        return Err("Hermes final response exceeds 32 KiB".into());
    }
    Ok(text.to_owned())
}

pub(super) fn parse_output(stdout: &str, stderr: &str, output_dir: &Path) -> HarnessTelemetry {
    let mut telemetry = HarnessTelemetry::default();
    let Ok(raw) = fs::read_to_string(output_dir.join("usage.json")) else {
        return telemetry;
    };
    let Ok(usage) = serde_json::from_str::<Value>(&raw) else {
        telemetry.semantic_error = Some("Hermes wrote an invalid usage file".into());
        telemetry.failure = Some(crate::FailureKind::HarnessProcess);
        return telemetry;
    };
    telemetry.usage_categories = [
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "reasoning_tokens",
        "total_tokens",
    ]
    .into_iter()
    .filter_map(|key| usage[key].as_u64().map(|value| (key.into(), value)))
    .collect::<BTreeMap<_, _>>();
    telemetry.tokens = usage["total_tokens"].as_u64();
    telemetry.token_semantics = telemetry.tokens.map(|_| "hermes-reported-total".into());
    telemetry.model = usage["model"].as_str().map(str::to_owned);
    // Hermes 0.18.2 exits 0 when the provider refuses a call, prints the
    // provider's message on stdout, and leaves `failure` out of the usage file;
    // its own setup errors go to stderr and `failure`.
    if usage["failed"] == true {
        telemetry.semantic_error = Some(
            [usage["failure"].as_str().unwrap_or(""), stderr, stdout]
                .into_iter()
                .map(str::trim)
                .find(|message| !message.is_empty())
                .unwrap_or("Hermes reported a failed run without a message")
                .to_owned(),
        );
        telemetry.failure = Some(crate::FailureKind::HarnessProcess);
    }
    telemetry
}
