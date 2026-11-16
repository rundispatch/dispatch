//! Batch landing: `dispatch accept` with no id, when two or more results of a
//! Git project wait for review. The fixture is a Git repository (`root`) with
//! real Rust declarations, so footprints, edges and verdicts are real; each
//! piece of Work is a linked worktree attached as foreign work that the test
//! edits directly, playing an agent Dispatch did not launch.
#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde_json::Value;
use sha2::{Digest, Sha256};

const AUTH: &str = "pub fn validate(token: &str) -> bool {\n    let a = 1;\n    let b = 2;\n    \
let c = 3;\n    let d = 4;\n    let e = 5;\n    let f = 6;\n    let g = 7;\n    let h = 8;\n    \
!token.is_empty() && a + b + c + d + e + f + g + h > 0\n}\n\npub fn refresh(token: &str) -> String {\n    \
token.to_owned()\n}\n";
const SIGNATURE: (&str, &str) = (
    "validate(token: &str)",
    "validate(token: &str, strict: bool)",
);
const CALLER: &str = "pub fn serve() {}\n\npub fn login(t: &str) -> bool {\n    validate(t)\n}\n";

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
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

struct Project {
    temp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
}

impl Project {
    /// `verify` is the root's `checks.verify`; `{root}` in a command is the
    /// test's temporary directory.
    fn new(verify: &[&str]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/auth.rs"), AUTH).unwrap();
        fs::write(root.join("src/api.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(root.join("src/cli.rs"), "pub fn main() {}\n").unwrap();
        let commands = verify
            .iter()
            .map(|command| format!("    - {command:?}\n"))
            .collect::<String>();
        let checks = if verify.is_empty() {
            String::new()
        } else {
            format!("checks:\n  verify:\n{commands}")
        };
        fs::write(
            root.join("dispatch.yml"),
            format!("{checks}coherence:\n  poll_secs: 1\n"),
        )
        .unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
        Self {
            root: fs::canonicalize(&root).unwrap(),
            state: temp.path().join("state"),
            temp,
        }
    }

    fn dispatch(&self, args: &[&str]) -> Output {
        Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&self.state)
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.dispatch(args);
        assert!(
            output.status.success(),
            "dispatch {args:?}: {}",
            text(&output)
        );
        stdout(&output)
    }

    fn fails(&self, args: &[&str]) -> Output {
        let output = self.dispatch(args);
        assert!(
            !output.status.success(),
            "dispatch {args:?}: {}",
            text(&output)
        );
        output
    }

    /// A linked worktree attached as foreign Work, not yet finished.
    fn attach(&self, name: &str, files: &[(&str, &str)]) -> String {
        let workspace = self.temp.path().join(name);
        git(
            &self.root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                name,
                workspace.to_str().unwrap(),
            ],
        );
        let out = self.ok(&[
            "attach",
            "--workspace",
            workspace.to_str().unwrap(),
            "--agent",
            name,
            "--allow-unsafe-local",
        ]);
        for (path, contents) in files {
            fs::write(workspace.join(path), contents).unwrap();
        }
        out.lines()
            .find_map(|line| line.strip_prefix("ATTACHED "))
            .expect("attach prints ATTACHED <id>")
            .to_owned()
    }

    /// `attach`, then `finish`: a result waiting for review.
    fn work(&self, name: &str, files: &[(&str, &str)]) -> String {
        let id = self.attach(name, files);
        self.ok(&["finish", &id]);
        id
    }

