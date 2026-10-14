use std::fs;

use dispatch::{
    config::HarnessConfig,
    harness::{HarnessAdapter, HarnessRunRequest, HermesAdapter},
};

#[test]
fn hermes_uses_attempt_usage_file_and_configured_identity() {
    let temp = tempfile::tempdir().unwrap();
    let adapter = HermesAdapter::new(HarnessConfig {
        model: Some("anthropic/claude-test".into()),
        provider: Some("anthropic".into()),
        ..HarnessConfig::default()
    });
    let request = HarnessRunRequest::new(temp.path(), "do work", temp.path().join("attempt"));
    let command = adapter.build_command(&request).unwrap();
    assert_eq!(command.program, "hermes");
    assert_eq!(
        command.args,
        vec![
            "-z",
            "do work",
            "--usage-file",
            request.output_dir.join("usage.json").to_str().unwrap(),
            "--yolo",
            "--model",
            "anthropic/claude-test",
            "--provider",
            "anthropic"
        ]
    );
}

#[test]
fn hermes_success_reads_reported_usage_and_plain_stdout() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("usage.json"), r#"{"input_tokens":12,"output_tokens":5,"cache_read_tokens":3,"cache_write_tokens":0,"reasoning_tokens":2,"total_tokens":20,"model":"anthropic/claude-test","provider":"anthropic","failed":false}"#).unwrap();
    let adapter = HermesAdapter::new(HarnessConfig::default());
    let usage = adapter.parse_output_in("Done.\n", "", temp.path());
    assert_eq!(adapter.final_result("Done.\n").as_deref(), Ok("Done."));
    assert_eq!(usage.tokens, Some(20));
    assert_eq!(
        usage.token_semantics.as_deref(),
        Some("hermes-reported-total")
    );
    assert_eq!(usage.model.as_deref(), Some("anthropic/claude-test"));
    assert_eq!(usage.usage_categories["reasoning_tokens"], 2);
    assert_eq!(usage.usage_categories["cache_read_tokens"], 3);
    assert_eq!(usage.usage_categories["cache_write_tokens"], 0);
    assert!(usage.semantic_error.is_none());
}

#[test]
fn hermes_failure_preserves_null_as_unknown() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("usage.json"), r#"{"estimated_cost_usd":null,"input_tokens":null,"output_tokens":null,"cache_read_tokens":null,"cache_write_tokens":null,"reasoning_tokens":null,"total_tokens":null,"model":null,"provider":null,"failed":true,"failure":"No inference provider configured. Run 'hermes model' to choose a provider and model, or set an API key (OPENROUTER_API_KEY, OPENAI_API_KEY, etc.) in ~/.hermes/.env."}"#).unwrap();
    let adapter = HermesAdapter::new(HarnessConfig::default());
    let usage = adapter.parse_output_in("", "hermes -z: agent failed", temp.path());
    assert_eq!(usage.tokens, None);
    assert!(usage.usage_categories.is_empty());
    assert_eq!(usage.cost_usd, None);
    assert!(
        usage
            .semantic_error
            .unwrap()
            .contains("No inference provider configured")
    );
    assert!(adapter.final_result("").is_err());
}
