//! `dispatch accept --despite-refresh`: a human may apply work the file and
//! symbol analysis refused, only with an explanation and only after the
//! project's checks pass on the merged tree. STOP, a patch that no longer
//! applies and a failing check are never overridable. The agent is a script
//! (run as the `cursor` harness) that adds `admin.py`, which calls
//! `auth.validate(token)`.
#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Output};

use assert_cmd::cargo_bin_cmd;
use serde_json::Value;

const ADMIN: &str =
    "from auth import validate\n\n\ndef require_admin(token):\n    return validate(token)\n";

struct Fixture {
    _temp: tempfile::TempDir,
    source: PathBuf,
    state: PathBuf,
    run_id: String,
}

impl Fixture {
    /// A ready result whose Δ adds `admin.py`. `checks` is the `checks:` block.
    fn new(checks: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let state = temp.path().join("state");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("auth.py"),
            "def validate(token):\n    return token\n",
        )
        .unwrap();
        let agent = temp.path().join("agent");
        fs::write(
            &agent,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'fixture 1.0\\n'; exit 0; fi\ncat > admin.py <<'PY'\n{ADMIN}PY\nprintf '{{\"type\":\"result\",\"model\":\"fixture\"}}\\n'\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            source.join("dispatch.yml"),
            format!(
                "execution:\n  timeout_secs: 600\n{checks}harnesses:\n  cursor:\n    executable: \"{}\"\n",
                agent.display()
            ),
        )
        .unwrap();
        let output = cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&state)
            .arg("run")
            .arg(&source)
            .args(["--task", "Add require_admin.", "--agent", "cursor"])
            .args(["--allow-unsafe-local", "--json"])
            .output()
            .unwrap();
        let result: Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("run failed: {}", String::from_utf8_lossy(&output.stderr)));
        Self {
            _temp: temp,
            source,
            state,
            run_id: result["run_id"].as_str().unwrap().to_owned(),
        }
    }

    fn accept(&self, extra: &[&str]) -> Output {
        cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&self.state)
            .args(["accept", &self.run_id])
            .args(extra)
            .output()
            .unwrap()
    }

    fn metadata(&self) -> Value {
        let path = self
            .state
            .join("runs")
            .join(&self.run_id)
            .join("metadata.json");
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn overridden_events(&self) -> Vec<Value> {
        let db = rusqlite::Connection::open(self.state.join("dispatch.db")).unwrap();
        let mut statement = db
            .prepare("SELECT payload_json FROM events WHERE run_id = ?1 AND event_type = 'coherence.overridden'")
            .unwrap();
        statement
            .query_map([&self.run_id], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
            .collect()
    }
}

// -B: Linux Python would otherwise write __pycache__ into the workspace, and
// that bytecode would become part of the patch (macOS's system Python caches
// bytecode elsewhere).
const IMPORT_CHECK: &str = "checks:\n  verify:\n    - python3 -B -c 'import admin'\n";

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_human_can_apply_an_analysis_refresh_with_an_explanation_after_checks_pass() {
    let f = Fixture::new(IMPORT_CHECK);
    // `validate` gains a required parameter: the call in admin.py is broken
    // by the analysis, yet importing admin still works.
    fs::write(
        f.source.join("auth.py"),
        "def validate(token, ctx):\n    return token\n",
    )
    .unwrap();

    let refused = f.accept(&[]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("def validate(token): => def validate(token, ctx):"),
        "{}",
        stderr(&refused)
    );
    assert!(!f.source.join("admin.py").exists());

    let unexplained = f.accept(&["--despite-refresh"]);
    assert!(!unexplained.status.success());
    assert!(
        stderr(&unexplained).contains("needs --explanation"),
        "{}",
        stderr(&unexplained)
    );
    assert!(!f.source.join("admin.py").exists());

    let why =
        "the router always passes ctx by keyword; this call site is updated in the next change";
    let overridden = f.accept(&["--despite-refresh", "--explanation", why]);
    assert!(overridden.status.success(), "{}", stderr(&overridden));
    assert_eq!(
        fs::read_to_string(f.source.join("admin.py")).unwrap(),
        ADMIN
    );

    let metadata = f.metadata();
    assert_eq!(metadata["outcome"]["application"], "applied");
    assert_eq!(metadata["outcome"]["applied_by"], "human");
    assert_eq!(metadata["outcome"]["review"], "accepted");
    assert_eq!(metadata["coherence"]["overridden"]["decision"], "refresh");
    assert_eq!(metadata["coherence"]["validity"]["analysis"], "integration");
    let events = f.overridden_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["explanation"], why);
    assert_eq!(events[0]["coherence"]["reasons"][0]["code"], "fact_broken");
    let history = cargo_bin_cmd!("dispatch")
        .arg("--state-dir")
        .arg(&f.state)
        .arg("history")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&history.stdout).contains("overridden"),
        "{}",
        String::from_utf8_lossy(&history.stdout)
    );
}

#[test]
fn a_failing_check_on_the_merged_tree_refuses_the_override() {
    let f = Fixture::new(IMPORT_CHECK);
    // `validate` is gone: the analysis says fact_missing, and importing admin
    // fails on the merged tree.
    fs::write(
        f.source.join("auth.py"),
        "def check(token):\n    return token\n",
    )
    .unwrap();

    let output = f.accept(&["--despite-refresh", "--explanation", "I checked"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("python3 -B -c 'import admin'` failed"),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output)
            .contains("it needs the checks to pass on the merged tree, and they did not")
            && !stderr(&output).contains("cannot be overridden"),
        "{}",
        stderr(&output)
    );
    assert!(!f.source.join("admin.py").exists());
    assert_eq!(f.metadata()["outcome"]["review"], "pending");
    assert!(f.overridden_events().is_empty());
}

#[test]
fn stop_is_never_overridable() {
    let f = Fixture::new(IMPORT_CHECK);
    fs::write(f.source.join("admin.py"), ADMIN).unwrap();

    let output = f.accept(&["--despite-refresh", "--explanation", "I checked"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("STOP"), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("cannot be overridden"),
        "{}",
        stderr(&output)
    );
    assert!(f.overridden_events().is_empty());
}

#[test]
fn an_override_without_checks_to_run_is_refused() {
    let f = Fixture::new("");
    fs::write(
        f.source.join("auth.py"),
        "def validate(token, ctx):\n    return token\n",
    )
    .unwrap();

    let output = f.accept(&["--despite-refresh", "--explanation", "I checked"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("none ran"), "{}", stderr(&output));
    assert!(!f.source.join("admin.py").exists());
    assert!(f.overridden_events().is_empty());
}
