//! `dispatch start` and `dispatch stop`: one background owner per project,
//! found through the serve lock and signalled only while its record names
//! the exact live process. No agent profile is involved.
#![cfg(unix)]

use std::{
    fs,
    os::fd::AsRawFd,
    path::PathBuf,
    process::{Command, Output},
    time::{Duration, Instant},
};

use serde_json::Value;
use sha2::{Digest, Sha256};

struct Project {
    _temp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
}

impl Project {
    /// A plain directory (not Git), with no `resources.yml` anywhere.
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("notes.txt"), "hello\n").unwrap();
        fs::write(root.join("dispatch.yml"), "coherence:\n  poll_secs: 1\n").unwrap();
        Self {
            root: fs::canonicalize(&root).unwrap(),
            state: temp.path().join("state"),
            _temp: temp,
        }
    }

    fn dispatch(&self, args: &[&str]) -> Output {
        Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&self.state)
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap()
    }

    fn key(&self) -> String {
        hex::encode(Sha256::digest(self.root.to_string_lossy().as_bytes()))
    }

    fn record_path(&self) -> PathBuf {
        self.state
            .join("watchers")
            .join(format!("{}.json", self.key()))
    }

    fn lock_path(&self) -> PathBuf {
        self.state
            .join("locks")
            .join(format!("serve-{}.lock", self.key()))
    }

    fn record(&self) -> Option<Value> {
        serde_json::from_slice(&fs::read(self.record_path()).ok()?).ok()
    }

    fn owner_pid(&self) -> i32 {
        self.record().expect("a watcher record")["identity"]["pid"]
            .as_i64()
            .unwrap() as i32
    }

    fn lock_is_held(&self) -> bool {
        let Ok(file) = fs::OpenOptions::new().write(true).open(self.lock_path()) else {
            return false;
        };
        // SAFETY: a non-blocking probe on a descriptor this function owns;
        // closing it on return releases the probe.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) != 0 }
    }
}

