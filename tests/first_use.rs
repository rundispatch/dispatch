//! First use without a terminal ceremony: `dispatch start` in a project
//! without checks watches at once, and `dispatch setup --checks --verify`
//! saves exact commands without a terminal. Neither grants consent, runs
//! project code, or changes the project beyond `dispatch.yml`.
#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};

use dispatch::{
    consent::{self, CheckConsent},
    state::State,
};

struct Project {
    temp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
}

impl Project {
    /// A plain directory with nothing of Dispatch's: no `dispatch.yml`, no
    /// hooks, no profile. It has a check Dispatch would detect, which leaves
    /// a mark if anything runs it.
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(temp.path().join("home")).unwrap();
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("verify.sh"),
            format!("touch {}\n", temp.path().join("ran").display()),
        )
        .unwrap();
        Self {
            root: fs::canonicalize(&root).unwrap(),
            state: temp.path().join("state"),
            temp,
        }
    }

    /// Run without a terminal: stdin is closed and output is captured.
    fn dispatch(&self, args: &[&str]) -> Output {
        let home = self.temp.path().join("home");
        Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&self.state)
            .args(args)
            .current_dir(&self.root)
            .env("HOME", &home)
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .output()
            .unwrap()
    }

    fn consents(&self) -> usize {
        fs::read_dir(self.state.join("projects")).map_or(0, |entries| entries.count())
    }

    fn assert_nothing_ran(&self) {
        assert!(!self.temp.path().join("ran").exists(), "project code ran");
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = self.dispatch(&["stop"]);
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn files(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Starts without a terminal and checks it is watching, quietly, and said
/// once that there are no checks.
fn starts_at_once(project: &Project) {
    let before = files(&project.root);
    let started = Instant::now();
    let output = project.dispatch(&["start"]);
    let said = text(&output);
    assert!(output.status.success(), "{said}");
    assert!(started.elapsed() < Duration::from_secs(10), "{said}");
    assert!(said.contains("✓ Watching"), "{said}");
    assert_eq!(
        said.matches("No checks are configured").count(),
        1,
        "{said}"
    );
    assert!(said.contains("dispatch setup --checks --verify"), "{said}");
    // Nothing is added to the project: no config, no Git, no .gitignore.
    assert_eq!(files(&project.root), before);
    assert_eq!(project.consents(), 0);
    project.assert_nothing_ran();
}

#[test]
fn a_first_start_without_a_terminal_watches_at_once_and_grants_nothing() {
    let project = Project::new();
    starts_at_once(&project);
    let stopped = text(&project.dispatch(&["stop"]));
    assert!(stopped.contains("stopped watching"), "{stopped}");
}

#[test]
fn an_unborn_repository_still_starts_without_a_commit() {
    let project = Project::new();
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&project.root)
            .output()
            .unwrap()
    };
    assert!(git(&["init", "-q"]).status.success());
    starts_at_once(&project);
    // Still unborn: Dispatch made no commit.
    assert!(
        !git(&["rev-parse", "--verify", "-q", "HEAD"])
            .status
            .success()
    );
}

#[test]
fn setup_without_a_terminal_saves_exact_checks_and_keeps_the_rest() {
    let project = Project::new();
    let config = project.root.join("dispatch.yml");
    fs::write(
        &config,
        "execution:\n  timeout_secs: 123\nchecks:\n  verify: ['old']\n",
    )
    .unwrap();
    let output = project.dispatch(&["setup", "--checks", "--verify", "a --x", "--verify", "b"]);
    let said = text(&output);
    assert!(output.status.success(), "{said}");
    assert!(
        said.contains("dispatch setup --checks in a terminal"),
        "{said}"
    );
    assert!(said.contains("--allow-unsafe-local"), "{said}");
    let saved: serde_yaml::Value = serde_yaml::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert_eq!(
        saved["checks"]["verify"],
        serde_yaml::to_value(["a --x", "b"]).unwrap()
    );
    assert_eq!(saved["execution"]["timeout_secs"], 123);
    assert_eq!(project.consents(), 0);
    project.assert_nothing_ran();

    // Without --verify there is nothing to save, and no terminal to ask.
    let output = project.dispatch(&["setup", "--checks"]);
    assert!(!output.status.success());
    assert!(text(&output).contains("--verify"), "{}", text(&output));
}

#[test]
fn saving_other_checks_this_way_ends_consent_given_for_the_old_ones() {
    let project = Project::new();
    fs::write(
        project.root.join("dispatch.yml"),
        "checks:\n  verify: ['a']\n",
    )
    .unwrap();
    let state = State::discover(Some(project.state.clone())).unwrap();
    consent::grant(&state, &project.root).unwrap();

    let output = project.dispatch(&["setup", "--checks", "--verify", "a", "--verify", "b"]);
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(
        consent::consent(&state, &project.root).unwrap(),
        CheckConsent::Changed {
            approved: vec!["a".into()],
            now: vec!["a".into(), "b".into()],
        }
    );
}

#[test]
fn first_use_on_a_terminal() {
    let output = Command::new("python3")
        .arg(format!(
            "{}/tests/fixtures/first_use.py",
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
