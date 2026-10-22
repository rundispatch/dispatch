#![cfg(unix)]

use std::process::Command;

#[test]
fn native_launch_consent_is_terminal_granted_and_exact() {
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/native_consent.py"
        ))
        .arg(assert_cmd::cargo_bin!("dispatch"))
        .output()
        .expect("native consent fixture starts");
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
