use serde_json::Value;

use super::{HarnessTelemetry, parse_jsonl_telemetry};

pub(super) fn parse_output(stdout: &str, _stderr: &str) -> HarnessTelemetry {
    let mut telemetry = parse_jsonl_telemetry(stdout);
    let init_model = telemetry
        .events
        .iter()
        .find(|event| event["type"] == "system" && event["subtype"] == "init")
        .and_then(|event| event["model"].as_str())
        .map(str::to_owned);
    telemetry.model = init_model.or(telemetry.model);
    if let Some(result) = telemetry
        .events
        .iter()
        .rev()
        .find(|e| e["type"] == "result")
    {
        telemetry.usage_categories = [
            ("inputTokens", "input_tokens"),
            ("outputTokens", "output_tokens"),
            ("cacheReadTokens", "cache_read_tokens"),
            ("cacheWriteTokens", "cache_write_tokens"),
        ]
        .into_iter()
        .filter_map(|(source, category)| {
            result["usage"][source]
                .as_u64()
                .map(|value| (category.into(), value))
        })
        .collect();
        let usage = &telemetry.usage_categories;
        if usage.len() == 4 {
            telemetry.tokens = usage["input_tokens"]
                .checked_add(usage["output_tokens"])
                .and_then(|sum| sum.checked_add(usage["cache_write_tokens"]));
            telemetry.token_semantics = telemetry
                .tokens
                .map(|_| "uncached-input+output+cache-creation".into());
        }
        if result["is_error"] == true || result["subtype"].as_str().is_some_and(|s| s != "success")
        {
            telemetry.semantic_error = Some(
                result["result"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or("Cursor reported an unsuccessful result")
                    .to_owned(),
            );
            telemetry.failure = Some(crate::FailureKind::HarnessProcess);
        }
    }
    telemetry
}

pub(super) fn final_result(stdout: &str) -> std::result::Result<String, String> {
    let result = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .rfind(|event| event["type"] == "result")
        .ok_or("missing Cursor result event")?;
    if result["is_error"] == true || result["subtype"] != "success" {
        return Err("Cursor reported an unsuccessful result".into());
    }
    let text = result["result"]
        .as_str()
        .ok_or("malformed Cursor final result")?;
    if text.len() > 32768 {
        return Err("Cursor final result exceeds 32 KiB".into());
    }
    Ok(text.to_owned())
}
