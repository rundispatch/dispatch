//! Δ merged 3-way from S0: when the source changed a context line of Δ's hunk
//! but no line Δ edits, `git apply` refuses Δ as it is, and Git's own 3-way
//! merge from the blobs S0 holds makes a tree with both changes. Such a
//! result is CONTINUE `merged_three_way`, which lands only once the merged
//! checks pass on that tree, and what lands is the merge, not Δ. The fixture
//! is a Git repository (`root`) and a linked worktree whose Work is attached;
//! the test edits both directly, and the checks are a stand-in that keeps a
//! copy of the file it saw on each run.
#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Three functions. The comment above `three` is context for an edit to its
/// body, and part of no declaration.
const LIB: &str = "pub fn one() -> i32 {\n    1\n}\n\npub fn two() -> i32 {\n    2\n}\n\
                   // three\npub fn three() -> i32 {\n    3\n}\n";

/// `sh <script> <dir>`: one more run in `<dir>/count`, and a copy of the
/// `src/lib.rs` it saw as `<dir>/seen-<run>.rs`.
const RECORD: &str = "n=$(cat \"$1/count\" 2>/dev/null || echo 0); n=$((n+1)); \
                      echo $n > \"$1/count\"; cp src/lib.rs \"$1/seen-$n.rs\"\n";

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    state: PathBuf,
    seen: PathBuf,
}

