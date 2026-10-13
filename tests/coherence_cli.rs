//! The coherence commands: `check` (read-only verdict), `refresh` (a new run on
//! the current source), and the coherence lines in `status` and `explain`. Uses
//! the built-in `fake-good` harness, which creates one empty file,
//! `dispatch-fake-good.txt`.
#![cfg(unix)]

use std::{fs, path::PathBuf};

use assert_cmd::cargo_bin_cmd;
use serde_json::Value;

struct Fixture {
    _temp: tempfile::TempDir,
    source: PathBuf,
    state: PathBuf,
    run_id: String,
    /// What `dispatch run` printed.
    run_stdout: String,
}

impl Fixture {
    fn new(extra_config: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let state = temp.path().join("state");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("original.txt"), "baseline\n").unwrap();
        fs::write(
            source.join("dispatch.yml"),
            format!("execution:\n  timeout_secs: 30\n{extra_config}"),
        )
        .unwrap();
        let output = cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&state)
            .arg("run")
            .arg(&source)
            .arg("--allow-unsafe-local")
            .args([
                "--task",
                "Create the fake artifact.",
                "--agent",
                "fake-good",
            ])
            .assert()
            .success()
            .get_output()
            .clone();
        let run_stdout = String::from_utf8(output.stdout).unwrap();
        let run_id = run_stdout
            .lines()
            .find_map(|line| line.strip_prefix("RUN "))
            .expect("run output includes an ID")
            .to_owned();
        let source = fs::canonicalize(source).unwrap();
        Self {
            _temp: temp,
            source,
            state,
            run_id,
            run_stdout,
        }
    }

    fn dispatch(&self, args: &[&str]) -> assert_cmd::assert::Assert {
        cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&self.state)
            .current_dir(&self.source)
            .args(args)
            .assert()
    }

    fn run_dir(&self, id: &str) -> PathBuf {
        self.state.join("runs").join(id)
    }

    fn metadata(&self, id: &str) -> Value {
        serde_json::from_slice(&fs::read(self.run_dir(id).join("metadata.json")).unwrap()).unwrap()
    }

    /// Every file of the run directory (relative name, bytes), for exact comparison.
    fn snapshot(&self, id: &str) -> Vec<(PathBuf, Vec<u8>)> {
        fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            let mut entries = fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            entries.sort();
            for path in entries {
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    out.push((
                        path.strip_prefix(root).unwrap().to_owned(),
                        fs::read(&path).unwrap(),
                    ));
                }
            }
        }
        let root = self.run_dir(id);
        let mut out = Vec::new();
        walk(&root, &root, &mut out);
        out
    }

    fn run_count(&self) -> usize {
        fs::read_dir(self.state.join("runs")).unwrap().count()
    }

    fn stdout(assert: &assert_cmd::assert::Assert) -> String {
        String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
    }

    fn stderr(assert: &assert_cmd::assert::Assert) -> String {
        String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
    }

    fn unrelated_edit(&self) {
        fs::write(self.source.join("notes.txt"), "someone else's work\n").unwrap();
    }

    fn conflicting_edit(&self) {
        fs::write(
            self.source.join("dispatch-fake-good.txt"),
            "someone else wrote this first\n",
        )
        .unwrap();
    }
}

#[test]
fn check_continues_for_an_unrelated_edit_and_writes_nothing() {
    let fixture = Fixture::new("");
    fixture.unrelated_edit();
    let before = fixture.snapshot(&fixture.run_id);
    let db_events_before = Fixture::stdout(&fixture.dispatch(&["status", "--jsonl"]).success());

    let assert = fixture.dispatch(&["check"]).success();

    let stdout = Fixture::stdout(&assert);
    assert!(stdout.contains("Coherence: CONTINUE"), "{stdout}");
    assert!(stdout.contains("World changed files: 1"), "{stdout}");
    assert!(
        stdout.contains(&format!("Next: dispatch accept {}", fixture.run_id)),
        "{stdout}"
    );
    assert_eq!(fixture.snapshot(&fixture.run_id), before);
    assert_eq!(
        Fixture::stdout(&fixture.dispatch(&["status", "--jsonl"]).success()),
        db_events_before,
        "check must not add events"
    );
    assert!(!fixture.source.join("dispatch-fake-good.txt").exists());
}

