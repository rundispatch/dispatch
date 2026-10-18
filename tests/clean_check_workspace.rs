use std::fs;

use assert_cmd::cargo_bin_cmd;

#[test]
fn clean_removes_only_listed_internal_check_workspaces_after_confirmation() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let workspaces = state.join("check-workspaces");
    let workspace = workspaces.join("a".repeat(64));
    fs::create_dir_all(workspace.join(".git")).unwrap();
    fs::write(workspace.join(".git/HEAD"), "ref: refs/heads/check\n").unwrap();
    let foreign = workspaces.join("keep-me");
    fs::create_dir_all(&foreign).unwrap();

    let run = |args: &[&str]| {
        cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&state)
            .arg("clean")
            .args(args)
            .output()
            .unwrap()
    };
    let dry = run(&["--dry-run"]);
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let text = String::from_utf8_lossy(&dry.stdout);
    assert!(text.contains(&workspace.display().to_string()), "{text}");
    assert!(!text.contains(&foreign.display().to_string()), "{text}");
    assert!(workspace.exists());

    let unasked = run(&[]);
    assert!(!unasked.status.success());
    assert!(workspace.exists());

    #[cfg(unix)]
    {
        use std::{fs::OpenOptions, os::fd::AsRawFd};

        let lock_path = state
            .join("locks")
            .join(format!("source-{}.lock", "a".repeat(64)));
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .unwrap();
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
        let busy = run(&["--yes"]);
        assert!(!busy.status.success());
        assert!(
            workspace.exists(),
            "clean must leave a check in progress alone"
        );
        drop(lock);
    }

    let cleaned = run(&["--yes"]);
    assert!(
        cleaned.status.success(),
        "{}",
        String::from_utf8_lossy(&cleaned.stderr)
    );
    assert!(!workspace.exists());
    assert!(foreign.exists());
}
