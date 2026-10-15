use dispatch::{
    config::HarnessConfig,
    harness::{CodexAdapter, CursorAdapter, HarnessAdapter},
};

const CURSOR_RESULT: &str = "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"Done.\",\"usage\":{\"inputTokens\":25959,\"outputTokens\":1526,\"cacheReadTokens\":50304,\"cacheWriteTokens\":0}}";
const CODEX_USAGE: &str = "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":83665,\"cached_input_tokens\":65408,\"cache_write_input_tokens\":0,\"output_tokens\":802,\"reasoning_output_tokens\":99}}";
const CODEX_FAILURE: &str = "{\"type\":\"turn.failed\",\"error\":{\"message\":\"{\\\"type\\\":\\\"error\\\",\\\"status\\\":400,\\\"error\\\":{\\\"type\\\":\\\"invalid_request_error\\\",\\\"message\\\":\\\"The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account.\\\"}}\"}}";

#[test]
fn cursor_result_preserves_cache_usage_model_and_non_json_noise() {
    let adapter = CursorAdapter::new(HarnessConfig::default());
    let stdout = format!(
        "Using worktree: /tmp/p0-cursor\n{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"Auto\"}}\n{CURSOR_RESULT}\n"
    );
    let usage = adapter.parse_output(&stdout, "");
    assert_eq!(adapter.final_result(&stdout).as_deref(), Ok("Done."));
    assert_eq!(usage.model.as_deref(), Some("Auto"));
    assert_eq!(usage.tokens, Some(27_485));
    assert_eq!(
        usage.token_semantics.as_deref(),
        Some("uncached-input+output+cache-creation")
    );
    assert_eq!(usage.usage_categories["input_tokens"], 25_959);
    assert_eq!(usage.usage_categories["output_tokens"], 1_526);
    assert_eq!(usage.usage_categories["cache_read_tokens"], 50_304);
    assert_eq!(usage.usage_categories["cache_write_tokens"], 0);
    assert!(usage.semantic_error.is_none());

    let with_cache_write = stdout.replace("\"cacheWriteTokens\":0", "\"cacheWriteTokens\":7");
    let usage = adapter.parse_output(&with_cache_write, "");
    assert_eq!(usage.tokens, Some(27_492));
    assert_eq!(usage.usage_categories["cache_read_tokens"], 50_304);
    assert_eq!(usage.usage_categories["cache_write_tokens"], 7);
}

#[test]
fn cursor_result_reports_semantic_failures() {
    let adapter = CursorAdapter::new(HarnessConfig::default());
    for result in [
        r#"{"type":"result","subtype":"error","is_error":false,"result":"Cursor stopped"}"#,
        r#"{"type":"result","subtype":"success","is_error":true,"result":"Cursor stopped"}"#,
    ] {
        let usage = adapter.parse_output(result, "");
        assert_eq!(usage.semantic_error.as_deref(), Some("Cursor stopped"));
        assert!(adapter.final_result(result).is_err());
    }
}

#[test]
fn codex_keeps_its_total_and_records_extra_counters() {
    let adapter = CodexAdapter::new(HarnessConfig::default());
    let stdout = format!(
        "not JSON\n{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"Done.\"}}}}\n{CODEX_USAGE}\n"
    );
    let usage = adapter.parse_output(&stdout, "");
    assert_eq!(adapter.final_result(&stdout).as_deref(), Ok("Done."));
    assert_eq!(usage.tokens, Some(19_059));
    assert_eq!(
        usage.token_semantics.as_deref(),
        Some("uncached-input+output+cache-creation")
    );
    assert_eq!(usage.usage_categories["input_tokens"], 83_665);
    assert_eq!(usage.usage_categories["cached_input_tokens"], 65_408);
    assert_eq!(usage.usage_categories["cache_write_input_tokens"], 0);
    assert_eq!(usage.usage_categories["output_tokens"], 802);
    assert_eq!(usage.usage_categories["reasoning_output_tokens"], 99);
}

#[test]
fn codex_exposes_the_nested_model_failure() {
    let adapter = CodexAdapter::new(HarnessConfig::default());
    let usage = adapter.parse_output(CODEX_FAILURE, "");
    assert_eq!(
        usage.semantic_error.as_deref(),
        Some("The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account.")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn codex_process_failure_keeps_the_harness_message() {
    use dispatch::{
        config::ExecutionConfig,
        executor::Executor,
        harness::{HarnessRunRequest, run_harness},
    };
    use std::{fs, os::unix::fs::PermissionsExt};
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex-fixture");
    fs::write(&script, format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo fixture; exit 0; fi\ncat <<'CODEX_EVENT'\n{CODEX_FAILURE}\nCODEX_EVENT\nexit 1\n")).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let adapter = CodexAdapter::new(HarnessConfig {
        executable: Some(script),
        ..HarnessConfig::default()
    });
    let result = run_harness(
        &adapter,
        &Executor::new(ExecutionConfig::default()),
        HarnessRunRequest::new(temp.path(), "task", temp.path().join("attempt")),
    )
    .await
    .unwrap();
    assert_eq!(
        result.execution.status,
        dispatch::executor::ExecutionStatus::Failed
    );
    assert!(
        result
            .execution
            .error
            .unwrap()
            .contains("model is not supported when using Codex with a ChatGPT account")
    );
}