#[test]
fn check_refreshes_for_a_conflicting_edit_and_names_the_next_commands() {
    let fixture = Fixture::new("");
    fixture.conflicting_edit();
    let before = fixture.snapshot(&fixture.run_id);

    let assert = fixture.dispatch(&["check", &fixture.run_id]).success();

    let stdout = Fixture::stdout(&assert);
    let id = &fixture.run_id;
    assert!(stdout.contains("Coherence: REFRESH"), "{stdout}");
    assert!(stdout.contains("  patch_conflict: "), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "Next: dispatch refresh {id}  (or dispatch reject {id})"
        )),
        "{stdout}"
    );
    assert_eq!(fixture.snapshot(id), before);
}

/// The native result summary says what holds now, by Work ID: the frozen
/// patch, its verification, a fresh verdict, what accept would verify, and
/// the commands that fit.
#[test]
fn the_result_summary_shows_the_patch_verdict_and_what_accept_would_verify() {
    let fixture = Fixture::new("");
    let (id, out) = (&fixture.run_id, &fixture.run_stdout);
    assert!(out.contains("Ready for review"), "{out}");
    assert!(out.contains(&format!("Work {id}\n")), "{out}");
    assert!(
        out.contains("  patch: ") && out.contains("diff.patch (1 file)"),
        "{out}"
    );
    assert!(out.contains("Verification\n  Not configured"), "{out}");
    assert!(
        out.contains("Coherence: CONTINUE — world unchanged"),
        "{out}"
    );
    assert!(
        out.contains(
            "At accept\n  merged checks would not run now — the source has not moved since the Work began"
        ),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "Then\n  dispatch accept {id}\n  dispatch reject {id}"
        )),
        "{out}"
    );
}

/// A result that no longer applies: `status` and `check` agree on the verdict
/// and on what accept would verify, offer refresh rather than accept, name
/// the file, and write nothing.
#[test]
fn status_and_check_agree_on_a_result_that_no_longer_applies_and_write_nothing() {
    let fixture = Fixture::new("");
    let id = &fixture.run_id;
    fixture.conflicting_edit();
    let before = fixture.snapshot(id);
    let status = Fixture::stdout(&fixture.dispatch(&["status", id]).success());
    let check = Fixture::stdout(&fixture.dispatch(&["check", id]).success());
    assert_eq!(fixture.snapshot(id), before);

    let conflict = "patch_conflict: the patch no longer applies to dispatch-fake-good.txt";
    let scope = "accept would refuse it now (REFRESH), before any merged checks";
    assert!(status.contains("Finished, but stale (REFRESH)"), "{status}");
    assert!(status.contains(conflict), "{status}");
    assert!(
        status.contains(&format!("At accept\n  {scope}")),
        "{status}"
    );
    assert!(
        status.contains(&format!(
            "Then\n  dispatch refresh {id}\n  dispatch reject {id}"
        )),
        "{status}"
    );
    assert!(!status.contains("dispatch accept"), "{status}");
    assert!(check.contains(conflict), "{check}");
    assert!(check.contains(&format!("At accept: {scope}")), "{check}");
}

#[test]
fn check_stops_when_the_patch_is_already_in_the_source() {
    let fixture = Fixture::new("");
    fs::write(fixture.source.join("dispatch-fake-good.txt"), "").unwrap();

    let stdout = Fixture::stdout(&fixture.dispatch(&["check"]).success());

    assert!(stdout.contains("Coherence: STOP"), "{stdout}");
    assert!(stdout.contains("already_applied: "), "{stdout}");
    // Landed changes are closed, not rejected: no human judgment of them.
    assert!(
        stdout.contains(&format!(
            "Next: dispatch close {} --reason already-present",
            fixture.run_id
        )),
        "{stdout}"
    );
}

/// A closed result reads as closed, never as waiting for review.
#[test]
fn a_closed_result_is_headed_closed() {
    let fixture = Fixture::new("");
    let id = &fixture.run_id;
    fs::write(fixture.source.join("dispatch-fake-good.txt"), "").unwrap();
    fixture
        .dispatch(&["close", id, "--reason", "already-present"])
        .success();

    let status = Fixture::stdout(&fixture.dispatch(&["status", id]).success());

    assert!(status.starts_with("\nClosed\n"), "{status}");
    assert!(!status.contains("Ready for review"), "{status}");
    assert!(
        status.contains("Closed: changes already present; source tree unchanged"),
        "{status}"
    );
}

