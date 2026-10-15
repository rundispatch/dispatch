//! Trust for the integrator (`docs/plan-0.4.14.md` §3 E): `dispatch check
//! --merged` runs `checks.verify` on the current source with the Work's Δ as
//! of now and changes nothing; the Δ digest ties what was reviewed to what
//! `accept --expect` lands; and every check command is reported on its own
//! line. The fixture is a Git repository (`root`) and a linked worktree
//! (`workspace`) whose Work is attached, playing an agent Dispatch did not
//! launch.
#![cfg(unix)]

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;
use sha2::{Digest, Sha256};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    state: PathBuf,
}

impl Fixture {
    /// `verify` is the root's `checks.verify`.
    fn new(verify: &[&str]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), ONE).unwrap();
        let commands = verify
            .iter()
            .map(|command| format!("    - {command}\n"))
            .collect::<String>();
        fs::write(
            root.join("dispatch.yml"),
            format!("checks:\n  verify:\n{commands}"),
        )
        .unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
        let workspace = temp.path().join("wt");
        git(
            &root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "agent",
                workspace.to_str().unwrap(),
            ],
        );
        Self {
            root: fs::canonicalize(root).unwrap(),
            workspace: fs::canonicalize(workspace).unwrap(),
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

    fn attach(&self) -> String {
        let output = self.dispatch(&[
            "attach",
            "--workspace",
            self.workspace.to_str().unwrap(),
            "--root",
            self.root.to_str().unwrap(),
            "--allow-unsafe-local",
        ]);
        assert!(output.status.success(), "{}", stderr(&output));
        stdout(&output)
            .lines()
            .find_map(|line| line.strip_prefix("ATTACHED "))
            .expect("attach prints ATTACHED <id>")
            .to_owned()
    }

    /// The agent's work: an edit to a tracked file and a new, untracked one.
    fn edit_workspace(&self) {
        fs::write(self.workspace.join("src/lib.rs"), TWO).unwrap();
        fs::write(self.workspace.join("src/new.rs"), "pub fn g() {}\n").unwrap();
    }

    fn finish(&self, id: &str) -> String {
        let output = self.dispatch(&["finish", id]);
        assert!(output.status.success(), "{}", stderr(&output));
        stdout(&output)
    }

    /// Someone else's work lands in the root: a committed, disjoint file.
    fn move_source(&self) {
        fs::write(self.root.join("notes.txt"), "someone else's work\n").unwrap();
        git(&self.root, &["add", "notes.txt"]);
        git(&self.root, &["commit", "--quiet", "-m", "notes"]);
    }

    fn run_dir(&self, id: &str) -> PathBuf {
        self.state.join("runs").join(id)
    }

    fn metadata(&self, id: &str) -> Value {
        serde_json::from_slice(&fs::read(self.run_dir(id).join("metadata.json")).unwrap()).unwrap()
    }

    fn digest(&self, id: &str) -> String {
        hex::encode(Sha256::digest(
            fs::read(self.run_dir(id).join("delta.patch")).unwrap(),
        ))
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.state.join("dispatch.db")).unwrap()
    }

    fn events(&self, id: &str, kind: &str) -> Vec<Value> {
        let db = self.db();
        let mut statement = db
            .prepare(
                "SELECT payload_json FROM events WHERE run_id = ?1 AND event_type = ?2 ORDER BY sequence",
            )
            .unwrap();
        statement
            .query_map([id, kind], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
            .collect()
    }

    /// The run's committed state: revision and projection.
    fn committed(&self, id: &str) -> (i64, String) {
        self.db()
            .query_row(
                "SELECT state_revision, run_projection_json FROM runs WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    }

    /// As `attach_cli`'s helper: finished Work without the acknowledgement,
    /// as Work a runtime registered without consent has it.
    fn clear_unsafe_local(&self, id: &str) {
        let path = self.run_dir(id).join("metadata.json");
        let mut metadata = self.metadata(id);
        metadata["environment"]["unsafe_local"] = Value::Bool(false);
        fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let db = self.db();
        let (_, projection) = self.committed(id);
        let mut projection: Value = serde_json::from_str(&projection).unwrap();
        projection["environment"]["unsafe_local"] = Value::Bool(false);
        db.execute(
            "UPDATE runs SET run_projection_json = ?1, unsafe_local = 0 WHERE id = ?2",
            (serde_json::to_string(&projection).unwrap(), id),
        )
        .unwrap();
    }
}

const ONE: &str = "pub fn f() -> i32 {\n    1\n}\n";
const TWO: &str = "pub fn f() -> i32 {\n    2\n}\n";

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
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Every file under `dir`, `.git` included, with its bytes.
fn tree(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
    }
    files
}

/// The per-command line for `command`, which must say `PASS` or `FAIL`
/// with `exit` and name a log that exists.
fn command_line<'a>(out: &'a str, command: &str, status: &str, exit: i32) -> &'a str {
    let line = out
        .lines()
        .find(|line| line.trim_start().starts_with(status) && line.contains(command))
        .unwrap_or_else(|| panic!("no {status} line for {command}: {out}"));
    assert!(line.contains(&format!("exit {exit} ")), "{line}");
    let logs = line.split_once("logs: ").expect("a log path").1;
    let stem = logs
        .strip_suffix(".{stdout,stderr}.log")
        .unwrap_or_else(|| panic!("{logs}"));
    for log in [format!("{stem}.stdout.log"), format!("{stem}.stderr.log")] {
        assert!(Path::new(&log).is_file(), "{log}");
    }
    line
}