    /// `dispatch accept` without a terminal: it proposes, decides nothing,
    /// and names the batch digest to accept.
    fn propose(&self) -> (String, String) {
        let output = self.fails(&["accept"]);
        let err = stderr(&output);
        assert!(err.contains("nothing was decided"), "{}", text(&output));
        let digest = err
            .split("dispatch accept --expect ")
            .nth(1)
            .expect("the command to accept the batch")
            .trim()
            .to_owned();
        (stdout(&output), digest)
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.root.join(path)).unwrap_or_default()
    }

    fn run(&self, id: &str) -> Value {
        serde_json::from_slice(
            &fs::read(self.state.join("runs").join(id).join("metadata.json")).unwrap(),
        )
        .unwrap()
    }

    fn delta(&self, id: &str) -> String {
        hex::encode(Sha256::digest(
            fs::read(self.state.join("runs").join(id).join("delta.patch")).unwrap(),
        ))
    }

    /// Every event as (run id, kind, payload), in the order committed.
    fn events(&self) -> Vec<(String, String, Value)> {
        let db = rusqlite::Connection::open(self.state.join("dispatch.db")).unwrap();
        let mut statement = db
            .prepare("SELECT run_id, event_type, payload_json FROM events ORDER BY id")
            .unwrap();
        statement
            .query_map([], |row| {
                let payload: String = row.get(2)?;
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    serde_json::from_str(&payload).unwrap(),
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn of(&self, kind: &str) -> Vec<(String, Value)> {
        self.events()
            .into_iter()
            .filter(|(_, event, _)| event == kind)
            .map(|(id, _, payload)| (id, payload))
            .collect()
    }

    /// The merged-check runs accept made, real or reused, as (run, payload).
    fn merged_runs(&self) -> Vec<(String, Value)> {
        self.of("coherence.merged_checks")
            .into_iter()
            .filter(|(_, payload)| payload["by"] == "accept")
            .collect()
    }

    fn source_commits(&self) -> Vec<String> {
        if git(&self.root, &["for-each-ref", "refs/dispatch/source"]).is_empty() {
            return Vec::new();
        }
        git(
            &self.root,
            &["reflog", "--format=%H", "refs/dispatch/source"],
        )
        .lines()
        .map(str::to_owned)
        .collect()
    }

    fn lock_is_free(&self, path: &Path) -> bool {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap();
        // SAFETY: the descriptor is open for the whole call. Closing the file
        // lets go of the lock again.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
    }

    fn source_lock(&self) -> PathBuf {
        let key = hex::encode(Sha256::digest(self.root.to_string_lossy().as_bytes()));
        self.state.join("locks").join(format!("source-{key}.lock"))
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = self.dispatch(&["stop"]);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn text(output: &Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

/// The proposal's line for its member at `position`, from 1, after
/// "  <position>. ": "<id>  Δ <12 hex>  <task>". Matched as a whole line
/// prefix, so no digest or id elsewhere in the output can stand in for it.
fn member(proposal: &str, position: usize) -> Option<&str> {
    let prefix = format!("  {position}. ");
    proposal.lines().find_map(|line| line.strip_prefix(&prefix))
}

/// The proposal's lines under the heading line `heading`, up to the next
/// line that is not indented; `None` without that heading.
fn section<'a>(proposal: &'a str, heading: &str) -> Option<Vec<&'a str>> {
    let mut lines = proposal.lines().skip_while(|line| *line != heading);
    lines.next()?;
    Some(lines.take_while(|line| line.starts_with(' ')).collect())
}

/// A changer and its user: the changer comes first though it finished last,
/// and the user is left out as REFRESH, judged against the changer.
#[test]
fn a_change_lands_before_what_uses_it_and_the_user_is_left_out() {
    let p = Project::new(&["true"]);
    let user = p.work("wt-user", &[("src/api.rs", CALLER)]);
    let changer = p.work(
        "wt-changer",
        &[("src/auth.rs", &AUTH.replacen(SIGNATURE.0, SIGNATURE.1, 1))],
    );

    let (proposal, digest) = p.propose();
    assert!(
        member(&proposal, 1).is_some_and(|line| line.starts_with(&format!("{changer}  Δ "))),
        "{proposal}"
    );
    assert!(member(&proposal, 2).is_none(), "{proposal}");
    // Why it comes first: before the user, named by a prefix of its id.
    let why = proposal
        .lines()
        .find_map(|line| line.strip_prefix("       before "))
        .and_then(|why| why.split_once(": "));
    assert!(
        why.is_some_and(|(who, what)| who.len() >= 8
            && user.starts_with(who)
            && what == "this Work changes the signature of validate (src/auth.rs), which it uses"),
        "{proposal}"
    );
    let left = section(&proposal, "Left out:").expect(&proposal);
    assert!(
        left.first()
            .is_some_and(|line| line.starts_with(&format!("  {user}  ")))
            && left
                .get(1)
                .is_some_and(|line| line.starts_with("       REFRESH: fact_broken: ")),
        "{proposal}"
    );

    p.ok(&["accept", "--expect", &digest]);
    assert!(p.read("src/auth.rs").contains(SIGNATURE.1));
    assert_eq!(p.read("src/api.rs"), "pub fn serve() {}\n");
    assert_eq!(p.run(&changer)["outcome"]["review"], "accepted");
    assert_eq!(p.run(&user)["outcome"]["review"], "pending");
    assert_eq!(p.run(&user)["outcome"]["application"], "not_applied");
}

/// Three independent results: one merged-check run on all of them, every
/// one applied as one patch, the verified blobs in the root, one commit in
/// `refs/dispatch/source` with an entry per member, and each member recorded
/// as a single accept records it, naming the batch.
#[test]
fn independent_results_land_together_on_one_check_run() {
    let p = Project::new(&["true", "test -f src/auth.rs"]);
    let files = [
        ("wt-a", "src/a.rs", "pub fn a() -> i32 {\n    1\n}\n"),
        ("wt-b", "src/b.rs", "pub fn b() -> i32 {\n    2\n}\n"),
        (
            "wt-c",
            "src/cli.rs",
            "pub fn main() {\n    println!(\"c\");\n}\n",
        ),
    ];
    let ids: Vec<String> = files
        .iter()
        .map(|(name, path, contents)| p.work(name, &[(path, contents)]))
        .collect();
    let head = git(&p.root, &["rev-parse", "HEAD"]);
    let index = fs::read(p.root.join(".git/index")).unwrap();

    let (proposal, digest) = p.propose();
    for (position, id) in ids.iter().enumerate() {
        let shown = format!("{id}  Δ {}  ", &p.delta(id)[..12]);
        assert!(
            member(&proposal, position + 1).is_some_and(|line| line.starts_with(&shown)),
            "{proposal}"
        );
    }
    assert!(member(&proposal, 4).is_none(), "{proposal}");
    let out = p.ok(&["accept", "--expect", &digest]);
    assert!(out.contains("Applied 3 results"), "{out}");

    for (_, path, contents) in &files {
        assert_eq!(p.read(path), *contents);
    }
    // Only the working tree changed: no commit, nothing staged.
    assert_eq!(git(&p.root, &["rev-parse", "HEAD"]), head);
    assert_eq!(fs::read(p.root.join(".git/index")).unwrap(), index);

    let merged = p.merged_runs();
    assert_eq!(merged.len(), 1, "{merged:?}");
    assert_eq!(merged[0].0, ids[0]);
    assert_eq!(merged[0].1["passed"], true);
    assert!(merged[0].1["batch"].as_str().unwrap().starts_with(&digest));
    let reused = p.of("checks.reused");
    assert_eq!(
        reused.iter().map(|(id, _)| id).collect::<Vec<_>>(),
        [&ids[1], &ids[2]]
    );
    for (id, payload) in &reused {
        assert_eq!(payload["reused_run"], ids[0].as_str());
        assert_eq!(payload["delta_digest"], p.delta(id));
        assert_eq!(payload["tree"], merged[0].1["tree_id"]);
    }
    for (id, payload) in p.of("batch.verified") {
        assert!(ids.contains(&id));
        assert_eq!(
            (payload["passed"].clone(), payload["runs"].clone()),
            (true.into(), 1.into())
        );
    }

    // One commit names the landing, for every member.
    let commits = p.source_commits();
    assert_eq!(commits.len(), 1, "{commits:?}");
    let key = hex::encode(Sha256::digest(p.root.to_string_lossy().as_bytes()));
    let sources: Value = serde_json::from_slice(
        &fs::read(p.state.join("projects").join(key).join("sources.json")).unwrap(),
    )
    .unwrap();
    let entries = sources.as_array().unwrap();
    assert_eq!(entries.len(), 3);
    for (entry, id) in entries.iter().zip(&ids) {
        assert_eq!(entry["landed_run"], id.as_str());
        assert_eq!(entry["commit"], commits[0].as_str());
    }

    // A fourth result, accepted on its own, is recorded the same way, but
    // for the batch.
    let single = p.work("wt-d", &[("src/d.rs", "pub fn d() {}\n")]);
    p.ok(&["accept", &single]);
    let landing_kinds = ["result.applied", "interaction.landed", "review.accepted"];
    let kinds = |id: &str| -> Vec<String> {
        p.events()
            .into_iter()
            .filter(|(run, kind, _)| run == id && landing_kinds.contains(&kind.as_str()))
            .map(|(_, kind, _)| kind)
            .collect()
    };
    let outcome = |id: &str| {
        let mut outcome = p.run(id)["outcome"].clone();
        outcome["phase"] = Value::Null;
        (outcome, p.run(id)["status"].clone())
    };
    for id in &ids {
        assert_eq!(kinds(id), kinds(&single), "{id}");
        assert_eq!(outcome(id), outcome(&single), "{id}");
    }
    for kind in landing_kinds {
        let single_keys: Vec<String> = p
            .of(kind)
            .into_iter()
            .find(|(id, _)| *id == single)
            .map(|(_, payload)| payload.as_object().unwrap().keys().cloned().collect())
            .unwrap();
        for (id, payload) in p.of(kind).into_iter().filter(|(id, _)| ids.contains(id)) {
            let mut keys: Vec<String> = payload.as_object().unwrap().keys().cloned().collect();
            assert!(
                payload["batch"].as_str().unwrap().starts_with(&digest),
                "{kind} {id}"
            );
            keys.retain(|key| key != "batch");
            assert_eq!(keys, single_keys, "{kind} {id}");
        }
    }
    for (id, payload) in p.of("review.accepted") {
        assert_eq!(payload["delta_digest"], p.delta(&id));
    }
}

/// Two results that change the same declaration: the later one is left out,
/// judged against the first.
#[test]
fn of_two_results_that_edit_one_declaration_the_later_is_left_out() {
    let p = Project::new(&["true"]);
    let first = p.work(
        "wt-a",
        &[(
            "src/auth.rs",
            &AUTH.replacen("let a = 1;", "let a = 10;", 1),
        )],
    );
    let later = p.work(
        "wt-b",
        &[(
            "src/auth.rs",
            &AUTH.replacen("let h = 8;", "let h = 80;", 1),
        )],
    );

    let (proposal, digest) = p.propose();
    assert!(
        member(&proposal, 1).is_some_and(|line| line.starts_with(&format!("{first}  Δ ")))
            && member(&proposal, 2).is_none(),
        "{proposal}"
    );
    let left = section(&proposal, "Left out:").expect(&proposal);
    assert!(
        left.first()
            .is_some_and(|line| line.starts_with(&format!("  {later}  ")))
            && left[1..]
                .iter()
                .any(|line| line.starts_with("       REFRESH: same_symbol_edited: ")),
        "{proposal}"
    );

    p.ok(&["accept", "--expect", &digest]);
    assert!(p.read("src/auth.rs").contains("let a = 10;"));
    assert!(p.read("src/auth.rs").contains("let h = 8;"));
    assert_eq!(p.run(&later)["outcome"]["review"], "pending");
}

/// The second result's Δ no longer applies on top of the first, which changed
/// a line its hunk holds as context though no line it edits: it is staged
/// 3-way from its S0, checked with the first, and both land.
#[test]
fn a_result_whose_context_the_one_before_it_changed_lands_3_way() {
    const LIB: &str =
        "pub fn one() -> i32 {\n    1\n}\n\n// three\npub fn three() -> i32 {\n    3\n}\n";
    let p = Project::new(&["true"]);
    fs::write(p.root.join("src/lib.rs"), LIB).unwrap();
    git(&p.root, &["add", "-A"]);
    git(&p.root, &["commit", "--quiet", "-m", "lib"]);
    let context = LIB.replace("// three\n", "// three, the last\n");
    let first = p.work("wt-a", &[("src/lib.rs", &context)]);
    let body = LIB.replace("    3\n", "    30\n");
    let second = p.work("wt-b", &[("src/lib.rs", &body)]);
    let frozen = p.delta(&second);

    let (proposal, digest) = p.propose();
    assert!(
        member(&proposal, 1).is_some_and(|line| line.starts_with(&format!("{first}  Δ ")))
            && member(&proposal, 2).is_some_and(|line| line.starts_with(&format!("{second}  Δ ")))
            && member(&proposal, 3).is_none(),
        "{proposal}"
    );
    assert!(section(&proposal, "Left out:").is_none(), "{proposal}");
    p.ok(&["accept", "--expect", &digest]);

    assert_eq!(p.read("src/lib.rs"), context.replace("    3\n", "    30\n"));
    let merged = p.merged_runs();
    assert_eq!(merged.len(), 1, "{merged:?}");
    assert_eq!(merged[0].1["passed"], true);
    // Δ stays exactly as the person reviewed it.
    assert_eq!(p.delta(&second), frozen);
    for id in [&first, &second] {
        assert_eq!(p.run(id)["outcome"]["review"], "accepted");
    }
}

/// What `--expect` refuses, applying nothing: a digest that is not the
/// batch's, a batch whose results changed since it was shown, and a
/// member's own Δ digest, which names one Work, not a batch.
#[test]
fn accept_refuses_another_batch_a_changed_set_and_a_members_delta() {
    let p = Project::new(&["true"]);
    let a = p.work("wt-a", &[("src/a.rs", "pub fn a() {}\n")]);
    let b = p.work("wt-b", &[("src/b.rs", "pub fn b() {}\n")]);
    let (_, digest) = p.propose();
    let unchanged = || {
        assert!(!p.root.join("src/a.rs").exists() && !p.root.join("src/b.rs").exists());
        assert!(p.of("review.accepted").is_empty() && p.of("result.applied").is_empty());
        assert!(p.merged_runs().is_empty());
    };

    let refused = stderr(&p.fails(&["accept", "--expect", "0123456789ab"]));
    assert!(refused.contains("nothing was applied"), "{refused}");
    unchanged();

    let own = &p.delta(&a)[..12];
    let refused = stderr(&p.fails(&["accept", "--expect", own]));
    assert!(
        refused.contains(&format!("dispatch accept {a} --expect {own}")),
        "{refused}"
    );
    unchanged();

    let c = p.work("wt-c", &[("src/c.rs", "pub fn c() {}\n")]);
    let refused = p.fails(&["accept", "--expect", &digest]);
    assert!(
        stderr(&refused).contains("changed since batch"),
        "{}",
        text(&refused)
    );
    assert!(
        member(&stdout(&refused), 3).is_some_and(|line| line.starts_with(&format!("{c}  Δ "))),
        "{}",
        text(&refused)
    );
    unchanged();
    for id in [&a, &b, &c] {
        assert_eq!(p.run(id)["outcome"]["review"], "pending");
    }
}

/// When the checks fail on the whole batch, bisection finds the first
/// prefix that fails, checking each tree once; the prefix before it lands,
/// and the failing result is named with its logs.
#[test]
fn a_failing_batch_lands_the_prefix_before_the_first_failing_result() {
    let p = Project::new(&["true", "test ! -e src/a.rs -o ! -e src/c.rs"]);
    let a = p.work("wt-a", &[("src/a.rs", "pub fn a() {}\n")]);
    let b = p.work("wt-b", &[("src/b.rs", "pub fn b() {}\n")]);
    let c = p.work("wt-c", &[("src/c.rs", "pub fn c() {}\n")]);
    // Someone else's change lands first, so no tree here was checked before.
    fs::write(p.root.join("notes.txt"), "elsewhere\n").unwrap();

    let (_, digest) = p.propose();
    let output = p.fails(&["accept", "--expect", &digest]);
    let out = stdout(&output);
    assert!(
        out.contains(&format!(
            "The first result the merged checks fail with: {c}"
        )),
        "{}",
        text(&output)
    );
    assert!(out.contains("test ! -e src/a.rs -o ! -e src/c.rs"), "{out}");
    assert!(stderr(&output).contains(&format!("Work {c} and the results after it did not land")));

    assert!(p.root.join("src/a.rs").exists() && p.root.join("src/b.rs").exists());
    assert!(!p.root.join("src/c.rs").exists());
    assert_eq!(p.run(&c)["outcome"]["review"], "pending");
    for id in [&a, &b] {
        assert_eq!(p.run(id)["outcome"]["review"], "accepted");
    }
    // The whole batch, then the first result, then the first two.
    let merged = p.merged_runs();
    let checked: Vec<(usize, bool)> = merged
        .iter()
        .map(|(id, payload)| {
            assert_eq!(id, &a);
            let members = payload["members"].as_array().unwrap().len();
            (members, payload["passed"].as_bool().unwrap())
        })
        .collect();
    assert_eq!(checked, [(3, false), (1, true), (2, true)]);
    let verified = p.of("batch.verified");
    assert_eq!(verified.len(), 3);
    for (_, payload) in verified {
        assert_eq!(payload["runs"], 3);
        assert_eq!(payload["lands"], 2);
        assert_eq!(payload["failed"], c.as_str());
    }
}

/// When the first result fails with the source, nothing lands, and the
/// message says the source's own checks may be failing.
#[test]
fn when_the_first_result_fails_nothing_lands() {
    let p = Project::new(&["test ! -e src/a.rs -o ! -e marker"]);
    let a = p.work("wt-a", &[("src/a.rs", "pub fn a() {}\n")]);
    let b = p.work("wt-b", &[("src/b.rs", "pub fn b() {}\n")]);
    fs::write(p.root.join("marker"), "the source moved\n").unwrap();

    let (_, digest) = p.propose();
    let output = p.fails(&["accept", "--expect", &digest]);
    assert!(
        stderr(&output).contains("the source's own checks may be failing"),
        "{}",
        text(&output)
    );
    assert!(!p.root.join("src/a.rs").exists() && !p.root.join("src/b.rs").exists());
    assert_eq!(p.merged_runs().len(), 2);
    for id in [&a, &b] {
        assert_eq!(p.run(id)["outcome"]["review"], "pending");
        assert_eq!(p.run(id)["outcome"]["application"], "not_applied");
    }
    assert!(p.source_commits().is_empty());
}

/// Watched, each member's landing lists the Work still in progress as a
/// counterpart, and never another member of its batch.
#[test]
fn members_that_landed_together_are_not_each_others_counterparts() {
    let p = Project::new(&["true"]);
    let a = p.work("wt-a", &[("src/a.rs", "pub fn a() {}\n")]);
    let b = p.work("wt-b", &[("src/b.rs", "pub fn b() {}\n")]);
    let live = p.attach("wt-live", &[("src/cli.rs", "pub fn main() { }\n")]);
    p.ok(&["start"]);
    let projection = p.state.join("watchers");
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let listed = fs::read_dir(&projection)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .find(|path| path.to_string_lossy().ends_with(".interactions.json"))
            .and_then(|path| serde_json::from_slice::<Value>(&fs::read(path).ok()?).ok());
        let has = |view: &Value, id: &str, delta: &str| {
            view["participants"]
                .as_array()
                .unwrap()
                .iter()
                .any(|participant| {
                    participant["run_id"] == id && participant["delta_sha256"] == delta
                })
        };
        if listed.as_ref().is_some_and(|view| {
            has(view, &a, &p.delta(&a))
                && has(view, &b, &p.delta(&b))
                && view["participants"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|x| x["run_id"] == live.as_str())
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the owner lists the Work: {listed:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    let (_, digest) = p.propose();
    p.ok(&["accept", "--expect", &digest]);
    let landed = p.of("interaction.landed");
    assert_eq!(landed.len(), 2, "{landed:?}");
    let batch = p.of("batch.landed");
    for (id, landing) in landed {
        assert_eq!(landing["status"], "observed", "{landing}");
        assert!(landing["batch"].as_str().unwrap().starts_with(&digest));
        let counterparts: Vec<&str> = landing["counterparts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|counterpart| counterpart["run_id"].as_str().unwrap())
            .collect();
        assert_eq!(counterparts, [live.as_str()], "{id}");
        assert_eq!(landing["world_before"], batch[0].1["world_before"]);
        assert_eq!(landing["world_after"], batch[0].1["world_after"]);
        assert_ne!(landing["world_before"], landing["world_after"]);
    }
}

/// A plain directory, and a project where only one waiting result may land
/// with others, decide as they always did: two results waiting is a refusal
/// that names each, and no batch is proposed.
#[test]
fn a_directory_source_and_one_eligible_result_behave_as_before() {
    let p = Project::new(&["test ! -e src/bad.rs"]);
    p.work("wt-a", &[("src/a.rs", "pub fn a() {}\n")]);
    let failing = p.work("wt-bad", &[("src/bad.rs", "pub fn bad() {}\n")]);
    assert_eq!(p.run(&failing)["outcome"]["verification"], "failed");
    let refused = stderr(&p.fails(&["accept"]));
    assert!(refused.contains("2 results wait for review"), "{refused}");

    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("plain");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("original.txt"), "baseline\n").unwrap();
    let state = temp.path().join("state");
    let dispatch = |args: &[&str]| {
        Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&state)
            .args(args)
            .current_dir(&source)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    for _ in 0..2 {
        let output = dispatch(&[
            "run",
            source.to_str().unwrap(),
            "--allow-unsafe-local",
            "--task",
            "Create the fake artifact.",
            "--agent",
            "fake-good",
        ]);
        assert!(output.status.success(), "{}", text(&output));
    }
    let output = dispatch(&["accept"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("2 results wait for review"),
        "{}",
        text(&output)
    );
    assert!(!source.join("dispatch-fake-good.txt").exists());
    assert!(p.of("batch.proposed").is_empty());
}

/// On a terminal, `accept` asks with Cancel focused, holding no lock while
/// it waits. Declining applies nothing; confirming lands the batch.
#[test]
fn the_menu_holds_no_lock_and_only_a_confirmation_lands() {
    let p = Project::new(&["true"]);
    let a = p.work("wt-a", &[("src/a.rs", "pub fn a() {}\n")]);
    let b = p.work("wt-b", &[("src/b.rs", "pub fn b() {}\n")]);

    let mut terminal = Terminal::open(&p, &["--plain", "accept"]);
    terminal.wait_for("Cancel");
    assert!(p.lock_is_free(&p.source_lock()));
    for id in [&a, &b] {
        assert!(p.lock_is_free(&p.state.join("runs").join(id).join(".operation.lock")));
    }
    terminal.answer("\n", "Nothing was decided.");
    assert!(!p.root.join("src/a.rs").exists() && !p.root.join("src/b.rs").exists());
    assert!(p.of("review.accepted").is_empty());
    assert_eq!(p.of("batch.proposed").len(), 2);

    let mut terminal = Terminal::open(&p, &["--plain", "accept"]);
    terminal.wait_for("Cancel");
    terminal.answer("1\n", "Review recorded for each.");
    assert!(p.root.join("src/a.rs").exists() && p.root.join("src/b.rs").exists());
    assert_eq!(p.of("review.accepted").len(), 2);
}

/// Dispatch on a pseudo-terminal, its output gathered as it comes.
struct Terminal {
    child: std::process::Child,
    master: fs::File,
    output: Arc<Mutex<Vec<u8>>>,
}

impl Terminal {
    fn open(p: &Project, args: &[&str]) -> Self {
        let (mut master, mut slave) = (0, 0);
        // SAFETY: openpty writes two new descriptors, owned below.
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0);
        // Children other tests spawn meanwhile must not keep this terminal.
        for fd in [master, slave] {
            // SAFETY: `fd` was just opened.
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        // SAFETY: both descriptors were just opened and are owned here only.
        let (master, slave) =
            unsafe { (fs::File::from_raw_fd(master), fs::File::from_raw_fd(slave)) };
        let child = Command::new(assert_cmd::cargo_bin!("dispatch"))
            .arg("--state-dir")
            .arg(&p.state)
            .args(args)
            .current_dir(&p.root)
            .env("TERM", "dumb")
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave)
            .spawn()
            .unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let (mut reader, gathered) = (master.try_clone().unwrap(), Arc::clone(&output));
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                gathered.lock().unwrap().extend_from_slice(&buffer[..read]);
            }
        });
        Self {
            child,
            master,
            output,
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    fn wait_for(&mut self, what: &str) {
        let mut deadline = Instant::now() + Duration::from_secs(300);
        while !self.text().contains(what) {
            // Once Dispatch has exited, all it printed is already on its way.
            if self.child.try_wait().unwrap().is_some() {
                deadline = deadline.min(Instant::now() + Duration::from_secs(10));
            }
            assert!(
                Instant::now() < deadline,
                "waiting for {what:?}: {}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Type `input`, then wait for Dispatch to say `said` and exit with
    /// success; everything it printed.
    fn answer(&mut self, input: &str, said: &str) -> String {
        self.master.write_all(input.as_bytes()).unwrap();
        self.wait_for(said);
        let deadline = Instant::now() + Duration::from_secs(300);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "dispatch did not exit: {}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(status.success(), "{}", self.text());
        self.text()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