#[test]
fn check_json_prints_the_validity_and_run_id() {
    let fixture = Fixture::new("");
    fixture.conflicting_edit();

    let assert = fixture.dispatch(&["check", "--json"]).success();

    let value: Value = serde_json::from_str(Fixture::stdout(&assert).trim()).unwrap();
    assert_eq!(value["run_id"], fixture.run_id.as_str());
    let validity = &value["validity"];
    assert_eq!(validity["decision"], "refresh");
    assert_eq!(validity["world_changed"], true);
    assert_eq!(validity["changed_files"], 1);
    assert_eq!(validity["reasons"][0]["code"], "patch_conflict");
    assert!(validity["analysis"].is_string() && validity["evaluated_at"].is_string());
}

#[test]
fn check_fails_cleanly_when_the_run_is_not_ready() {
    let fixture = Fixture::new("");
    fixture.dispatch(&["accept", &fixture.run_id]).success();

    let assert = fixture.dispatch(&["check", &fixture.run_id]).failure();

    assert!(
        Fixture::stderr(&assert).contains("not a ready, unapplied result"),
        "{}",
        Fixture::stderr(&assert)
    );
}

#[test]
fn status_json_carries_coherence_only_when_the_world_moved() {
    let fixture = Fixture::new("");
    let quiet = fixture.dispatch(&["status", "--json"]).success();
    let quiet: Value = serde_json::from_str(Fixture::stdout(&quiet).trim()).unwrap();
    assert!(quiet.get("coherence").is_none(), "{quiet}");
    let text = Fixture::stdout(&fixture.dispatch(&["status"]).success());
    assert!(
        text.contains("Coherence: CONTINUE — world unchanged"),
        "{text}"
    );

    fixture.conflicting_edit();
    let moved = fixture
        .dispatch(&["status", &fixture.run_id, "--json"])
        .success();
    let moved: Value = serde_json::from_str(Fixture::stdout(&moved).trim()).unwrap();
    assert_eq!(moved["coherence"]["decision"], "refresh");
    assert_eq!(moved["coherence"]["changed_files"], 1);
    assert_eq!(moved["coherence"]["reasons"][0]["code"], "patch_conflict");
    let text = Fixture::stdout(&fixture.dispatch(&["status"]).success());
    assert!(text.contains("Coherence: REFRESH — "), "{text}");

    // The verdict is display-only: nothing was stored by looking at it.
    assert!(fixture.metadata(&fixture.run_id)["coherence"].is_null());
}

#[test]
fn explain_shows_the_coherence_section_only_when_there_is_data() {
    let fixture = Fixture::new("");
    let quiet = fixture.dispatch(&["explain"]).success();
    assert!(!Fixture::stdout(&quiet).contains("Coherence"));

    fixture.conflicting_edit();
    // A blocked accept stores the verdict and when the work first went stale.
    let blocked = fixture.dispatch(&["accept", &fixture.run_id]).failure();
    assert!(Fixture::stderr(&blocked).contains("stale"));

    let assert = fixture.dispatch(&["explain"]).success();
    let stdout = Fixture::stdout(&assert);
    assert!(stdout.contains("\nCoherence\n"), "{stdout}");
    assert!(stdout.contains("  decision: REFRESH"), "{stdout}");
    assert!(stdout.contains("  analysis: "), "{stdout}");
    assert!(
        stdout.contains("  world changed: yes (1 file(s))"),
        "{stdout}"
    );
    assert!(stdout.contains("  reason: patch_conflict: "), "{stdout}");
    assert!(stdout.contains("  first invalid at: "), "{stdout}");
    assert!(!stdout.contains("refreshed from"), "{stdout}");
    if let Some(line) = stdout
        .lines()
        .find(|l| l.contains("Agent time after the work became invalid"))
    {
        assert!(
            line.contains(" of ") && line.trim_end().ends_with("%)"),
            "{line}"
        );
    }
}

#[test]
fn blocked_accept_names_refresh_and_reject() {
    let fixture = Fixture::new("");
    fixture.conflicting_edit();

    let assert = fixture.dispatch(&["accept", &fixture.run_id]).failure();

    let id = &fixture.run_id;
    let stderr = Fixture::stderr(&assert);
    assert!(stderr.contains("this work is stale"), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "Run 'dispatch refresh {id}' to redo the work on the current source, or 'dispatch reject {id}'."
        )),
        "{stderr}"
    );
}

