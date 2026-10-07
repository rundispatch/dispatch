//! Registering Work while ignored build output churns. The fixture is a
//! repository (`root`) with a runtime-style worktree inside it
//! (`.claude/worktrees/w`, as `claude --worktree` places it) holding a large
//! ignored `target/` that a background thread keeps creating and deleting
//! files in, the way cargo does during a build. Attach must succeed in every
//! form, strict mode included, and none of `target/` may enter S0 or Δ.
#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
};

use assert_cmd::cargo_bin_cmd;
use serde_json::Value;

const TARGET_DIRECTORIES: usize = 40;
const FILES_PER_DIRECTORY: usize = 100;

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    worktree: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new(strict: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn f() -> i32 {\n    1\n}\n").unwrap();
        fs::write(root.join(".gitignore"), "/target/\n").unwrap();
        let coherence = if strict {
            "coherence:\n  accept: strict\n"
        } else {
            ""
        };
        fs::write(
            root.join("dispatch.yml"),
            format!("checks:\n  verify: ['true']\n{coherence}"),
        )
        .unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
        git(
            &root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "w",
                ".claude/worktrees/w",
            ],
        );
        let root = fs::canonicalize(root).unwrap();
        let worktree = root.join(".claude/worktrees/w");
        for directory in 0..TARGET_DIRECTORIES {
            let directory = worktree.join(format!("target/debug/deps/d{directory}"));
            fs::create_dir_all(&directory).unwrap();
            for file in 0..FILES_PER_DIRECTORY {
                fs::write(directory.join(format!("f{file}.o")), b"object\n").unwrap();
            }
        }
        Self {
            state: temp.path().join("state"),
            _temp: temp,
            root,
            worktree,
        }
    }

    fn dispatch(&self) -> Command {
        let mut command = Command::new(assert_cmd::cargo_bin!("dispatch"));
        command.arg("--state-dir").arg(&self.state);
        command
    }

    fn run_dir(&self, id: &str) -> PathBuf {
        self.state.join("runs").join(id)
    }

    fn metadata(&self, id: &str) -> Value {
        serde_json::from_slice(&fs::read(self.run_dir(id).join("metadata.json")).unwrap()).unwrap()
    }

    /// No path of S0 (the run's baseline) or Δ names `target/`.
    fn assert_no_build_output(&self, id: &str) {
        let baseline = self.run_dir(id).join("baseline");
        let tracked = git(&baseline, &["ls-files"]);
        assert!(!tracked.contains("target/"), "{tracked}");
        assert!(!baseline.join("target").exists());
        assert!(!baseline.join(".claude").exists());
        let patch = fs::read_to_string(self.run_dir(id).join("delta.patch")).unwrap();
        assert!(patch.contains("+    2"), "{patch}");
        assert!(!patch.contains("target/"), "{patch}");
    }

    /// `attach --workspace <worktree>`, an edit, and `finish`.
    fn attach_workspace(&self) -> String {
        let output = self
            .dispatch()
            .arg("attach")
            .arg("--workspace")
            .arg(&self.worktree)
            .arg("--allow-unsafe-local")
            .output()
            .unwrap();
        let id = attached_id(&output);
        fs::write(
            self.worktree.join("src/lib.rs"),
            "pub fn f() -> i32 {\n    2\n}\n",
        )
        .unwrap();
        cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&self.state)
            .args(["finish", &id])
            .assert()
            .success();
        fs::write(
            self.worktree.join("src/lib.rs"),
            "pub fn f() -> i32 {\n    1\n}\n",
        )
        .unwrap();
        id
    }

    /// `attach -- sh -c '…'` from the checkout itself: Dispatch makes the
    /// workspace from the root's world. The agent also writes build output.
    /// The wrapped form prints no `ATTACHED` line, so its run is the one in
    /// `runs/` other than `foreign`.
    fn attach_wrapped(&self, foreign: &str) -> String {
        let output = self
            .dispatch()
            .current_dir(&self.root)
            .args(["attach", "--allow-unsafe-local", "--", "sh", "-c"])
            .arg(
                "printf 'pub fn f() -> i32 {\\n    2\\n}\\n' > src/lib.rs \
                 && mkdir -p target && echo object > target/built.o",
            )
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_success(&output);
        let runs: Vec<String> = fs::read_dir(self.state.join("runs"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|id| id != foreign)
            .collect();
        assert_eq!(runs.len(), 1, "{runs:?}");
        runs.into_iter().next().unwrap()
    }
}

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn attached_id(output: &std::process::Output) -> String {
    assert_success(output);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("ATTACHED "))
        .expect("attach prints ATTACHED <id>")
        .to_owned()
}

/// Keeps rewriting, deleting and recreating files and directories under
/// `target/` until dropped, as cargo does while it builds.
struct Churn {
    stop: Arc<AtomicBool>,
    rounds: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl Churn {
    fn start(target: &Path) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let rounds = Arc::new(AtomicU64::new(0));
        let target = target.to_owned();
        let thread = thread::spawn({
            let stop = Arc::clone(&stop);
            let rounds = Arc::clone(&rounds);
            move || {
                while !stop.load(Ordering::Relaxed) {
                    for directory in 0..TARGET_DIRECTORIES {
                        let deps = target.join(format!("debug/deps/d{directory}"));
                        for file in 0..FILES_PER_DIRECTORY {
                            let path = deps.join(format!("f{file}.o"));
                            let _ = fs::remove_file(&path);
                            let temporary = deps.join(format!("f{file}.o.tmp"));
                            let _ = fs::write(&temporary, b"object\n");
                            let _ = fs::rename(&temporary, &path);
                        }
                        let incremental = target.join(format!("debug/incremental/s{directory}"));
                        let _ = fs::create_dir_all(&incremental);
                        let _ = fs::write(incremental.join("query-cache.bin"), b"cache\n");
                        let _ = fs::remove_dir_all(&incremental);
                    }
                    rounds.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        let churn = Self {
            stop,
            rounds,
            thread: Some(thread),
        };
        churn.wait_for_rounds(1);
        churn
    }

    fn wait_for_rounds(&self, rounds: u64) {
        let start = self.rounds.load(Ordering::Relaxed);
        while self.rounds.load(Ordering::Relaxed) < start + rounds {
            thread::yield_now();
        }
    }
}

impl Drop for Churn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn attach_while_target_churns(strict: bool) {
    let fixture = Fixture::new(strict);
    let churn = Churn::start(&fixture.worktree.join("target"));

    let foreign = fixture.attach_workspace();
    let wrapped = fixture.attach_wrapped(&foreign);
    // The churn really ran alongside both attaches.
    churn.wait_for_rounds(1);
    drop(churn);

    for id in [&foreign, &wrapped] {
        fixture.assert_no_build_output(id);
        let fingerprint = fixture.metadata(id)["source_fingerprint"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            fingerprint.len() == 64,
            strict,
            "strict mode fingerprints the root, and only strict mode: {fingerprint}"
        );
    }
}

#[test]
fn attach_succeeds_while_ignored_build_output_churns() {
    attach_while_target_churns(false);
}

#[test]
fn strict_attach_succeeds_while_ignored_build_output_churns() {
    attach_while_target_churns(true);
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}