impl Fixture {
    /// `config` is the root's `dispatch.yml`; `{record}` in it is the
    /// recording check.
    fn new(config: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let script = base.join("record.sh");
        fs::write(&script, RECORD).unwrap();
        let seen = base.join("seen");
        fs::create_dir(&seen).unwrap();
        let record = format!("sh {} {}", script.display(), seen.display());
        let root = base.join("root");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), LIB).unwrap();
        fs::write(root.join("b.bin"), b"bin\0ary\n").unwrap();
        fs::write(
            root.join("dispatch.yml"),
            config.replace("{record}", &record),
        )
        .unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
        let workspace = base.join("wt");
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
            root,
            workspace,
            state: base.join("state"),
            seen,
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

    fn ok(&self, args: &[&str]) -> String {
        let output = self.dispatch(args);
        assert!(
            output.status.success(),
            "{args:?}: {}{}",
            stdout(&output),
            stderr(&output)
        );
        stdout(&output)
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_str(&self.ok(args)).unwrap()
    }

    /// The agent's finished Work: `edit` applied to its workspace.
    fn finished(&self, edit: impl FnOnce(&Path)) -> String {
        let id = self
            .ok(&[
                "attach",
                "--workspace",
                self.workspace.to_str().unwrap(),
                "--root",
                self.root.to_str().unwrap(),
                "--allow-unsafe-local",
            ])
            .lines()
            .find_map(|line| line.strip_prefix("ATTACHED "))
            .expect("attach prints ATTACHED <id>")
            .to_owned();
        edit(&self.workspace);
        self.ok(&["finish", &id]);
        id
    }

    /// Someone else's work lands in the root, committed.
    fn land_in_root(&self, path: &str, contents: &[u8]) {
        fs::write(self.root.join(path), contents).unwrap();
        git(&self.root, &["commit", "--quiet", "-am", "someone else"]);
    }

    fn run_dir(&self, id: &str) -> PathBuf {
        self.state.join("runs").join(id)
    }

    fn metadata(&self, id: &str) -> Value {
        serde_json::from_slice(&fs::read(self.run_dir(id).join("metadata.json")).unwrap()).unwrap()
    }

    fn events(&self, id: &str, kind: &str) -> Vec<Value> {
        let db = rusqlite::Connection::open(self.state.join("dispatch.db")).unwrap();
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

    /// How many times the recording check has run.
    fn runs(&self) -> u32 {
        fs::read_to_string(self.seen.join("count")).map_or(0, |count| count.trim().parse().unwrap())
    }

    /// What the root's checkout holds that a landing may change: its index
    /// file byte for byte, its HEAD, and its files.
    fn root_state(&self) -> (Vec<u8>, String, String, Vec<u8>) {
        (
            fs::read(self.root.join(".git/index")).unwrap(),
            git(&self.root, &["rev-parse", "HEAD"]),
            fs::read_to_string(self.root.join("src/lib.rs")).unwrap(),
            fs::read(self.root.join("b.bin")).unwrap(),
        )
    }

    /// `check --json`'s and `status --json`'s verdicts, which must agree:
    /// the decision and the reason codes.
    fn verdict(&self, id: &str) -> (String, Vec<String>) {
        let codes = |reasons: &Value| {
            reasons
                .as_array()
                .unwrap()
                .iter()
                .map(|reason| reason["code"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let checked = &self.json(&["check", id, "--json"])["validity"];
        let status = &self.json(&["status", id, "--json"])["coherence"];
        assert_eq!(checked["decision"], status["decision"]);
        assert_eq!(codes(&checked["reasons"]), codes(&status["reasons"]));
        (
            checked["decision"].as_str().unwrap().to_owned(),
            codes(&checked["reasons"]),
        )
    }
}

fn agent_edits_three(workspace: &Path) {
    fs::write(
        workspace.join("src/lib.rs"),
        LIB.replace("    3\n", "    30\n"),
    )
    .unwrap();
}

/// The comment above `three`, which the agent's hunk holds as context.
fn changed_context() -> String {
    LIB.replace("// three\n", "// three, the last\n")
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
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn sha256(path: &Path) -> String {
    hex::encode(Sha256::digest(fs::read(path).unwrap()))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn changed_context_lands_3_way_after_the_merged_checks_pass_and_check_status_and_accept_agree() {
    let f = Fixture::new("checks:\n  verify:\n    - {record}\n");
    let id = f.finished(agent_edits_three);
    assert_eq!(f.runs(), 1, "finish checks S0 + Δ in the workspace");
    f.land_in_root("src/lib.rs", changed_context().as_bytes());
    let (index, head, _, _) = f.root_state();
    let delta = f.run_dir(&id).join("delta.patch");
    let digest = sha256(&delta);

    assert_eq!(
        f.verdict(&id),
        ("continue".into(), vec!["merged_three_way".into()])
    );
    let checked = f.ok(&["check", &id]);
    assert!(
        checked.contains("Coherence: CONTINUE\n")
            && checked.contains("  merged_three_way: applied 3-way from S0\n"),
        "{checked}"
    );
    // Looking at it changed nothing in the root.
    assert_eq!(f.root_state().0, index);

    let accepted = f.ok(&["accept", &id, "--expect", &digest[..12]]);

    // The merged checks ran on the merged tree, and that tree landed.
    let merged = changed_context().replace("    3\n", "    30\n");
    assert_eq!(f.runs(), 2, "{accepted}");
    assert_eq!(
        fs::read_to_string(f.seen.join("seen-2.rs")).unwrap(),
        merged
    );
    assert!(
        accepted.contains("Merged checks: passed on the current source with this patch"),
        "{accepted}"
    );
    let landed = f.run_dir(&id).join("delta-landed.patch");
    assert!(
        accepted.contains(&format!(
            "Merged 3-way from S0; what landed: {}\n",
            landed.display()
        )),
        "{accepted}"
    );
    assert_eq!(
        f.root_state(),
        (index, head, merged, b"bin\0ary\n".to_vec())
    );
    assert_eq!(f.metadata(&id)["outcome"]["application"], "applied");

    // Δ is as frozen; what landed is recorded beside it, with both digests.
    assert_eq!(sha256(&delta), digest);
    assert_ne!(sha256(&landed), digest);
    let [applied] = &f.events(&id, "result.applied")[..] else {
        panic!("one landing");
    };
    assert_eq!(applied["delta_digest"], digest);
    assert_eq!(applied["landed_patch"], landed.to_str().unwrap());
    assert_eq!(applied["landed_digest"], sha256(&landed));
    assert_eq!(applied["confirmed"], true);
    assert_eq!(applied["unconfirmed"], serde_json::json!([]));
    let checks = f.events(&id, "coherence.merged_checks");
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert_eq!(checks[0]["delta_digest"], digest);
}

#[test]
fn overlapping_edits_stay_refresh_and_keep_what_the_3_way_merge_said() {
    let f = Fixture::new("checks:\n  verify:\n    - {record}\n");
    let id = f.finished(agent_edits_three);
    f.land_in_root(
        "src/lib.rs",
        changed_context().replace("    3\n", "    33\n").as_bytes(),
    );
    let before = f.root_state();

    assert_eq!(
        f.verdict(&id),
        ("refresh".into(), vec!["patch_conflict".into()])
    );
    let refused = f.dispatch(&["accept", &id]);

    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("this work is stale (REFRESH)"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(f.root_state(), before);
    assert_eq!(f.runs(), 1);
    let kept = fs::read_to_string(f.run_dir(&id).join("patch-conflict.txt")).unwrap();
    assert!(
        kept.starts_with("error: patch failed: src/lib.rs:")
            && kept.contains("\n\nMerged 3-way from S0:\n")
            && kept.contains("U src/lib.rs"),
        "{kept}"
    );
    assert!(!f.run_dir(&id).join("delta-landed.patch").exists());
}

#[test]
fn a_binary_file_changed_on_both_sides_stays_refresh() {
    let f = Fixture::new("checks:\n  verify:\n    - {record}\n");
    let id = f.finished(|workspace| fs::write(workspace.join("b.bin"), b"bin\0agent\n").unwrap());
    f.land_in_root("b.bin", b"bin\0world\n");
    let before = f.root_state();

    assert_eq!(
        f.verdict(&id),
        ("refresh".into(), vec!["patch_conflict".into()])
    );
    assert!(!f.dispatch(&["accept", &id]).status.success());
    assert_eq!(f.root_state(), before);
}

#[test]
fn a_3_way_merge_whose_merged_checks_cannot_run_is_refresh() {
    for (config, why) in [
        ("{}\n", "no checks are configured"),
        (
            "checks:\n  verify:\n    - {record}\ncoherence:\n  integration_checks: false\n",
            "they are turned off",
        ),
    ] {
        let f = Fixture::new(config);
        let id = f.finished(agent_edits_three);
        f.land_in_root("src/lib.rs", changed_context().as_bytes());
        let before = f.root_state();
        let runs = f.runs();

        assert_eq!(
            f.verdict(&id),
            (
                "refresh".into(),
                vec!["analysis_uncertain".into(), "merged_three_way".into()]
            ),
            "{why}"
        );
        let checked = f.json(&["check", &id, "--json"]);
        let detail = checked["validity"]["reasons"][0]["detail"]
            .as_str()
            .unwrap();
        assert!(
            detail.starts_with(&format!("merged checks cannot run — {why}")),
            "{detail}"
        );
        let refused = f.dispatch(&["accept", &id]);

        assert!(!refused.status.success(), "{why}");
        assert!(
            stderr(&refused).contains("this work is stale (REFRESH)"),
            "{why}: {}",
            stderr(&refused)
        );
        assert_eq!(f.root_state(), before, "{why}");
        assert_eq!(f.runs(), runs, "{why}");
        assert!(!f.run_dir(&id).join("delta-landed.patch").exists());
    }
}

#[test]
fn a_3_way_merge_whose_merged_checks_fail_is_refused_and_the_source_is_unchanged() {
    // Passes on S0 + Δ, where the comment is unchanged; fails on the merge.
    let f = Fixture::new("checks:\n  verify:\n    - \"! grep -q 'the last' src/lib.rs\"\n");
    let id = f.finished(agent_edits_three);
    f.land_in_root("src/lib.rs", changed_context().as_bytes());
    let before = f.root_state();
    assert_eq!(
        f.verdict(&id),
        ("continue".into(), vec!["merged_three_way".into()])
    );

    let refused = f.dispatch(&["accept", &id]);

    assert!(!refused.status.success());
    let said = stderr(&refused);
    assert!(
        said.contains("this work is stale (REFRESH)") && said.contains("grep -q 'the last'"),
        "{said}"
    );
    assert_eq!(f.root_state(), before);
    assert!(!f.run_dir(&id).join("delta-landed.patch").exists());
    let metadata = f.metadata(&id);
    assert_eq!(
        metadata["outcome"]["application"],
        "blocked_by_source_drift"
    );
    let codes = metadata["coherence"]["validity"]["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|reason| reason["code"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(codes, ["merged_three_way", "integration_check_failed"]);
}