#[test]
fn check_merged_on_finished_work_after_the_source_moved_changes_nothing() {
    let f = Fixture::new(&["test -f src/new.rs", "test -f src/lib.rs"]);
    let id = f.attach();
    f.edit_workspace();
    f.finish(&id);
    f.move_source();
    let head = git(&f.root, &["rev-parse", "HEAD"]);
    let source_before = tree(&f.root);
    let run_before = tree(&f.run_dir(&id));
    let committed_before = f.committed(&id);

    let output = f.dispatch(&["check", &id, "--merged"]);

    let out = stdout(&output);
    assert!(output.status.success(), "{out}{}", stderr(&output));
    assert!(out.contains("Coherence: CONTINUE"), "{out}");
    assert!(
        out.contains(&format!(
            "Merged checks on the current source (Git HEAD {}) with this Δ: passed",
            &head[..12]
        )),
        "{out}"
    );
    command_line(&out, "test -f src/new.rs", "PASS", 0);
    command_line(&out, "test -f src/lib.rs", "PASS", 0);
    assert!(
        out.contains("accept runs its own merged checks, because the source can move after"),
        "{out}"
    );
    let digest = f.digest(&id);
    assert!(
        out.lines()
            .any(|line| line == format!("Δ {}", &digest[..12])),
        "{out}"
    );

    // Nothing applied, nothing about the run changed but its evidence: the
    // logs and the event.
    assert_eq!(tree(&f.root), source_before);
    assert_eq!(f.committed(&id), committed_before);
    let mut run_after = tree(&f.run_dir(&id));
    run_after.retain(|path, _| {
        !path.starts_with(f.run_dir(&id).join("coherence-checks"))
            && !path.ends_with("events.jsonl")
    });
    let mut run_before = run_before;
    run_before.retain(|path, _| !path.ends_with("events.jsonl"));
    assert_eq!(run_after, run_before);
    assert!(!f.root.join("src/new.rs").exists());

    let events = f.events(&id, "coherence.merged_checks");
    assert_eq!(events.len(), 1, "{events:?}");
    let event = &events[0];
    assert_eq!(event["by"], "check");
    assert_eq!(event["delta_digest"], digest);
    assert_eq!(
        event["source"],
        serde_json::json!({"git": {"head": head, "dirty": false}})
    );
    assert_eq!(event["passed"], true);
    let results = event["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    for result in results {
        assert!(result["duration_ms"].is_u64(), "{result}");
        assert_eq!(result["status"], "passed");
        assert_eq!(result["exit_code"], 0);
    }
    assert!(
        fs::read_to_string(f.run_dir(&id).join("events.jsonl"))
            .unwrap()
            .contains("coherence.merged_checks")
    );
}

