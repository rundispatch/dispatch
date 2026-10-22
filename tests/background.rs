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

impl Project {
    /// The record, changed by `edit`, written back over the owner's.
    fn edit_record(&self, edit: impl FnOnce(&mut serde_json::Map<String, Value>)) {
        let mut record = self.record().expect("a watcher record");
        edit(record.as_object_mut().unwrap());
        fs::write(self.record_path(), record.to_string()).unwrap();
    }

    fn started(&self) -> i32 {
        let output = self.dispatch(&["start"]);
        assert!(output.status.success(), "{}", text(&output));
        self.owner_pid()
    }
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// An owner killed without `dispatch stop` leaves its record and frees the
/// lock: the next `status` there starts it again, and says so on stderr only.
#[test]
fn status_brings_back_an_owner_that_was_killed() {
    let project = Project::new();
    let pid = project.started();
    let record = project.record().unwrap();
    assert_eq!(record["version"], 2);
    let exe = fs::canonicalize(assert_cmd::cargo_bin!("dispatch")).unwrap();
    assert_eq!(record["exe"], exe.to_str().unwrap());
    assert_eq!(
        record["environment"]["PATH"],
        std::env::var("PATH").unwrap().as_str()
    );
    unsafe { libc::kill(pid, libc::SIGKILL) };
    wait_until("the lock to be released", || !project.lock_is_held());

    let status = project.dispatch(&["status"]);
    assert!(status.status.success(), "{}", text(&status));
    let stderr = String::from_utf8_lossy(&status.stderr);
    assert_eq!(
        stderr.trim(),
        format!(
            "Watching {} again: its owner had stopped.",
            project.root.display()
        )
    );
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("Project: watched in the background"),
        "{stdout}"
    );
    assert!(project.lock_is_held());
    assert_ne!(project.owner_pid(), pid);
    assert!(alive(project.owner_pid()));

    // Watching again, nothing more is said.
    let again = project.dispatch(&["status", "--json"]);
    assert!(again.status.success(), "{}", text(&again));
    assert!(again.stderr.is_empty(), "{}", text(&again));

    // `dispatch stop` removes the record: nothing comes back.
    assert!(project.dispatch(&["stop"]).status.success());
    let stopped = project.dispatch(&["status"]);
    assert!(stopped.stderr.is_empty(), "{}", text(&stopped));
    assert!(!project.lock_is_held());
}

/// A dead owner is not brought back while the project's configuration does
/// not read: `status` says why in one line and spawns nothing (its log, which
/// a spawned owner writes to, is untouched), and the record stays, so it
/// comes back once the configuration is fixed.
#[test]
fn a_broken_configuration_brings_back_no_owner() {
    let project = Project::new();
    let pid = project.started();
    unsafe { libc::kill(pid, libc::SIGKILL) };
    wait_until("the lock to be released", || !project.lock_is_held());
    fs::write(project.root.join("dispatch.yml"), "coherence: [\n").unwrap();
    let log = project
        .state
        .join("watchers")
        .join(format!("{}.log", project.key()));
    let before = fs::read(&log).unwrap();

    for _ in 0..2 {
        let status = project.dispatch(&["status"]);
        assert!(status.status.success(), "{}", text(&status));
        let stderr = String::from_utf8_lossy(&status.stderr);
        assert_eq!(stderr.lines().count(), 1, "{stderr}");
        assert!(
            stderr.starts_with(&format!(
                "Could not watch {} again: ",
                project.root.display()
            )),
            "{stderr}"
        );
        assert!(stderr.contains("dispatch.yml"), "{stderr}");
        assert!(!project.lock_is_held());
        assert_eq!(project.owner_pid(), pid, "the record stays");
        assert_eq!(fs::read(&log).unwrap(), before, "nothing was spawned");
    }

    fs::write(
        project.root.join("dispatch.yml"),
        "coherence:\n  poll_secs: 1\n",
    )
    .unwrap();
    let status = project.dispatch(&["status"]);
    assert!(
        String::from_utf8_lossy(&status.stderr).contains("again: its owner had stopped."),
        "{}",
        text(&status)
    );
    assert!(project.lock_is_held());
    assert!(project.dispatch(&["stop"]).status.success());
}

