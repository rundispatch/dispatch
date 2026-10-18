//! The verification memo (contract A): a tree that passed its checks is not
//! checked again with the same commands in the same environment, whichever
//! of finish, `check --merged` or accept asks, and attached finish checks
//! what would land. The checks are a stand-in that counts its own runs in a
//! file outside every tree, so a reuse is visible as a count that did not
//! move. The fixture is a Git repository (`root`) and a linked worktree
//! (`workspace`) whose Work is attached, as an agent Dispatch did not launch.
#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    state: PathBuf,
    count: PathBuf,
}

/// `sh <script> <count file> [n]`: one more run in the count file; passes
/// from the `n`th run on (default: always).
const COUNTER: &str = "n=$(cat \"$1\" 2>/dev/null || echo 0); n=$((n+1)); echo $n > \"$1\"; [ \"$n\" -ge \"${2:-1}\" ]\n";

impl Fixture {
    /// `verify` is the root's `checks.verify`; `{count}` in a command is the
    /// counting check.
    fn new(verify: &[&str]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let script = base.join("count.sh");
        fs::write(&script, COUNTER).unwrap();
        let count = base.join("count");
        let counter = format!("sh {} {}", script.display(), count.display());
        let root = base.join("root");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), ONE).unwrap();
        let commands = verify
            .iter()
            .map(|command| format!("    - {}\n", command.replace("{count}", &counter)))
            .collect::<String>();
        fs::write(
            root.join("dispatch.yml"),
            format!("checks:\n  verify:\n{commands}"),
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
            count,
            _temp: temp,
        }
    }

    fn dispatch_with(&self, args: &[&str], path: Option<&str>) -> Output {
        let mut command = Command::new(assert_cmd::cargo_bin!("dispatch"));
        command
            .arg("--state-dir")
            .arg(&self.state)
            .args(args)
            .current_dir(&self.root);
        if let Some(path) = path {
            command.env("PATH", path);
        }
        command.output().unwrap()
    }

    fn dispatch(&self, args: &[&str]) -> Output {
        self.dispatch_with(args, None)
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

    fn attach(&self) -> String {
        let out = self.ok(&[
            "attach",
            "--workspace",
            self.workspace.to_str().unwrap(),
            "--root",
            self.root.to_str().unwrap(),
            "--allow-unsafe-local",
        ]);
        out.lines()
            .find_map(|line| line.strip_prefix("ATTACHED "))
            .expect("attach prints ATTACHED <id>")
            .to_owned()
    }

    /// The agent's work: an edit to a tracked file and a new, untracked one.
    fn edit_workspace(&self) {
        fs::write(self.workspace.join("src/lib.rs"), TWO).unwrap();
        fs::write(self.workspace.join("src/new.rs"), "pub fn g() {}\n").unwrap();
    }

    /// Someone else's work lands in the root: a committed, disjoint file.
    fn move_source(&self) {
        fs::write(self.root.join("notes.txt"), "someone else's work\n").unwrap();
        git(&self.root, &["add", "notes.txt"]);
        git(&self.root, &["commit", "--quiet", "-m", "notes"]);
    }

    /// How many times the counting check has run.
    fn runs(&self) -> u32 {
        fs::read_to_string(&self.count).map_or(0, |count| count.trim().parse().unwrap())
    }

    fn run_dir(&self, id: &str) -> PathBuf {
        self.state.join("runs").join(id)
    }

    /// Merged-check log directories of `id`: one per run on a merged tree.
    fn merged_runs(&self, id: &str) -> usize {
        fs::read_dir(self.run_dir(id).join("coherence-checks")).map_or(0, |entries| {
            entries
                .filter(|e| e.as_ref().unwrap().path().is_dir())
                .count()
        })
    }

    fn memo_entries(&self) -> Vec<PathBuf> {
        fs::read_dir(self.state.join("memo")).map_or_else(
            |_| Vec::new(),
            |entries| entries.map(|entry| entry.unwrap().path()).collect(),
        )
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

/// `PATH` with one more directory in front: a different environment.
fn other_path(f: &Fixture) -> String {
    let extra = f.state.parent().unwrap().join("bin");
    fs::create_dir_all(&extra).unwrap();
    format!("{}:{}", extra.display(), std::env::var("PATH").unwrap())
}

#[test]
fn check_merged_after_a_passing_finish_on_an_unmoved_source_reuses_it() {
    let f = Fixture::new(&["{count}"]);
    let id = f.attach();
    f.edit_workspace();
    let finished = f.ok(&["finish", &id]);
    // The source has not moved: one tree, checked once, in the workspace.
    assert_eq!(f.runs(), 1);
    assert_eq!(f.merged_runs(&id), 0);
    assert!(f.events(&id, "coherence.merged_checks").is_empty());
    assert!(!finished.contains("finish:"), "{finished}");

    let out = f.ok(&["check", &id, "--merged"]);

    assert_eq!(f.runs(), 1, "{out}");
    assert_eq!(f.merged_runs(&id), 0);
    assert!(f.events(&id, "coherence.merged_checks").is_empty());
    assert!(
        out.contains(&format!("with this Δ: passed, reused from {id} at ")),
        "{out}"
    );
    let reused = f.events(&id, "checks.reused");
    assert_eq!(reused.len(), 1, "{reused:?}");
    assert_eq!(reused[0]["by"], "check");
    assert_eq!(reused[0]["reused_run"], id);
    assert_eq!(
        reused[0]["logs"],
        f.run_dir(&id).join("checks/verify").to_str().unwrap()
    );
    assert_eq!(reused[0]["key"].as_str().unwrap().len(), 64);
    let key = reused[0]["key"].as_str().unwrap();
    assert_eq!(f.memo_entries(), [f.state.join(format!("memo/{key}.json"))]);

    // `check --json` and status say the same.
    let json: Value = serde_json::from_str(&f.ok(&["check", &id, "--merged", "--json"])).unwrap();
    assert_eq!(json["merged_checks"]["reused_run"], id);
    assert_eq!(json["merged_checks"]["passed"], true);
    assert_eq!(f.runs(), 1);
    let status = f.ok(&["status", &id]);
    assert!(
        status.contains(&format!("check: checks: passed, reused from {id} at ")),
        "{status}"
    );
    let status: Value = serde_json::from_str(&f.ok(&["status", &id, "--json"])).unwrap();
    assert_eq!(status["checks_reused"][0]["reused_run"], id);

    // `dispatch clean` removes the memo.
    f.ok(&["clean", "--yes"]);
    assert!(!f.state.join("memo").exists());
}

#[test]
fn accept_after_a_passing_check_merged_on_the_same_tree_reuses_it() {
    let f = Fixture::new(&["{count}"]);
    let id = f.attach();
    f.edit_workspace();
    f.ok(&["finish", &id]);
    f.move_source();
    f.ok(&["check", &id, "--merged"]);
    assert_eq!((f.runs(), f.merged_runs(&id)), (2, 1));

    let out = f.ok(&["accept", &id]);

    assert_eq!((f.runs(), f.merged_runs(&id)), (2, 1), "{out}");
    assert!(
        out.contains(&format!("Merged checks: passed, reused from {id} at ")),
        "{out}"
    );
    assert_eq!(fs::read_to_string(f.root.join("src/lib.rs")).unwrap(), TWO);
    let reused = f.events(&id, "checks.reused");
    assert_eq!(reused.len(), 1, "{reused:?}");
    assert_eq!(reused[0]["by"], "accept");
    let checked = f.events(&id, "coherence.merged_checks");
    assert_eq!(reused[0]["logs"], checked[0]["logs"]);
    assert_eq!(checked.len(), 1, "{checked:?}");
}

#[test]
fn a_moved_source_is_a_miss_and_its_own_pass_is_reused() {
    let f = Fixture::new(&["{count}"]);
    let id = f.attach();
    f.edit_workspace();
    f.ok(&["finish", &id]);
    f.move_source();

    f.ok(&["check", &id, "--merged"]);
    assert_eq!(f.runs(), 2);
    assert!(f.events(&id, "checks.reused").is_empty());

    let out = f.ok(&["check", &id, "--merged"]);
    assert_eq!(f.runs(), 2, "{out}");
    assert_eq!(f.events(&id, "checks.reused").len(), 1);
}

#[test]
fn a_changed_path_is_a_miss() {
    let f = Fixture::new(&["{count}"]);
    let id = f.attach();
    f.edit_workspace();
    let other = other_path(&f);

    // Unfinished Work: nothing has checked this tree before.
    f.ok(&["check", &id, "--merged"]);
    assert_eq!(f.runs(), 1);
    assert!(
        f.dispatch_with(&["check", &id, "--merged"], Some(&other))
            .status
            .success()
    );
    assert_eq!(f.runs(), 2);
    assert!(
        f.dispatch_with(&["check", &id, "--merged"], Some(&other))
            .status
            .success()
    );
    f.ok(&["check", &id, "--merged"]);
    assert_eq!(f.runs(), 2);
    assert_eq!(f.events(&id, "checks.reused").len(), 2);
    assert_eq!(f.memo_entries().len(), 2);
}

#[test]
fn a_changed_command_is_a_miss() {
    let f = Fixture::new(&["{count}"]);
    let id = f.attach();
    f.edit_workspace();
    f.ok(&["check", &id, "--merged"]);
    assert_eq!(f.runs(), 1);
    // The run's frozen commands gain one.
    let snapshot = f.run_dir(&id).join("config.snapshot.yml");
    let mut config: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(&snapshot).unwrap()).unwrap();
    config["checks"]["verify"]
        .as_sequence_mut()
        .unwrap()
        .push("true".into());
    fs::write(&snapshot, serde_yaml::to_string(&config).unwrap()).unwrap();

    f.ok(&["check", &id, "--merged"]);
    assert_eq!(f.runs(), 2);
    f.ok(&["check", &id, "--merged"]);
    assert_eq!(f.runs(), 2);
    assert_eq!(f.memo_entries().len(), 2);
}

#[test]
fn an_expired_pass_is_a_miss() {
    let f = Fixture::new(&["{count}"]);
    let id = f.attach();
    f.edit_workspace();
    f.ok(&["check", &id, "--merged"]);
    let [entry] = &f.memo_entries()[..] else {
        panic!("one pass is recorded")
    };
    let mut recorded: Value = serde_json::from_slice(&fs::read(entry).unwrap()).unwrap();
    let day_ago = chrono::Utc::now() - chrono::Duration::hours(24) - chrono::Duration::minutes(1);
    recorded["at"] = serde_json::json!(day_ago);
    fs::write(entry, serde_json::to_vec(&recorded).unwrap()).unwrap();

    f.ok(&["check", &id, "--merged"]);
    assert_eq!(f.runs(), 2);
    f.ok(&["check", &id, "--merged"]);
    assert_eq!(f.runs(), 2);
}

#[test]
fn a_failure_is_never_recorded() {
    // Fails on its first run, passes from the second.
    let f = Fixture::new(&["{count} 2"]);
    let id = f.attach();
    f.edit_workspace();

    let failed = f.dispatch(&["check", &id, "--merged"]);
    assert!(!failed.status.success());
    assert_eq!(f.runs(), 1);
    assert!(f.memo_entries().is_empty());

    f.ok(&["check", &id, "--merged"]);
    assert_eq!(f.runs(), 2);
    f.ok(&["check", &id, "--merged"]);
    assert_eq!(f.runs(), 2);
    assert_eq!(f.memo_entries().len(), 1);
}

#[test]
fn finish_on_a_moved_source_checks_what_would_land_and_accept_reuses_it() {
    // Passes only where someone else's work is: the current source.
    let f = Fixture::new(&["test -f notes.txt", "{count}"]);
    let id = f.attach();
    f.edit_workspace();
    f.move_source();

    let finished = f.ok(&["finish", &id]);

    assert!(finished.contains("Verification\n  PASS"), "{finished}");
    assert!(
        finished.contains("finish: checked on the current source with this Δ"),
        "{finished}"
    );
    assert_eq!((f.runs(), f.merged_runs(&id)), (1, 1));
    // S0 + Δ did not run.
    assert!(!f.run_dir(&id).join("checks/verify").exists());
    let merged = f.events(&id, "coherence.merged_checks");
    assert_eq!(merged.len(), 1, "{merged:?}");
    assert_eq!(
        (&merged[0]["by"], &merged[0]["passed"]),
        (&"finish".into(), &true.into())
    );

    let out = f.ok(&["accept", &id]);
    assert!(out.contains("Merged checks: passed, reused from"), "{out}");
    assert_eq!(f.runs(), 1);
    assert_eq!(fs::read_to_string(f.root.join("src/lib.rs")).unwrap(), TWO);
}

#[test]
fn finish_checks_s0_with_its_delta_only_when_the_current_source_fails() {
    let f = Fixture::new(&["test ! -f forbidden.txt", "{count}"]);
    let id = f.attach();
    f.edit_workspace();
    fs::write(f.root.join("forbidden.txt"), "added by someone else\n").unwrap();

    let finished = f.ok(&["finish", &id]);

    // Both ran: the merged tree failed, the Work's own start passed.
    assert_eq!((f.runs(), f.merged_runs(&id)), (2, 1));
    assert!(finished.contains("Verification\n  PASS"), "{finished}");
    assert!(
        finished.contains("the current source with this Δ did not pass; the checks ran again on this Δ's own start (S0)"),
        "{finished}"
    );
    let failed = finished
        .lines()
        .find(|line| line.trim_start().starts_with("FAIL"))
        .unwrap_or_else(|| panic!("{finished}"));
    assert!(failed.contains("test ! -f forbidden.txt"), "{failed}");
    assert!(f.run_dir(&id).join("checks/verify").is_dir());
    let merged = f.events(&id, "coherence.merged_checks");
    assert_eq!(merged.len(), 1, "{merged:?}");
    assert_eq!(
        (&merged[0]["by"], &merged[0]["passed"]),
        (&"finish".into(), &false.into())
    );
    // The merged failure is not recorded; S0 + Δ's pass is, for its own tree.
    assert_eq!(f.memo_entries().len(), 1);
}

#[test]
fn a_pass_is_not_reused_where_git_converts_line_endings() {
    // Passes only while `notes.txt` has no carriage return.
    let f = Fixture::new(&["{count}", r"tr -d '\r' < notes.txt | cmp -s - notes.txt"]);
    fs::write(f.root.join(".gitattributes"), "* text=auto\n").unwrap();
    fs::write(f.root.join("notes.txt"), "a\nb\n").unwrap();
    git(&f.root, &["add", "-A"]);
    git(&f.root, &["commit", "--quiet", "-m", "attributes"]);
    let id = f.attach();
    f.edit_workspace();
    f.ok(&["check", &id, "--merged"]);
    assert_eq!(f.runs(), 1);

    // The same text with CRLF: Git stores one blob for both.
    fs::write(f.root.join("notes.txt"), "a\r\nb\r\n").unwrap();
    let out = f.dispatch(&["check", &id, "--merged"]);

    assert_eq!(f.runs(), 2, "{}", stdout(&out));
    assert!(!out.status.success(), "{}", stdout(&out));
    assert!(f.events(&id, "checks.reused").is_empty());
    assert!(f.memo_entries().is_empty());
}

#[test]
fn finish_with_an_unknown_merged_tree_on_an_unmoved_source_checks_once_in_place() {
    let f = Fixture::new(&["{count}"]);
    // An untracked nested repository in the source: the merged tree's key
    // cannot be known.
    let nested = f.root.join("vendor");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("b.txt"), "b\n").unwrap();
    git(&nested, &["init", "--quiet"]);
    let id = f.attach();
    f.edit_workspace();

    f.ok(&["finish", &id]);

    assert_eq!((f.runs(), f.merged_runs(&id)), (1, 0));
    assert!(f.run_dir(&id).join("checks/verify").is_dir());
    assert!(f.events(&id, "coherence.merged_checks").is_empty());
    assert!(!f.state.join("check-workspaces").exists());
}

#[test]
fn finish_with_an_unknown_merged_tree_on_a_moved_source_checks_what_would_land() {
    let f = Fixture::new(&["{count}"]);
    let nested = f.root.join("vendor");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("b.txt"), "b\n").unwrap();
    git(&nested, &["init", "--quiet"]);
    let id = f.attach();
    f.edit_workspace();
    f.move_source();

    f.ok(&["finish", &id]);

    assert_eq!((f.runs(), f.merged_runs(&id)), (1, 1));
    assert!(!f.run_dir(&id).join("checks/verify").exists());
    assert_eq!(f.events(&id, "coherence.merged_checks").len(), 1);
}
