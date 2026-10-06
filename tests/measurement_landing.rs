//! The landing observation (0.4.9, `docs/plan-0.4.9.md` §3.2): every
//! successful apply records one `interaction.landed` on the applied run,
//! describing how that Work interacted with the other unintegrated Work just
//! before it landed. It never changes whether or how the apply happens. Each
//! piece of Work here is a linked worktree attached as foreign Work that the
//! test edits directly.
#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};

use serde_json::Value;
use sha2::{Digest, Sha256};

const AUTH: &str = "pub fn validate(token: &str) -> bool {\n    !token.is_empty()\n}\n\n\
pub fn filler_1() {}\npub fn filler_2() {}\npub fn filler_3() {}\npub fn filler_4() {}\n\
pub fn filler_5() {}\npub fn filler_6() {}\npub fn filler_7() {}\npub fn filler_8() {}\n\n\
pub fn refresh(token: &str) -> String {\n    token.to_owned()\n}\n";
const CALLER: &str = "pub fn serve() {}\n\npub fn login(t: &str) -> bool {\n    validate(t)\n}\n";
const POLL: &str = "coherence:\n  poll_secs: 1\n";

fn changed_signature() -> String {
    AUTH.replacen(
        "validate(token: &str)",
        "validate(token: &str, strict: bool)",
        1,
    )
}

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
    fn new(config: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/auth.rs"), AUTH).unwrap();
        fs::write(root.join("src/api.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(root.join("src/cli.rs"), "pub fn main() {}\n").unwrap();
        fs::write(root.join("dispatch.yml"), config).unwrap();
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
    fn work(&self, name: &str, extra: &[&str]) -> (PathBuf, String) {
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
        let mut args = vec![
            "attach",
            "--workspace",
            workspace.to_str().unwrap(),
            "--agent",
            name,
        ];
        args.extend_from_slice(extra);
        let id = self
            .ok(&args)
            .lines()
            .find_map(|line| line.strip_prefix("ATTACHED "))
            .unwrap()
            .to_owned();
        (fs::canonicalize(workspace).unwrap(), id)
    }

    fn interactions_file(&self) -> Option<PathBuf> {
        fs::read_dir(self.state.join("watchers"))
            .ok()?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .find(|path| path.to_string_lossy().ends_with(".interactions.json"))
    }

    fn projection(&self) -> Option<Value> {
        serde_json::from_slice(&fs::read(self.interactions_file()?).ok()?).ok()
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

    /// The SHA-256 of the run's frozen patch, `delta.patch`.
    fn delta_sha256(&self, id: &str) -> String {
        let path = self.run(id)["candidates"][0]["diff_path"]
            .as_str()
            .unwrap()
            .to_owned();
        hex::encode(Sha256::digest(fs::read(path).unwrap()))
    }

    /// The committed events of `id`, through `dispatch events`.
    fn events(&self, id: &str) -> Vec<Value> {
        self.ok(&["events", id])
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|record| record["type"] == "event")
            .map(|record| record["event"].clone())
            .collect()
    }

    /// The payloads of `id`'s `interaction.landed` events.
    fn landings(&self, id: &str) -> Vec<Value> {
        self.events(id)
            .into_iter()
            .filter(|event| event["event_type"] == "interaction.landed")
            .map(|event| event["payload"].clone())
            .collect()
    }

    /// Whether the owner lists `id` with its frozen patch.
    fn lists_frozen(&self, projection: &Value, id: &str) -> bool {
        let sha = self.delta_sha256(id);
        projection["participants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["run_id"] == id && p["delta_sha256"] == sha.as_str())
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = self.dispatch(&["stop"]);
    }
}

fn counterpart<'a>(landing: &'a Value, id: &str) -> &'a Value {
    landing["counterparts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["run_id"] == id)
        .unwrap_or_else(|| panic!("{id} is not listed: {landing}"))
}

#[test]
fn accepted_work_records_how_it_interacted_with_every_other_work() {
    let p = Project::new(POLL);
    let (wa, a) = p.work("wt-a", &[]);
    let (wb, b) = p.work("wt-b", &[]);
    let (wd, d) = p.work("wt-d", &[]);
    fs::write(wa.join("src/auth.rs"), changed_signature()).unwrap();
    fs::write(wb.join("src/api.rs"), CALLER).unwrap();
    fs::write(wd.join("src/cli.rs"), "pub fn main() { }\n").unwrap();
    p.ok(&["finish", &a]);
    p.ok(&["start"]);
    p.until("the owner lists A's frozen patch, and B and D", |v| {
        p.lists_frozen(v, &a) && v["participants"].as_array().unwrap().len() == 3
    });

    p.ok(&["accept", &a]);
    let landings = p.landings(&a);
    assert_eq!(landings.len(), 1, "{landings:?}");
    let landing = &landings[0];
    assert_eq!(landing["status"], "observed", "{landing}");
    assert_eq!(landing["landed"]["run_id"], a.as_str());
    assert_eq!(landing["landed"]["delta_sha256"], p.delta_sha256(&a));
    assert_eq!(landing["landed"]["applied_by"], "human");
    assert_eq!(landing["landed"]["s0"], p.run(&a)["baseline_commit"]);
    assert!(landing["projection_computed_at"].is_string(), "{landing}");
    assert!(landing["world_after"].is_string(), "{landing}");

    let with_b = counterpart(landing, &b);
    assert_eq!(with_b["s0"], p.run(&b)["baseline_commit"], "{with_b}");
    let interactions = with_b["interactions"].as_array().unwrap();
    assert_eq!(interactions.len(), 1, "{with_b}");
    assert_eq!(interactions[0]["rule"], "uses");
    assert_eq!(interactions[0]["writer"], "a");
    assert_eq!(interactions[0]["target"]["name"], "validate");
    // D touches nothing of A's, and is still listed: that is what makes a
    // miss measurable.
    assert_eq!(
        counterpart(landing, &d)["interactions"],
        serde_json::json!([])
    );
    assert_eq!(landing["counterparts"].as_array().unwrap().len(), 2);
}

#[test]
fn unwatched_work_lands_with_an_unwatched_observation() {
    let p = Project::new(POLL);
    let (wa, a) = p.work("wt-a", &[]);
    p.work("wt-b", &[]);
    fs::write(wa.join("src/auth.rs"), changed_signature()).unwrap();
    p.ok(&["finish", &a]);
    p.ok(&["accept", &a]);

    assert_eq!(p.run(&a)["outcome"]["application"], "applied");
    let landings = p.landings(&a);
    assert_eq!(landings.len(), 1, "{landings:?}");
    assert_eq!(landings[0]["status"], "unwatched", "{}", landings[0]);
    assert_eq!(landings[0]["counterparts"], serde_json::json!([]));
    assert_eq!(landings[0]["landed"]["delta_sha256"], p.delta_sha256(&a));
}

#[test]
fn an_unreadable_view_never_stops_the_apply() {
    let p = Project::new(POLL);
    let (wa, a) = p.work("wt-a", &[]);
    p.work("wt-b", &[]);
    fs::write(wa.join("src/auth.rs"), changed_signature()).unwrap();
    p.ok(&["finish", &a]);
    p.ok(&["start"]);
    p.until("the owner lists A's frozen patch", |v| {
        p.lists_frozen(v, &a)
    });
    // The owner rewrites its view only when the view changes, and nothing
    // changes before the accept below.
    fs::write(p.interactions_file().unwrap(), "{ not json").unwrap();

    p.ok(&["accept", &a]);
    let events = p.events(&a);
    assert!(
        events.iter().any(|e| e["event_type"] == "result.applied"),
        "{events:?}"
    );
    let landings = p.landings(&a);
    assert_eq!(landings.len(), 1, "{landings:?}");
    assert_eq!(landings[0]["status"], "unavailable", "{}", landings[0]);
    assert!(landings[0]["detail"].is_string(), "{}", landings[0]);
    assert_eq!(landings[0]["counterparts"], serde_json::json!([]));
}

#[test]
fn auto_applied_work_records_its_landing_as_auto_applied() {
    let p = Project::new("coherence:\n  poll_secs: 1\nchecks:\n  verify: ['true']\n");
    let (wa, a) = p.work("wt-a", &["--auto-apply", "--allow-unsafe-local"]);
    fs::write(wa.join("src/auth.rs"), changed_signature()).unwrap();
    p.ok(&["start"]);
    p.ok(&["finish", &a]);
    // The owner applies it; the landing is committed right after
    // `result.applied`.
    let deadline = Instant::now() + Duration::from_secs(30);
    while p.landings(&a).is_empty() {
        assert!(Instant::now() < deadline, "{}", p.run(&a)["outcome"]);
        std::thread::sleep(Duration::from_millis(200));
    }

    assert_eq!(p.run(&a)["outcome"]["applied_by"], "auto_apply");
    let landings = p.landings(&a);
    assert_eq!(landings.len(), 1, "{landings:?}");
    assert_eq!(landings[0]["landed"]["applied_by"], "auto_apply");
    assert_eq!(landings[0]["landed"]["delta_sha256"], p.delta_sha256(&a));
}

#[test]
fn an_apply_refused_for_drift_records_no_landing() {
    let p = Project::new(POLL);
    let (wa, a) = p.work("wt-a", &[]);
    fs::write(wa.join("src/cli.rs"), "pub fn main() { 1; }\n").unwrap();
    p.ok(&["finish", &a]);
    // The source changes the very line A changed: A's patch no longer applies.
    fs::write(p.root.join("src/cli.rs"), "pub fn main() { 2; }\n").unwrap();
    git(&p.root, &["commit", "--quiet", "-am", "moved"]);

    let refused = p.dispatch(&["accept", &a]);
    assert!(!refused.status.success());
    let events = p.events(&a);
    assert!(
        events
            .iter()
            .any(|e| e["event_type"] == "application.failed"),
        "{events:?}"
    );
    assert!(p.landings(&a).is_empty(), "{events:?}");
    assert_eq!(
        p.run(&a)["outcome"]["application"],
        "blocked_by_source_drift"
    );
}