fn refreshed_id(stdout: &str, old: &str) -> String {
    let line = stdout
        .lines()
        .find(|line| line.starts_with("Refreshed from "))
        .unwrap_or_else(|| panic!("no lineage line in {stdout}"));
    let (from, new) = line
        .strip_prefix("Refreshed from ")
        .and_then(|rest| rest.split_once("; new run "))
        .unwrap();
    assert_eq!(from, old);
    new.trim().to_owned()
}

#[test]
fn refresh_starts_a_new_run_with_the_addendum_and_never_touches_the_old_one() {
    let fixture = Fixture::new("");
    fixture.unrelated_edit();
    let old = fixture.run_id.clone();
    let before = fixture.snapshot(&old);

    let assert = fixture.dispatch(&["refresh", &old]).success();

    let new = refreshed_id(&Fixture::stdout(&assert), &old);
    assert_ne!(new, old);
    assert_eq!(fixture.snapshot(&old), before, "old run must not change");
    let metadata = fixture.metadata(&new);
    let task = metadata["task"].as_str().unwrap();
    assert!(task.starts_with("Create the fake artifact.\n\nContext: this task was previously attempted against an earlier version of the repository (run "));
    assert!(task.contains(&format!("(run {old})")));
    assert!(task.contains("\n- "), "{task}");
    assert!(task.ends_with("do not assume the earlier attempt's assumptions still hold."));
    assert_eq!(metadata["coherence"]["refreshed_from"], old.as_str());
    assert_eq!(metadata["candidates"][0]["harness_id"], "fake-good");
    // The new run was made against the current source, so its baseline has the edit.
    assert_eq!(metadata["source_path"], fixture.source.to_str().unwrap());
    assert_eq!(fixture.run_count(), 2);

    let explain = Fixture::stdout(&fixture.dispatch(&["explain", &new]).success());
    assert!(
        explain.contains(&format!("  refreshed from: {old}")),
        "{explain}"
    );
}

#[test]
fn refresh_requires_the_same_acknowledgements_again() {
    let fixture = Fixture::new("checks:\n  verify:\n    - test -f original.txt\n");
    let old = fixture.run_id.clone();
    assert_eq!(
        fixture.metadata(&old)["environment"]["unsafe_local"],
        true,
        "the original run executed project checks on the host"
    );

    let assert = fixture.dispatch(&["refresh", &old]).failure();

    let stderr = Fixture::stderr(&assert);
    assert!(stderr.contains("--allow-unsafe-local"), "{stderr}");
    assert_eq!(fixture.run_count(), 1, "no run may be created");

    let assert = fixture
        .dispatch(&["refresh", &old, "--allow-unsafe-local"])
        .success();
    let new = refreshed_id(&Fixture::stdout(&assert), &old);
    assert_eq!(
        fixture.metadata(&new)["coherence"]["refreshed_from"],
        old.as_str()
    );
    assert_eq!(fixture.run_count(), 2);
}

#[test]
fn refresh_of_a_run_that_is_not_ready_fails_cleanly() {
    let fixture = Fixture::new("");
    fixture.dispatch(&["accept", &fixture.run_id]).success();
    let before = fixture.snapshot(&fixture.run_id);

    let assert = fixture.dispatch(&["refresh", &fixture.run_id]).failure();

    let stderr = Fixture::stderr(&assert);
    assert!(stderr.contains("not a ready, unapplied result"), "{stderr}");
    assert_eq!(fixture.run_count(), 1);
    assert_eq!(fixture.snapshot(&fixture.run_id), before);
}

#[test]
fn refresh_after_a_conflict_carries_the_reason_into_the_task() {
    let fixture = Fixture::new("");
    fixture.conflicting_edit();
    let old = fixture.run_id.clone();

    let assert = fixture.dispatch(&["refresh", &old, "--json"]).success();

    let result: Value = serde_json::from_str(Fixture::stdout(&assert).trim()).unwrap();
    let new = result["run_id"].as_str().unwrap();
    let metadata = fixture.metadata(new);
    let task = metadata["task"].as_str().unwrap();
    assert!(
        task.contains("\n- ") && task.contains("dispatch-fake-good.txt"),
        "{task}"
    );
    assert_eq!(metadata["coherence"]["refreshed_from"], old.as_str());
    assert_eq!(fixture.metadata(&old)["coherence"], Value::Null);
}