#[test]
fn check_merged_reports_a_check_failing_only_on_the_merged_tree() {
    let f = Fixture::new(&["test ! -f forbidden.txt"]);
    let id = f.attach();
    f.edit_workspace();
    let finished = f.finish(&id);
    assert!(finished.contains("Verification\n  PASS"), "{finished}");
    fs::write(f.root.join("forbidden.txt"), "added by someone else\n").unwrap();
    let verdict_before = f.metadata(&id)["coherence"].clone();

    let output = f.dispatch(&["check", &id, "--merged"]);

    let out = stdout(&output);
    assert!(!output.status.success(), "{out}");
    assert!(
        stderr(&output).contains(&format!("the merged checks did not pass for {id}")),
        "{}",
        stderr(&output)
    );
    assert!(
        out.contains("with uncommitted changes) with this Δ: did not pass"),
        "{out}"
    );
    command_line(&out, "test ! -f forbidden.txt", "FAIL", 1);
    // The verdict is the file and symbol one, unchanged, and nothing landed.
    assert!(out.contains("Coherence: CONTINUE"), "{out}");
    assert_eq!(f.metadata(&id)["coherence"], verdict_before);
    assert_eq!(f.metadata(&id)["outcome"]["review"], "pending");
    assert_eq!(f.metadata(&id)["outcome"]["application"], "not_applied");
    assert_eq!(fs::read_to_string(f.root.join("src/lib.rs")).unwrap(), ONE);
    assert!(!f.root.join("src/new.rs").exists());
    let events = f.events(&id, "coherence.merged_checks");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["passed"], false);
    assert_eq!(events[0]["results"][0]["exit_code"], 1);
}

#[test]
fn check_merged_on_unfinished_attached_work_includes_new_untracked_files() {
    let f = Fixture::new(&["test -f src/new.rs"]);
    let id = f.attach();
    f.edit_workspace();
    f.move_source();
    let delta = f.run_dir(&id).join("delta.patch");
    let delta_before = fs::read(&delta).ok();
    let committed_before = f.committed(&id);

    let output = f.dispatch(&["check", &id, "--merged"]);

    let out = stdout(&output);
    assert!(output.status.success(), "{out}{}", stderr(&output));
    command_line(&out, "test -f src/new.rs", "PASS", 0);
    assert!(out.contains("Unfinished: its Δ as of now is at "), "{out}");
    assert_eq!(fs::read(&delta).ok(), delta_before);
    assert_eq!(f.committed(&id), committed_before);
    assert_eq!(f.metadata(&id)["outcome"]["lifecycle"], "working");
    // The captured Δ is kept with the logs, and its digest is the one shown.
    let events = f.events(&id, "coherence.merged_checks");
    let captured = PathBuf::from(events[0]["patch"].as_str().unwrap());
    assert!(captured.starts_with(f.run_dir(&id).join("coherence-checks")));
    let patch = fs::read_to_string(&captured).unwrap();
    assert!(patch.contains("b/src/new.rs"), "{patch}");
    let digest = hex::encode(Sha256::digest(patch.as_bytes()));
    assert_eq!(events[0]["delta_digest"], digest);
    assert!(out.contains(&format!("Δ {}", &digest[..12])), "{out}");
}

#[test]
fn check_merged_needs_authority_and_never_widens_it() {
    let f = Fixture::new(&["true"]);
    let id = f.attach();
    f.edit_workspace();
    f.finish(&id);
    f.clear_unsafe_local(&id);

    let refused = f.dispatch(&["check", &id, "--merged"]);

    assert!(!refused.status.success());
    let error = stderr(&refused);
    assert!(error.contains("pass --allow-unsafe-local"), "{error}");
    assert!(error.contains("dispatch setup --checks"), "{error}");
    assert!(f.events(&id, "coherence.merged_checks").is_empty());
    assert!(!f.run_dir(&id).join("coherence-checks").exists());

    let allowed = f.dispatch(&["check", &id, "--merged", "--allow-unsafe-local"]);

    assert!(allowed.status.success(), "{}", stderr(&allowed));
    command_line(&stdout(&allowed), "true", "PASS", 0);
    assert_eq!(f.metadata(&id)["environment"]["unsafe_local"], false);
    assert!(f.events(&id, "attach.authorized").is_empty());
}

