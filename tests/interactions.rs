//! Work that notices Work: the project owner compares the footprints of every
//! piece of Work not yet integrated and publishes where they interact, in
//! `<state>/watchers/<key>.interactions.json`. Advisory only: verdicts,
//! accept and auto-apply are untouched. Each piece of Work here is a linked
//! worktree attached as foreign work that the test edits directly, playing an
//! agent Dispatch did not launch.
#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};

use serde_json::Value;

const AUTH: &str = "pub fn validate(token: &str) -> bool {\n    !token.is_empty()\n}\n\n\
pub fn filler_1() {}\npub fn filler_2() {}\npub fn filler_3() {}\npub fn filler_4() {}\n\
pub fn filler_5() {}\npub fn filler_6() {}\npub fn filler_7() {}\npub fn filler_8() {}\n\n\
pub fn refresh(token: &str) -> String {\n    token.to_owned()\n}\n";
const SIGNATURE: (&str, &str) = (
    "validate(token: &str)",
    "validate(token: &str, strict: bool)",
);
const CALLER: &str = "pub fn serve() {}\n\npub fn login(t: &str) -> bool {\n    validate(t)\n}\n";

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
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
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

struct Project {
    temp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
}

impl Project {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/auth.rs"), AUTH).unwrap();
        fs::write(root.join("src/api.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(root.join("src/cli.rs"), "pub fn main() {}\n").unwrap();
        fs::write(root.join("dispatch.yml"), "coherence:\n  poll_secs: 1\n").unwrap();
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
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.dispatch(args);
        assert!(
            output.status.success(),
            "dispatch {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// A linked worktree attached as foreign Work; returns (workspace, id).
    fn work(&self, name: &str) -> (PathBuf, String) {
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
        ]);
        let id = out
            .lines()
            .find_map(|line| line.strip_prefix("ATTACHED "))
            .unwrap()
            .to_owned();
        (fs::canonicalize(workspace).unwrap(), id)
    }

    fn projection(&self) -> Option<Value> {
        let entries = fs::read_dir(self.state.join("watchers")).ok()?;
        entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .find(|path| path.to_string_lossy().ends_with(".interactions.json"))
            .and_then(|path| serde_json::from_slice(&fs::read(path).ok()?).ok())
    }

    /// The projection once `check` holds, within 30 s.
    fn until(&self, what: &str, check: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(projection) = self.projection()
                && check(&projection)
            {
                return projection;
            }
            assert!(Instant::now() < deadline, "{what}: {:?}", self.projection());
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn run(&self, id: &str) -> Value {
        serde_json::from_slice(
            &fs::read(self.state.join("runs").join(id).join("metadata.json")).unwrap(),
        )
        .unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = self.dispatch(&["stop"]);
    }
}

fn participants(projection: &Value) -> Vec<String> {
    let mut ids: Vec<String> = projection["participants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["run_id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

/// The rules between `a` and `b`, in either order.
fn rules(projection: &Value, a: &str, b: &str) -> Vec<String> {
    projection["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|edge| (edge["a"] == a && edge["b"] == b) || (edge["a"] == b && edge["b"] == a))
        .flat_map(|edge| edge["interactions"].as_array().unwrap().clone())
        .map(|i| i["rule"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn work_that_edits_into_an_overlap_interacts_until_it_reverts() {
    let p = Project::new();
    let (wa, a) = p.work("wt-a");
    let (wb, b) = p.work("wt-b");
    let (wd, d) = p.work("wt-d");
    p.ok(&["start"]);
    let mut ids = vec![a.clone(), b.clone(), d.clone()];
    ids.sort();
    p.until("all three participate", |v| participants(v) == ids);

    fs::write(
        wa.join("src/auth.rs"),
        AUTH.replacen(SIGNATURE.0, SIGNATURE.1, 1),
    )
    .unwrap();
    fs::write(wb.join("src/api.rs"), CALLER).unwrap();
    fs::write(wd.join("src/cli.rs"), "pub fn main() { }\n").unwrap();
    let seen = p.until("A changes what B uses", |v| rules(v, &a, &b) == ["uses"]);
    assert!(
        rules(&seen, &a, &d).is_empty() && rules(&seen, &b, &d).is_empty(),
        "{seen}"
    );
    let edge = &seen["edges"][0]["interactions"][0];
    assert_eq!(edge["change"], "signature", "{edge}");
    assert_eq!(edge["target"]["name"], "validate", "{edge}");

    // B changes its mind: nothing of A's is used any more.
    fs::write(wb.join("src/api.rs"), "pub fn serve() {}\n").unwrap();
    p.until("the edge is gone", |v| rules(v, &a, &b).is_empty());
}

#[test]
fn integrated_rejected_and_lost_work_leaves_the_comparison() {
    let p = Project::new();
    let (wa, a) = p.work("wt-a");
    let (wb, b) = p.work("wt-b");
    let (wc, c) = p.work("wt-c");
    fs::write(
        wa.join("src/auth.rs"),
        AUTH.replacen(SIGNATURE.0, SIGNATURE.1, 1),
    )
    .unwrap();
    fs::write(wb.join("src/api.rs"), CALLER).unwrap();
    let body = AUTH.replacen("!token.is_empty()", "token.len() > 2", 1);
    fs::write(wc.join("src/auth.rs"), body).unwrap();
    p.ok(&["start"]);
    let seen = p.until("A meets B and C", |v| {
        rules(v, &a, &b) == ["uses"] && rules(v, &a, &c) == ["same_declaration"]
    });
    // C changes only validate's body, which B merely calls.
    assert!(rules(&seen, &b, &c).is_empty(), "{seen}");

    // A lands: it is World now, and B is judged by coherence as before.
    p.ok(&["finish", &a]);
    p.until("finished A still participates", |v| {
        participants(v).contains(&a)
    });
    p.ok(&["accept", &a]);
    p.until("applied A leaves", |v| !participants(v).contains(&a));
    let deadline = Instant::now() + Duration::from_secs(30);
    while p.run(&b)["coherence"]["validity"]["decision"] != "refresh" {
        assert!(Instant::now() < deadline, "{}", p.run(&b)["coherence"]);
        std::thread::sleep(Duration::from_millis(200));
    }

    // Rejected Work leaves too.
    p.ok(&["finish", &c]);
    p.ok(&["reject", &c]);
    p.until("rejected C leaves", |v| participants(v) == [b.clone()]);

    // Lost Work (its workspace vanished unannounced) can never land.
    fs::remove_dir_all(&wb).unwrap();
    p.until("lost B leaves", |v| participants(v).is_empty());

    p.ok(&["stop"]);
    assert!(p.projection().is_none(), "the view goes with its owner");
}

#[test]
fn live_work_mid_edit_keeps_its_last_footprint_and_frozen_work_is_whole_file() {
    let p = Project::new();
    let (wa, a) = p.work("wt-a");
    let (wb, b) = p.work("wt-b");
    fs::write(wb.join("src/api.rs"), CALLER).unwrap();
    let body = AUTH.replacen("!token.is_empty()", "token.len() > 2", 1);
    fs::write(wa.join("src/auth.rs"), &body).unwrap();
    p.ok(&["start"]);
    let clean = p.until("both analyzed, no interaction", |v| {
        participants(v).len() == 2
            && v["participants"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["analysis"] == "analyzed")
    });
    assert!(rules(&clean, &a, &b).is_empty(), "{clean}");

    // Mid-edit, A's file does not parse: its last clean footprint stands.
    let broken = body.replacen("token.len() > 2\n}", "token.len( > 2\n", 1);
    fs::write(wa.join("src/auth.rs"), &broken).unwrap();
    let mid = p.until("A last seen", |v| {
        v["participants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["run_id"] == a && p["analysis"] == "last_seen")
    });
    assert!(
        rules(&mid, &a, &b).is_empty(),
        "no file-level warning mid-edit: {mid}"
    );

    // Frozen like that, the file is judged as a whole.
    p.ok(&["finish", &a]);
    p.until("frozen A is whole-file", |v| rules(v, &a, &b) == ["file"]);
}

#[test]
fn status_and_watch_show_interactions_apart_from_the_verdict() {
    let p = Project::new();
    let (wa, a) = p.work("wt-a");
    let (wb, b) = p.work("wt-b");
    let before = p.ok(&["status", &b]);
    assert!(
        before.contains("Concurrent\n  not known: the project is not watched"),
        "{before}"
    );

    fs::write(
        wa.join("src/auth.rs"),
        AUTH.replacen(SIGNATURE.0, SIGNATURE.1, 1),
    )
    .unwrap();
    fs::write(wb.join("src/api.rs"), CALLER).unwrap();
    p.ok(&["start"]);
    p.until("A changes what B uses", |v| rules(v, &a, &b) == ["uses"]);

    let status = p.ok(&["status", &b]);
    // Work attached within the same quarter second shares its first 8
    // characters; A is then named by as much as tells it from B.
    let shared = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    let expected = format!(
        "Concurrent\n  with {}: it changes the signature of validate (src/auth.rs), which this Work uses",
        &a[..(shared + 1).max(8)]
    );
    assert!(status.contains(&expected), "{status}");
    let json: Value = serde_json::from_str(&p.ok(&["status", &b, "--json"])).unwrap();
    let entry = &json["interactions"][0];
    assert_eq!(entry["with"], a.as_str(), "{json}");
    assert_eq!(entry["direction"], "theirs_affects_this");
    assert_eq!(entry["rule"], "uses");

    // `watch --json`: the work object carries its interactions next to, not
    // inside, its verdict.
    let mut watch = Command::new(assert_cmd::cargo_bin!("dispatch"))
        .arg("--state-dir")
        .arg(&p.state)
        .args(["watch", "--json"])
        .current_dir(&p.root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufRead::lines(std::io::BufReader::new(watch.stdout.take().unwrap()));
    let work = loop {
        let line: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        if line["type"] == "work" && line["run_id"] == a.as_str() {
            break line;
        }
    };
    let _ = watch.kill();
    let _ = watch.wait();
    // Valid against the project, and still touching other Work: both hold.
    assert_eq!(work["verdict"], "continue", "{work}");
    assert_eq!(work["interactions"][0]["with"], b.as_str(), "{work}");
    assert_eq!(work["interactions"][0]["direction"], "this_affects_theirs");
}
