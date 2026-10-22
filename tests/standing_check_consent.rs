//! A person's standing check consent authorizes exactly the project checks
//! for typed Work, including the later finish, and the Work carries that
//! authority to accept, where the merged checks run.
#![cfg(unix)]

use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Output, Stdio},
};

use dispatch::{
    CheckStatus,
    consent::{self, CheckConsent},
    orchestrator::attach::{self, AttachRequest},
    state::State,
};

struct Project {
    _temp: tempfile::TempDir,
    state: State,
    root: PathBuf,
    worktree: PathBuf,
}

/// A check that says what it looked at, and fails only on a tree holding
/// `forbidden.txt`.
const CHECK: &str = "sh check.sh";

impl Project {
    fn new() -> Self {
        Self::with("true", "worktree")
    }

    /// A project whose checks are `verify`, with a worktree at `worktree`
    /// (relative to the temporary directory).
    fn with(verify: &str, worktree: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("file.txt"), "before\n").unwrap();
        fs::write(
            root.join("check.sh"),
            "echo looked for forbidden.txt\ntest ! -f forbidden.txt\n",
        )
        .unwrap();
        fs::write(root.join(".gitignore"), ".claude/\n").unwrap();
        fs::write(
            root.join("dispatch.yml"),
            format!("checks:\n  verify: ['{verify}']\n"),
        )
        .unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
        let worktree = temp.path().join(worktree);
        git(
            &root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "work",
                worktree.to_str().unwrap(),
            ],
        );
        let state = State::discover(Some(temp.path().join("state"))).unwrap();
        Self {
            _temp: temp,
            state,
            root: fs::canonicalize(root).unwrap(),
            worktree: fs::canonicalize(worktree).unwrap(),
        }
    }

    fn request(&self) -> AttachRequest {
        AttachRequest {
            workspace: self.worktree.clone(),
            root: Some(self.root.clone()),
            task: None,
            agent: None,
            pid: None,
            command: None,
            allow_unsafe_local: false,
            auto_apply: false,
            runtime: None,
        }
    }

    fn checks(&self, command: &str) {
        fs::write(
            self.root.join("dispatch.yml"),
            format!("checks:\n  verify: ['{command}']\n"),
        )
        .unwrap();
    }

    fn dispatch(&self, args: &[&str]) -> Output {
        assert_cmd::cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&self.state.root)
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap()
    }

    fn run(&self, id: &str) -> serde_json::Value {
        let path = self.state.run_dir(id).join("metadata.json");
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    /// Who authorized the Work's host commands, event by event, from the
    /// lines written in full: the owner may be appending one as this reads.
    fn authorized(&self, id: &str) -> Vec<String> {
        fs::read_to_string(self.state.run_dir(id).join("events.jsonl"))
            .unwrap()
            .split_inclusive('\n')
            .filter_map(|line| line.strip_suffix('\n'))
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|event| event["event_type"] == "attach.authorized")
            .map(|event| event["payload"]["by"].as_str().unwrap().to_owned())
            .collect()
    }

    fn consent_path(&self) -> PathBuf {
        fs::read_dir(self.state.root.join("projects"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if self.state.root.join("watchers").exists() {
            let _ = self.dispatch(&["stop"]);
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

fn git(root: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn version_one_needs_a_new_grant_for_the_wider_scope() {
    let p = Project::new();
    consent::grant(&p.state, &p.root).unwrap();
    let path = p.consent_path();
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(record["version"], 2);
    record["version"] = 1.into();
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();

    let old = consent::consent(&p.state, &p.root).unwrap();
    assert!(!old.is_valid());
    assert!(old.describe().unwrap().contains("session end"));
    consent::grant(&p.state, &p.root).unwrap();
    assert!(consent::consent(&p.state, &p.root).unwrap().is_valid());
    let record: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(record["version"], 2);
}

#[tokio::test]
async fn typed_attach_and_finish_require_consent_for_the_exact_checks() {
    let p = Project::new();
    assert!(attach::create(&p.state, p.request()).is_err());

    consent::grant(&p.state, &p.root).unwrap();
    p.checks("false");
    assert_eq!(
        consent::consent(&p.state, &p.root).unwrap(),
        CheckConsent::Changed {
            approved: vec!["true".into()],
            now: vec!["false".into()],
        }
    );
    assert!(attach::create(&p.state, p.request()).is_err());

    p.checks("true");
    let run = attach::create(&p.state, p.request()).unwrap();
    // The consent it proceeded under is the Work's authority, as the flag's
    // would be, and is recorded as such.
    assert!(run.environment.unsafe_local);
    assert_eq!(p.authorized(&run.id), ["project consent"]);
    fs::write(p.worktree.join("file.txt"), "after\n").unwrap();

    let finished = attach::finish(&p.state, &run.id, false).await.unwrap();
    assert_eq!(finished.candidates[0].checks.len(), 1);
    assert_eq!(finished.candidates[0].checks[0].status, CheckStatus::Passed);
    assert!(finished.environment.unsafe_local);
    assert_eq!(p.authorized(&run.id), ["project consent"]);
}

/// The id `dispatch attach` printed.
fn attached(output: &Output) -> String {
    assert!(output.status.success(), "{}", text(output));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("ATTACHED "))
        .unwrap_or_else(|| panic!("{}", text(output)))
        .to_owned()
}

/// `dispatch accept` after the source moved: the merged checks ran, and what
/// they printed is kept in the log their line names.
fn accept_after_moving(p: &Project, id: &str, moved: &str) -> Output {
    fs::write(p.root.join(moved), "someone else's work\n").unwrap();
    let accepted = p.dispatch(&["accept", id]);
    let said = text(&accepted);
    assert!(!said.contains("Merged checks: not run"), "{said}");
    let line = said
        .lines()
        .find(|line| line.contains(CHECK) && line.contains("logs: "))
        .unwrap_or_else(|| panic!("{said}"));
    let logs = line.rsplit("logs: ").next().unwrap();
    let stdout = fs::read_to_string(logs.replace("{stdout,stderr}", "stdout")).unwrap();
    assert_eq!(stdout, "looked for forbidden.txt\n", "{said}");
    accepted
}

#[test]
fn typed_attach_under_consent_runs_the_merged_checks_at_accept() {
    for (moved, passes) in [("notes.txt", true), ("forbidden.txt", false)] {
        let p = Project::with(CHECK, "worktree");
        consent::grant(&p.state, &p.root).unwrap();
        let id = attached(&p.dispatch(&["attach", "--workspace", p.worktree.to_str().unwrap()]));
        assert_eq!(p.run(&id)["environment"]["unsafe_local"], true);
        assert_eq!(p.authorized(&id), ["project consent"]);
        fs::write(p.worktree.join("file.txt"), "after\n").unwrap();
        let finished = p.dispatch(&["finish", &id]);
        assert!(finished.status.success(), "{}", text(&finished));

        let accepted = accept_after_moving(&p, &id, moved);
        let said = text(&accepted);
        let applied = fs::read_to_string(p.root.join("file.txt")).unwrap();
        if passes {
            assert!(accepted.status.success(), "{said}");
            assert!(
                said.contains("Merged checks: passed on the current source with this patch"),
                "{said}"
            );
            assert_eq!(applied, "after\n", "{said}");
        } else {
            assert!(!accepted.status.success(), "{said}");
            assert!(said.contains("FAIL") && said.contains("exit 1"), "{said}");
            assert_eq!(applied, "before\n", "{said}");
        }
    }
}

#[test]
fn consent_given_after_registration_authorizes_finish_and_the_merged_checks() {
    let p = Project::with(CHECK, "source/.claude/worktrees/x");
    let started = p.dispatch(&["start"]);
    assert!(started.status.success(), "{}", text(&started));
    // Claude Code registers the Work by its hook; nobody has consented yet.
    let start = serde_json::json!({
        "session_id": "96fce6bd-fe23-4b1c-bd1e-b171635c421b",
        "transcript_path": p.root.join("transcript.jsonl"),
        "cwd": p.worktree,
        "hook_event_name": "SessionStart",
        "source": "startup",
        "model": "claude-opus-5-5",
    });
    let mut hook = Command::new(assert_cmd::cargo_bin!("dispatch"))
        .arg("--state-dir")
        .arg(&p.state.root)
        .args(["hook", "claude"])
        .env("DISPATCH_REGISTRATION_BUDGET_MS", "600000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    hook.stdin
        .take()
        .unwrap()
        .write_all(start.to_string().as_bytes())
        .unwrap();
    let reply = hook.wait_with_output().unwrap();
    assert!(
        text(&reply).contains("tracking this worktree"),
        "{}",
        text(&reply)
    );
    let ids: Vec<String> = fs::read_dir(p.state.root.join("runs"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let [id] = &ids[..] else { panic!("{ids:?}") };
    assert_eq!(p.run(id)["environment"]["unsafe_local"], false);
    assert!(p.authorized(id).is_empty());
    fs::write(p.worktree.join("file.txt"), "after\n").unwrap();

    // Without consent, or with consent for other checks, finish is refused
    // as before, and records nothing.
    let refusal = "pass --allow-unsafe-local, or give this project standing consent";
    let finished = p.dispatch(&["finish", id]);
    assert!(!finished.status.success() && text(&finished).contains(refusal));
    p.checks("true");
    consent::grant(&p.state, &p.root).unwrap();
    p.checks(CHECK);
    let finished = p.dispatch(&["finish", id]);
    assert!(!finished.status.success() && text(&finished).contains(refusal));
    assert!(p.authorized(id).is_empty());

    consent::grant(&p.state, &p.root).unwrap();
    let finished = p.dispatch(&["finish", id]);
    assert!(finished.status.success(), "{}", text(&finished));
    assert_eq!(p.run(id)["environment"]["unsafe_local"], true);
    assert_eq!(p.authorized(id), ["project consent"]);

    let accepted = accept_after_moving(&p, id, "notes.txt");
    let said = text(&accepted);
    assert!(accepted.status.success(), "{said}");
    assert!(
        said.contains("Merged checks: passed on the current source with this patch"),
        "{said}"
    );
    assert_eq!(
        fs::read_to_string(p.root.join("file.txt")).unwrap(),
        "after\n"
    );
}

#[test]
fn wrapped_attach_can_finish_and_auto_apply_with_standing_check_consent() {
    let p = Project::new();
    consent::grant(&p.state, &p.root).unwrap();
    let output = assert_cmd::cargo_bin_cmd!("dispatch")
        .arg("--state-dir")
        .arg(&p.state.root)
        .current_dir(&p.root)
        .args([
            "attach",
            "--auto-apply",
            "--",
            "sh",
            "-c",
            "printf 'after\\n' > file.txt",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(p.root.join("file.txt")).unwrap(),
        "after\n",
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