#[test]
fn accept_expect_lands_only_the_reviewed_delta_and_reports_each_check() {
    let f = Fixture::new(&["test -f src/new.rs"]);
    let id = f.attach();
    f.edit_workspace();
    f.finish(&id);
    f.move_source();
    let digest = f.digest(&id);
    let source_before = tree(&f.root);

    let refused = f.dispatch(&["accept", &id, "--expect", "0123456789abcdef"]);

    assert!(!refused.status.success());
    let error = stderr(&refused);
    assert!(error.contains(&digest[..16]), "{error}");
    assert!(error.contains("0123456789abcdef"), "{error}");
    assert!(error.contains("nothing was applied"), "{error}");
    assert_eq!(tree(&f.root), source_before);
    assert_eq!(f.metadata(&id)["outcome"]["review"], "pending");
    assert!(f.events(&id, "review.accepted").is_empty());
    assert!(f.events(&id, "coherence.merged_checks").is_empty());

    let accepted = f.dispatch(&["accept", &id, "--expect", &digest[..12].to_uppercase()]);

    let out = stdout(&accepted);
    assert!(accepted.status.success(), "{out}{}", stderr(&accepted));
    assert!(out.contains("Merged checks: passed"), "{out}");
    command_line(&out, "test -f src/new.rs", "PASS", 0);
    assert_eq!(fs::read_to_string(f.root.join("src/lib.rs")).unwrap(), TWO);
    let reviews = f.events(&id, "review.accepted");
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0]["delta_digest"], digest);
    let merged = f.events(&id, "coherence.merged_checks");
    assert_eq!(merged.len(), 1, "{merged:?}");
    assert_eq!(merged[0]["by"], "accept");
    assert_eq!(merged[0]["delta_digest"], digest);
    assert!(merged[0]["results"][0]["duration_ms"].is_u64());
}

#[test]
fn accept_reports_each_check_that_failed_on_the_merged_tree() {
    let f = Fixture::new(&["test ! -f forbidden.txt"]);
    let id = f.attach();
    f.edit_workspace();
    f.finish(&id);
    fs::write(f.root.join("forbidden.txt"), "added by someone else\n").unwrap();

    let refused = f.dispatch(&["accept", &id]);

    assert!(!refused.status.success());
    let out = stdout(&refused);
    assert!(out.contains("Merged checks: did not pass"), "{out}");
    command_line(&out, "test ! -f forbidden.txt", "FAIL", 1);
    assert!(!f.root.join("src/new.rs").exists());
    let merged = f.events(&id, "coherence.merged_checks");
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0]["passed"], false);
    assert!(merged[0]["results"][0]["duration_ms"].is_u64());
}

#[test]
fn finish_check_and_diff_stat_show_the_digest_and_json_keeps_its_fields() {
    let f = Fixture::new(&["true", "test -f src/new.rs"]);
    let id = f.attach();
    f.edit_workspace();

    let finished = f.finish(&id);

    let digest = f.digest(&id);
    let shown = format!("Δ {}", &digest[..12]);
    assert!(
        finished.lines().any(|line| line.trim() == shown),
        "{finished}"
    );
    command_line(&finished, "true", "PASS", 0);
    command_line(&finished, "test -f src/new.rs", "PASS", 0);

    let stat = stdout(&f.dispatch(&["diff", &id, "--stat"]));
    assert!(stat.lines().any(|line| line == shown), "{stat}");
    let patch = stdout(&f.dispatch(&["diff", &id]));
    assert!(!patch.contains('Δ'), "{patch}");

    let check = stdout(&f.dispatch(&["check", &id]));
    assert!(check.lines().any(|line| line == shown), "{check}");

    let json: Value =
        serde_json::from_slice(&f.dispatch(&["check", &id, "--json"]).stdout).unwrap();
    assert_eq!(json["run_id"], id);
    assert_eq!(json["validity"]["decision"], "continue");
    assert_eq!(json["delta_digest"], digest);

    let merged = f.dispatch(&["check", &id, "--merged", "--json"]);
    assert!(merged.status.success(), "{}", stderr(&merged));
    let merged: Value = serde_json::from_slice(&merged.stdout).unwrap();
    let mut keys = merged
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    assert_eq!(
        keys,
        ["delta_digest", "merged_checks", "run_id", "validity"]
    );
    assert_eq!(merged["validity"]["decision"], "continue");
    assert_eq!(merged["merged_checks"]["passed"], true);
    assert_eq!(
        merged["merged_checks"]["results"].as_array().unwrap().len(),
        2
    );
}