/// An owner on another version is replaced only by the binary that started
/// it (upgraded in place) or, when its record does not name one, by a newer
/// Dispatch.
#[test]
fn an_owner_on_another_version_is_replaced_only_from_its_own_executable() {
    let project = Project::new();
    let restarted = |old: &str| {
        let pid = project.owner_pid();
        let status = project.dispatch(&["status"]);
        assert!(status.status.success(), "{}", text(&status));
        assert_eq!(
            String::from_utf8_lossy(&status.stderr).trim(),
            format!(
                "Restarted watching {} with Dispatch {VERSION} (was {old}).",
                project.root.display()
            )
        );
        wait_until("the old owner to exit", || !alive(pid));
        let record = project.record().unwrap();
        assert_eq!(record["dispatch_version"], VERSION);
        assert_eq!(record["version"], 2);
        assert_ne!(project.owner_pid(), pid);
        assert!(project.lock_is_held());
    };
    let left_alone = |old: &str| {
        let pid = project.owner_pid();
        let status = project.dispatch(&["status"]);
        assert!(status.status.success(), "{}", text(&status));
        assert!(status.stderr.is_empty(), "{}", text(&status));
        let stdout = String::from_utf8_lossy(&status.stdout);
        assert!(
            stdout.contains(&format!("it runs Dispatch {old}, this is {VERSION}")),
            "{stdout}"
        );
        assert_eq!(project.owner_pid(), pid);
        assert!(alive(pid));
    };

    // Upgraded in place: this executable started it. The new owner keeps
    // the environment the old one recorded.
    project.started();
    let recorded = format!("/nonexistent/recorded:{}", std::env::var("PATH").unwrap());
    project.edit_record(|record| {
        record.insert("dispatch_version".into(), "0.0.1".into());
        record["environment"]["PATH"] = recorded.as_str().into();
    });
    restarted("0.0.1");
    assert_eq!(
        project.record().unwrap()["environment"]["PATH"],
        recorded.as_str()
    );

    // Another install path.
    project.edit_record(|record| {
        record.insert("dispatch_version".into(), "0.0.1".into());
        record.insert("exe".into(), "/elsewhere/bin/dispatch".into());
    });
    left_alone("0.0.1");

    // A version 1 record of a newer Dispatch.
    project.edit_record(|record| {
        record.insert("version".into(), 1.into());
        record.remove("exe");
        record.insert("dispatch_version".into(), "99.0.0".into());
    });
    left_alone("99.0.0");

    // A version 1 record of an older Dispatch: it keeps no environment, so
    // the new owner runs with this one's.
    project.edit_record(|record| {
        record.insert("dispatch_version".into(), "0.0.1".into());
        record.remove("environment");
    });
    restarted("0.0.1");
    assert_eq!(
        project.record().unwrap()["environment"]["PATH"],
        std::env::var("PATH").unwrap().as_str()
    );

    // `start` replaces the version half of "Already watching".
    project.edit_record(|record| {
        record.insert("dispatch_version".into(), "0.0.2".into());
    });
    let pid = project.owner_pid();
    let start = project.dispatch(&["start"]);
    assert!(start.status.success(), "{}", text(&start));
    assert!(
        String::from_utf8_lossy(&start.stderr).contains("(was 0.0.2)"),
        "{}",
        text(&start)
    );
    let stdout = String::from_utf8_lossy(&start.stdout);
    assert!(stdout.contains("Already watching"), "{stdout}");
    assert!(
        !stdout.contains("dispatch stop && dispatch start"),
        "{stdout}"
    );
    wait_until("the old owner to exit", || !alive(pid));
    assert!(project.dispatch(&["stop"]).status.success());
}

/// The owner notices its project, or the state directory it records in, is
/// gone, and exits within a few ticks instead of running on for days.
#[test]
fn an_owner_exits_when_its_project_is_deleted() {
    let project = Project::new();
    let pid = project.started();
    fs::remove_dir_all(&project.root).unwrap();
    wait_until("the owner to exit", || !alive(pid));
    assert!(!project.lock_is_held());
    assert!(project.record().is_none(), "the owner removed its record");
    let log = fs::read_to_string(project.record_path().with_extension("log")).unwrap();
    assert!(log.contains("no longer exists"), "{log}");
}

#[test]
fn an_owner_exits_when_its_state_directory_is_deleted() {
    let project = Project::new();
    let pid = project.started();
    // The owner may be writing there while it goes.
    wait_until("the state directory to be removed", || {
        fs::remove_dir_all(&project.state).is_ok()
    });
    wait_until("the owner to exit", || !alive(pid));
}

/// `status` where no Work was ever made: the project line, no error.
#[test]
fn status_without_work_shows_the_project() {
    let project = Project::new();
    let status = project.dispatch(&["status"]);
    assert!(status.status.success(), "{}", text(&status));
    assert_eq!(
        String::from_utf8_lossy(&status.stdout),
        format!(
            "No Work in {} yet.\n\nProject: not watched · dispatch start\n",
            project.root.display()
        )
    );
    let json = project.dispatch(&["status", "--json"]);
    assert!(json.status.success(), "{}", text(&json));
    assert_eq!(String::from_utf8_lossy(&json.stdout), "null\n");
    let jsonl = project.dispatch(&["status", "--jsonl"]);
    assert!(jsonl.status.success(), "{}", text(&jsonl));
    assert!(jsonl.stdout.is_empty(), "{}", text(&jsonl));

    project.started();
    let watched = text(&project.dispatch(&["status"]));
    assert!(
        watched.contains("Project: watched in the background"),
        "{watched}"
    );
    assert!(project.dispatch(&["stop"]).status.success());
}
