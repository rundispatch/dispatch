//! Funding-identity proof suite (0.4.1 S5a; the gate for deleting capacity and
//! admission in S6b, which must leave this file unchanged). Each case runs the
//! real binary against fixture provider executables; see
//! `tests/fixtures/funding_safety.py` and `docs/plan-0.4.1-pruning.md`.
#![cfg(unix)]
use std::process::Command;

fn scenario(provider: &str, name: &str) {
    let output = Command::new("python3")
        .arg(format!(
            "{}/tests/fixtures/funding_safety.py",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(assert_cmd::cargo_bin!("dispatch"))
        .arg(name)
        .env("DISPATCH_FIXTURE_PROVIDER", provider)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("Python standard-library fixture");
    assert!(
        output.status.success(),
        "{provider} {name}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

macro_rules! cases {
    ($($test:ident => ($provider:literal, $name:literal)),* $(,)?) => {
        $(#[test] fn $test() { scenario($provider, $name); })*
    };
}

cases! {
    codex_authorized_subscription_launches => ("codex", "codex_authorized"),
    codex_other_authentication_is_refused => ("codex", "auth"),
    codex_available_paid_credits_are_refused => ("codex", "credits"),
    codex_non_standard_service_tier_is_refused => ("codex", "tier"),
    codex_other_plan_is_refused => ("codex", "plan"),
    codex_other_account_is_refused => ("codex", "account"),
    codex_unobservable_identity_is_refused => ("codex", "unknown"),
    codex_unreadable_account_is_refused => ("codex", "codex_unreadable"),
    codex_missing_account_evidence_is_refused => ("codex", "codex_missing_evidence"),
    codex_change_at_the_spawn_boundary_is_refused_and_recorded => ("codex", "codex_spawn_boundary"),
    codex_profile_changed_after_selection_is_refused => ("codex", "codex_profile_changed_after_selection"),
    codex_refusal_is_sticky_until_reauthorized => ("codex", "codex_sticky"),
    claude_refusal_is_sticky_until_reauthorized => ("claude", "claude_sticky"),
    claude_version_probe_timeout_is_not_a_change => ("claude", "claude_slow_version"),
    claude_observed_version_change_is_refused_and_sticky => ("claude", "claude_version_changed"),
    claude_unanswered_account_probe_refuses_that_launch_only => ("claude", "claude_slow_account"),
}