/// Kills whatever owner a test left running.
impl Drop for Project {
    fn drop(&mut self) {
        if let Some(record) = self.record()
            && let Some(pid) = record["identity"]["pid"].as_i64()
            && self.lock_is_held()
        {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(300);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn start_returns_at_once_and_stop_ends_exactly_that_owner() {
    let project = Project::new();
    let output = project.dispatch(&["start"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(text(&output).contains("✓ Watching"), "{}", text(&output));
    assert!(project.lock_is_held());
    // `start` returned while the owner it started keeps watching.
    let pid = project.owner_pid();
    assert!(alive(pid));
    assert!(!project.state.join("resources.yml").exists());

    let again = project.dispatch(&["start"]);
    assert!(again.status.success());
    assert!(
        text(&again).contains("Already watching"),
        "{}",
        text(&again)
    );
    assert_eq!(project.owner_pid(), pid);

    let stopped = project.dispatch(&["stop"]);
    assert!(stopped.status.success(), "{}", text(&stopped));
    assert!(
        text(&stopped).contains("stopped watching"),
        "{}",
        text(&stopped)
    );
    assert!(!project.lock_is_held());
    assert!(project.record().is_none());
    wait_until("the owner to exit", || !alive(pid));

    let idle = project.dispatch(&["stop"]);
    assert!(idle.status.success());
    assert!(text(&idle).contains("Not watching"), "{}", text(&idle));
}

#[test]
fn start_waits_for_an_owner_that_is_slow_to_become_ready() {
    // A loaded machine: the owner takes longer than 10 s to be ready.
    let project = Project::new();
    let output = Command::new(assert_cmd::cargo_bin!("dispatch"))
        .arg("--state-dir")
        .arg(&project.state)
        .arg("start")
        .current_dir(&project.root)
        .env("DISPATCH_REGISTRATION_FAULT", "owner_ready:delay=12000")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output));
    assert!(text(&output).contains("✓ Watching"), "{}", text(&output));
    assert!(alive(project.owner_pid()));
}

#[test]
fn stop_waits_for_an_owner_that_is_slow_to_exit() {
    // A loaded machine: the owner takes longer than 10 s to let go.
    let project = Project::new();
    let output = Command::new(assert_cmd::cargo_bin!("dispatch"))
        .arg("--state-dir")
        .arg(&project.state)
        .arg("start")
        .current_dir(&project.root)
        .env("DISPATCH_REGISTRATION_FAULT", "owner_stopping:delay=12000")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output));
    let pid = project.owner_pid();
    let stopped = project.dispatch(&["stop"]);
    assert!(stopped.status.success(), "{}", text(&stopped));
    assert!(
        text(&stopped).contains("stopped watching"),
        "{}",
        text(&stopped)
    );
    assert!(!project.lock_is_held());
    wait_until("the owner to exit", || !alive(pid));
}

#[test]
fn concurrent_starts_leave_one_owner() {
    let project = Project::new();
    let starts: Vec<_> = (0..3)
        .map(|_| {
            Command::new(assert_cmd::cargo_bin!("dispatch"))
                .arg("--state-dir")
                .arg(&project.state)
                .arg("start")
                .current_dir(&project.root)
                .spawn()
                .unwrap()
        })
        .collect();
    for mut start in starts {
        assert!(start.wait().unwrap().success());
    }
    let owners = Command::new("pgrep")
        .args([
            "-f",
            &format!("serve --background --root {}", project.root.display()),
        ])
        .output()
        .unwrap();
    let owners = String::from_utf8_lossy(&owners.stdout);
    assert_eq!(owners.lines().count(), 1, "{owners}");
    assert_eq!(owners.trim(), project.owner_pid().to_string());
}

#[test]
fn stop_leaves_other_projects_watched() {
    let one = Project::new();
    let mut two = Project::new();
    two.state = one.state.clone();
    assert!(one.dispatch(&["start"]).status.success());
    assert!(two.dispatch(&["start"]).status.success());
    let other_pid = two.owner_pid();
    assert!(one.dispatch(&["stop"]).status.success());
    assert!(alive(other_pid));
    assert!(two.lock_is_held());
}

#[test]
fn a_crashed_owner_is_not_watching_and_start_recovers() {
    let project = Project::new();
    assert!(project.dispatch(&["start"]).status.success());
    let pid = project.owner_pid();
    unsafe { libc::kill(pid, libc::SIGKILL) };
    wait_until("the lock to be released", || !project.lock_is_held());
    // The leftover record claims nothing.
    assert!(project.record().is_some());

    let restarted = project.dispatch(&["start"]);
    assert!(restarted.status.success(), "{}", text(&restarted));
    assert!(text(&restarted).contains("✓ Watching"));
    assert_ne!(project.owner_pid(), pid);
}

#[test]
fn stop_never_signals_a_process_the_record_does_not_identify() {
    let project = Project::new();
    let mut sleeper = Command::new("sleep").arg("60").spawn().unwrap();
    let pid = sleeper.id() as i32;
    fs::create_dir_all(project.record_path().parent().unwrap()).unwrap();
    let record = serde_json::json!({
        "version": 1, "root": project.root, "started_at": "2026-01-01T00:00:00Z",
        "dispatch_version": "0.0.0", "background": true,
        "identity": {"pid": pid, "start": "not this process", "boot": "not this boot",
                     "process_group": pid},
    });
    fs::write(project.record_path(), record.to_string()).unwrap();

    // Lock free: nothing watches, whatever the record says.
    let idle = project.dispatch(&["stop"]);
    assert!(idle.status.success());
    assert!(text(&idle).contains("Not watching"), "{}", text(&idle));
    assert!(alive(pid));

    // Lock held by someone the record does not identify: refused.
    fs::write(project.record_path(), record.to_string()).unwrap();
    fs::create_dir_all(project.lock_path().parent().unwrap()).unwrap();
    let holder = fs::File::create(project.lock_path()).unwrap();
    assert_eq!(unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX) }, 0);
    let refused = project.dispatch(&["stop"]);
    assert!(!refused.status.success());
    assert!(
        text(&refused).contains("nothing was signalled"),
        "{}",
        text(&refused)
    );
    assert!(alive(pid));

    drop(holder);
    let _ = sleeper.kill();
    let _ = sleeper.wait();
}

#[test]
fn start_with_a_broken_configuration_explains_and_spawns_nothing() {
    let project = Project::new();
    fs::write(project.root.join("dispatch.yml"), "coherence: [\n").unwrap();
    let output = project.dispatch(&["start"]);
    assert!(!output.status.success());
    assert!(text(&output).contains("dispatch.yml"), "{}", text(&output));
    assert!(project.record().is_none());
}

