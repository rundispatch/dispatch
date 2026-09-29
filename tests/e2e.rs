use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use assert_cmd::cargo_bin_cmd;
use rusqlite::Connection;
use serde_json::Value;

#[test]
fn non_git_native_run_applies_safely_end_to_end() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("plain-source");
    let state = temp.path().join("dispatch-state");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("original.txt"), "unchanged baseline\n").unwrap();
    fs::write(
        source.join("dispatch.yml"),
        "execution:\n  timeout_secs: 30\nchecks:\n  verify:\n    - test -f dispatch-fake-good.txt || test -f dispatch-fake-bad.txt\n",
    )
    .unwrap();
    let run = |task: &str, agent: &str| {
        let output = cargo_bin_cmd!("dispatch")
            .args(["--state-dir"])
            .arg(&state)
            .arg("run")
            .arg(&source)
            .arg("--allow-unsafe-local")
            .args(["--task", task, "--agent", agent, "--json"])
            .assert()
            .success()
            .get_output()
            .clone();
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        result["run_id"].as_str().unwrap().to_owned()
    };

    let run_id = run("Make a deterministic candidate artifact.", "fake-good");
    // The work happened in an isolated copy; the source is untouched.
    assert_eq!(
        fs::read_to_string(source.join("original.txt")).unwrap(),
        "unchanged baseline\n"
    );
    assert!(!source.join("dispatch-fake-good.txt").exists());
    let metadata_path = state.join("runs").join(&run_id).join("metadata.json");
    let metadata: Value = serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
    assert_eq!(metadata["source_kind"], "directory");
    assert_eq!(metadata["outcome"]["work_result"], "ready");
    assert_eq!(metadata["baseline_checks"][0]["status"], "failed");
    let candidate = &metadata["candidates"][0];
    assert_eq!(candidate["status"], "completed");
    assert_eq!(candidate["checks"][0]["status"], "passed");
    assert_eq!(candidate["diff_stats"]["files_changed"], 1);
    assert!(
        Path::new(candidate["workspace_path"].as_str().unwrap())
            .join("original.txt")
            .is_file()
    );
    assert!(Path::new(candidate["diff_path"].as_str().unwrap()).is_file());

    // A human accept applies the result and keeps the explanation verbatim.
    let explanation = "\nThe exact regression behavior is right.\nKeep this spacing verbatim.  \n";
    let explanation_path = temp.path().join("reasoning.txt");
    fs::write(&explanation_path, explanation).unwrap();
    cargo_bin_cmd!("dispatch")
        .args(["--state-dir"])
        .arg(&state)
        .args(["accept", &run_id, "--reason", "correctness"])
        .arg("--explanation-file")
        .arg(&explanation_path)
        .assert()
        .success();
    assert!(source.join("dispatch-fake-good.txt").is_file());
    assert_eq!(
        fs::read_to_string(source.join("original.txt")).unwrap(),
        "unchanged baseline\n"
    );
    let database = Connection::open(state.join("dispatch.db")).unwrap();
    let stored_explanation: String = database
        .query_row(
            "SELECT explanation FROM goal_feedback_revisions WHERE run_id = ?1",
            [&run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_explanation, explanation);

    // A later run snapshots the now-current source. A change that occupies
    // the result's own output afterwards makes it stale; nothing is applied.
    let second_run = run("Create another fake artifact.", "fake-bad");
    for name in ["dispatch-fake-good.txt", "dispatch-fake-bad.txt"] {
        fs::write(source.join(name), "user wrote this after the run\n").unwrap();
    }
    cargo_bin_cmd!("dispatch")
        .args(["--state-dir"])
        .arg(&state)
        .args(["accept", &second_run])
        .assert()
        .failure()
        .stderr(predicates::str::contains("this work is stale"));
    assert_eq!(
        fs::read_to_string(source.join("dispatch-fake-bad.txt")).unwrap(),
        "user wrote this after the run\n"
    );
}

#[cfg(unix)]
fn drain(mut pipe: impl std::io::Read + Send + 'static) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let _ = std::io::copy(&mut pipe, &mut std::io::sink());
    })
}

#[cfg(unix)]
#[test]
fn interrupt_cancels_children_and_persists_terminal_status() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let state = temp.path().join("state");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("input.txt"), "baseline\n").unwrap();
    fs::write(
        source.join("dispatch.yml"),
        "execution:\n  timeout_secs: 30\n",
    )
    .unwrap();

    let mut child = Command::new(assert_cmd::cargo_bin!("dispatch"))
        .args(["--state-dir"])
        .arg(&state)
        .arg("run")
        .arg(&source)
        .args([
            "--task",
            "Wait until interrupted.",
            "--agent",
            "fake-timeout",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Drained while waiting: a child blocked writing into a full pipe nobody
    // reads would never exit (pipes can be as small as 512 bytes).
    let drains = [
        drain(child.stdout.take().unwrap()),
        drain(child.stderr.take().unwrap()),
    ];

    let deadline = Instant::now() + Duration::from_secs(8);
    let metadata_path = loop {
        if let Some(path) = first_metadata_path(&state) {
            let metadata: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            if metadata["status"] == "running" {
                break path;
            }
        }
        assert!(
            Instant::now() < deadline,
            "run never reached running status"
        );
        thread::sleep(Duration::from_millis(20));
    };

    // SAFETY: this targets only the Dispatch child spawned by this test.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let exit_deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= exit_deadline {
            let _ = child.kill();
            panic!("Dispatch did not exit after SIGINT");
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success());
    for drain in drains {
        drain.join().unwrap();
    }

    let metadata: Value = serde_json::from_slice(&fs::read(metadata_path).unwrap()).unwrap();
    // The native engine records a user interrupt as cancelled work.
    assert_eq!(metadata["execution"]["failure"], "cancelled");
    assert_eq!(metadata["outcome"]["lifecycle"], "finished");
    assert_eq!(metadata["outcome"]["work_result"], "cancelled");
    assert_eq!(metadata["outcome"]["verification"], "not_run");
    assert_eq!(metadata["candidates"][0]["status"], "cancelled");
    assert_eq!(metadata["attempts"][0]["outcome"], "cancelled");
}

fn first_metadata_path(state: &Path) -> Option<PathBuf> {
    fs::read_dir(state.join("runs"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("metadata.json"))
        .find(|path| path.is_file())
}
