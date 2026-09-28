//! `dispatch watch` on a terminal: see `tests/fixtures/watch_session.py`.
#![cfg(unix)]
use std::process::Command;
#[test]
fn watch_on_a_terminal_rejects_after_asking_and_accepts_without_ids() {
    let output = Command::new("python3")
        .arg(format!(
            "{}/tests/fixtures/watch_session.py",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(assert_cmd::cargo_bin!("dispatch"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