impl Project {
    /// A Ready native result from the built-in `fake-good` agent, which adds
    /// `dispatch-fake-good.txt`.
    fn ready_result(&self) -> String {
        let run = self.dispatch(&[
            "run",
            self.root.to_str().unwrap(),
            "--allow-unsafe-local",
            "--agent",
            "fake-good",
            "--task",
            "Create the fake artifact.",
        ]);
        assert!(run.status.success(), "{}", text(&run));
        String::from_utf8_lossy(&run.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("RUN ").map(str::to_owned))
            .unwrap()
    }
}

/// End to end: a Ready native result goes stale while nothing but the
/// background owner runs, and `status` says who watches.
#[test]
fn the_background_owner_keeps_a_ready_result_current() {
    let project = Project::new();
    let run_id = project.ready_result();
    let status = text(&project.dispatch(&["status", &run_id]));
    assert!(status.contains("Project: not watched"), "{status}");
    assert!(project.dispatch(&["start"]).status.success());
    let status = text(&project.dispatch(&["status", &run_id]));
    assert!(
        status.contains("Project: watched in the background"),
        "{status}"
    );
    fs::write(
        project.root.join("dispatch-fake-good.txt"),
        "someone else's\n",
    )
    .unwrap();
    let metadata = project
        .state
        .join("runs")
        .join(&run_id)
        .join("metadata.json");
    wait_until("the owner to record REFRESH", || {
        fs::read(&metadata)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|m| m["coherence"]["validity"]["decision"] == "refresh")
    });
    assert!(project.dispatch(&["stop"]).status.success());
}

#[test]
fn watch_follows_the_project_and_leaving_it_keeps_watching() {
    use std::io::{BufRead, BufReader};

    let project = Project::new();
    let run_id = project.ready_result();
    let mut watch = Command::new(assert_cmd::cargo_bin!("dispatch"))
        .arg("--state-dir")
        .arg(&project.state)
        .arg("watch")
        .current_dir(&project.root)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(watch.stdout.take().unwrap()).lines();
    let mut until = |needle: &str| {
        for line in lines.by_ref() {
            if line.unwrap().contains(needle) {
                return;
            }
        }
        panic!("watch ended before showing {needle:?}");
    };
    until("not watched");
    until(&run_id[..8]);

    // watch holds no lock: the owner starts while it runs, and it notices.
    assert!(project.dispatch(&["start"]).status.success());
    until("watched in the background");
    fs::write(
        project.root.join("dispatch-fake-good.txt"),
        "someone else's\n",
    )
    .unwrap();
    until("REFRESH");

    unsafe { libc::kill(watch.id() as i32, libc::SIGINT) };
    assert!(watch.wait().unwrap().success());
    assert!(alive(project.owner_pid()));
    assert!(project.dispatch(&["stop"]).status.success());
}

#[test]
fn the_watcher_directory_and_log_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let project = Project::new();
    // As an earlier version could have left them.
    let log = project.record_path().with_extension("log");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    fs::set_permissions(log.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(&log, "old\n").unwrap();
    fs::set_permissions(&log, fs::Permissions::from_mode(0o644)).unwrap();

    assert!(project.dispatch(&["start"]).status.success());
    let mode = |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(log.parent().unwrap()), 0o700);
    assert_eq!(mode(&log), 0o600);
    assert!(project.dispatch(&["stop"]).status.success());
}

#[test]
fn a_lock_that_cannot_be_probed_is_an_error_not_a_watcher() {
    let project = Project::new();
    fs::create_dir_all(project.lock_path().parent().unwrap()).unwrap();
    let target = project.state.join("elsewhere");
    fs::write(&target, "").unwrap();
    std::os::unix::fs::symlink(&target, project.lock_path()).unwrap();

    for command in ["start", "stop"] {
        let output = project.dispatch(&[command]);
        assert!(!output.status.success(), "{command}: {}", text(&output));
        assert!(
            text(&output).contains("refusing operation lock symlink"),
            "{command}: {}",
            text(&output)
        );
    }
    assert!(project.record().is_none());
}
