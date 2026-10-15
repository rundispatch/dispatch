//! Runs created before coherence existed (0.1.x) have no `coherence` key in
//! `metadata.json` or in `runs.run_projection_json`. Coherence derives from the
//! run's baseline and patch, so such a run must work under the current binary
//! with no migration and no backfill. Stripping the key from a run the current
//! code just produced reproduces what a 0.1.3-rc.1 run looks like on disk
//! (schema 21, so the "no migration" assertions hold). A state directory made
//! by 0.1.2 is at schema 11 and does migrate, with a `dispatch.schema-11-*`
//! backup; that path was checked by hand and is described in the WP10b report.
#![cfg(unix)]

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::cargo_bin_cmd;
use serde_json::Value;

struct Fixture {
    _temp: tempfile::TempDir,
    source: PathBuf,
    state: PathBuf,
    run_id: String,
}

impl Fixture {
    /// A Ready `fake-good` run (it creates one empty file, `dispatch-fake-good.txt`)
    /// whose stored records have been stripped of any coherence data.
    fn pre_coherence() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let state = temp.path().join("state");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("original.txt"), "baseline\n").unwrap();
        fs::write(
            source.join("dispatch.yml"),
            "execution:\n  timeout_secs: 600\n",
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
        let run_id = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("RUN "))
            .expect("run output includes an ID")
            .to_owned();
        Self {
            _temp: temp,
            source: fs::canonicalize(source).unwrap(),
            state,
            run_id,
        }
    }

    fn metadata_path(&self) -> PathBuf {
        self.state
            .join("runs")
            .join(&self.run_id)
            .join("metadata.json")
    }

    fn metadata(&self) -> Value {
        serde_json::from_slice(&fs::read(self.metadata_path()).unwrap()).unwrap()
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.state.join("dispatch.db")).unwrap()
    }

    /// Remove the `coherence` key from both stored copies of the run.
    fn strip_coherence(&self) {
        let mut metadata = self.metadata();
        assert!(
            metadata
                .as_object_mut()
                .unwrap()
                .remove("coherence")
                .is_some()
        );
        fs::write(self.metadata_path(), serde_json::to_vec(&metadata).unwrap()).unwrap();
        let db = self.db();
        let projection: String = db
            .query_row(
                "SELECT run_projection_json FROM runs WHERE id = ?1",
                [&self.run_id],
                |row| row.get(0),
            )
            .unwrap();
        let mut projection: Value = serde_json::from_str(&projection).unwrap();
        assert!(
            projection
                .as_object_mut()
                .unwrap()
                .remove("coherence")
                .is_some()
        );
        let changed = db
            .execute(
                "UPDATE runs SET run_projection_json = ?1 WHERE id = ?2",
                (serde_json::to_string(&projection).unwrap(), &self.run_id),
            )
            .unwrap();
        assert_eq!(changed, 1);
        assert!(self.metadata().get("coherence").is_none());
    }

    /// What the schema-11 to 20 migration leaves for a run made by 0.1.2, which
    /// stored no outcome: the run finished, but its work result reads `pending`.
    fn mark_work_result_pending(&self) {
        let mut metadata = self.metadata();
        metadata["outcome"]["work_result"] = "pending".into();
        fs::write(self.metadata_path(), serde_json::to_vec(&metadata).unwrap()).unwrap();
        let db = self.db();
        let (outcome, projection): (String, String) = db
            .query_row(
                "SELECT outcome_json, run_projection_json FROM runs WHERE id = ?1",
                [&self.run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let (mut outcome, mut projection): (Value, Value) = (
            serde_json::from_str(&outcome).unwrap(),
            serde_json::from_str(&projection).unwrap(),
        );
        outcome["work_result"] = "pending".into();
        projection["outcome"]["work_result"] = "pending".into();
        db.execute(
            "UPDATE runs SET outcome_json = ?1, run_projection_json = ?2 WHERE id = ?3",
            (outcome.to_string(), projection.to_string(), &self.run_id),
        )
        .unwrap();
    }

    fn schema_versions(&self) -> Vec<i64> {
        let db = self.db();
        let mut statement = db
            .prepare("SELECT version FROM schema_migrations ORDER BY version")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn state_entries(&self) -> BTreeSet<String> {
        fs::read_dir(&self.state)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    fn dispatch(&self, args: &[&str]) -> assert_cmd::assert::Assert {
        cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&self.state)
            .current_dir(&self.source)
            .args(args)
            .assert()
    }

    fn stdout(assert: &assert_cmd::assert::Assert) -> String {
        String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
    }

    fn stderr(assert: &assert_cmd::assert::Assert) -> String {
        String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
    }

    fn source_files(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        collect(&self.source, &self.source, &mut out);
        out
    }
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect(root, &path, out);
        } else {
            out.push((
                path.strip_prefix(root).unwrap().to_owned(),
                fs::read(&path).unwrap(),
            ));
        }
    }
}

#[test]
fn a_run_without_coherence_data_reads_as_coherence_capable_with_no_migration() {
    let fixture = Fixture::pre_coherence();
    let entries_before = fixture.state_entries();
    let versions_before = fixture.schema_versions();
    assert_eq!(
        versions_before.last(),
        Some(&current_schema()),
        "{versions_before:?}"
    );
    fixture.strip_coherence();
    let source_before = fixture.source_files();

    // status: a quiet source has no coherence object; the text says nothing moved.
    let status = fixture.dispatch(&["status", "--json"]).success();
    let status: Value = serde_json::from_str(Fixture::stdout(&status).trim()).unwrap();
    assert_eq!(status["run_id"], fixture.run_id.as_str());
    assert!(status.get("coherence").is_none(), "{status}");
    let text = Fixture::stdout(&fixture.dispatch(&["status"]).success());
    assert!(
        text.contains("Coherence: CONTINUE — world unchanged"),
        "{text}"
    );

    // check: evaluated from the baseline and patch, with no stored verdict.
    let check = Fixture::stdout(&fixture.dispatch(&["check", &fixture.run_id]).success());
    assert!(check.contains("Coherence: CONTINUE"), "{check}");
    assert!(
        check.contains(&format!("Next: dispatch accept {}", fixture.run_id)),
        "{check}"
    );

    // Someone else edits a file the patch does not touch, then writes the very
    // file the patch creates: both are judged from the run's own baseline.
    fs::write(fixture.source.join("notes.txt"), "someone else\n").unwrap();
    let check = fixture
        .dispatch(&["check", &fixture.run_id, "--json"])
        .success();
    let check: Value = serde_json::from_str(Fixture::stdout(&check).trim()).unwrap();
    assert_eq!(check["validity"]["decision"], "continue", "{check}");
    assert_eq!(check["validity"]["world_changed"], true);
    assert_eq!(check["validity"]["changed_files"], 1);
    let moved = fixture
        .dispatch(&["status", &fixture.run_id, "--json"])
        .success();
    let moved: Value = serde_json::from_str(Fixture::stdout(&moved).trim()).unwrap();
    assert_eq!(moved["coherence"]["decision"], "continue", "{moved}");

    fs::write(
        fixture.source.join("dispatch-fake-good.txt"),
        "someone else wrote this first\n",
    )
    .unwrap();
    let check = fixture
        .dispatch(&["check", &fixture.run_id, "--json"])
        .success();
    let check: Value = serde_json::from_str(Fixture::stdout(&check).trim()).unwrap();
    assert_eq!(check["validity"]["decision"], "refresh", "{check}");
    assert_eq!(check["validity"]["reasons"][0]["code"], "patch_conflict");

    // A blocked accept stores the verdict on the old run; explain then shows it.
    let blocked = fixture.dispatch(&["accept", &fixture.run_id]).failure();
    assert!(Fixture::stderr(&blocked).contains("stale"));
    let explain = Fixture::stdout(&fixture.dispatch(&["explain", &fixture.run_id]).success());
    assert!(explain.contains("\nCoherence\n"), "{explain}");
    assert!(explain.contains("  decision: REFRESH"), "{explain}");
    assert!(explain.contains("  reason: patch_conflict: "), "{explain}");
    assert!(explain.contains("  first invalid at: "), "{explain}");

    // Only the two files the test wrote differ; nothing was applied.
    let mut expected = source_before;
    expected.push((
        PathBuf::from("dispatch-fake-good.txt"),
        b"someone else wrote this first\n".to_vec(),
    ));
    expected.push((PathBuf::from("notes.txt"), b"someone else\n".to_vec()));
    expected.sort();
    let mut actual = fixture.source_files();
    actual.sort();
    assert_eq!(actual, expected);

    // No migration ran: same schema, no backup file, no new state entries
    // (`locks/` is the per-operation lock directory that `apply` creates).
    assert_eq!(fixture.schema_versions(), versions_before);
    let mut entries = fixture.state_entries();
    entries.remove("locks");
    assert_eq!(entries, entries_before);
    assert!(
        fixture
            .state_entries()
            .iter()
            .all(|name| !name.starts_with("dispatch.schema-")),
        "{:?}",
        fixture.state_entries()
    );
}

#[test]
fn a_run_without_coherence_data_applies_after_an_unrelated_edit() {
    let fixture = Fixture::pre_coherence();
    fixture.strip_coherence();
    fs::write(fixture.source.join("notes.txt"), "someone else's work\n").unwrap();
    fs::write(fixture.source.join("original.txt"), "edited elsewhere\n").unwrap();

    fixture.dispatch(&["accept", &fixture.run_id]).success();

    assert!(fixture.source.join("dispatch-fake-good.txt").is_file());
    assert_eq!(
        fs::read_to_string(fixture.source.join("original.txt")).unwrap(),
        "edited elsewhere\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.source.join("notes.txt")).unwrap(),
        "someone else's work\n"
    );
    assert_eq!(fixture.metadata()["status"], "applied");
    assert_eq!(fixture.schema_versions().last(), Some(&current_schema()));
    assert!(
        fixture
            .state_entries()
            .iter()
            .all(|name| !name.starts_with("dispatch.schema-"))
    );
}

#[test]
fn a_run_without_coherence_data_is_blocked_as_stale_by_a_conflicting_edit() {
    let fixture = Fixture::pre_coherence();
    fixture.strip_coherence();
    let occupied = fixture.source.join("dispatch-fake-good.txt");
    fs::write(&occupied, "someone else wrote this first\n").unwrap();
    let before = fixture.source_files();

    let blocked = fixture.dispatch(&["accept", &fixture.run_id]).failure();

    let stderr = Fixture::stderr(&blocked);
    assert!(stderr.contains("stale"), "{stderr}");
    assert!(
        stderr.contains(&format!("dispatch refresh {}", fixture.run_id)),
        "{stderr}"
    );
    assert_eq!(fixture.source_files(), before, "the source is untouched");
    let metadata = fixture.metadata();
    assert_ne!(metadata["status"], "applied");
    assert_eq!(
        metadata["outcome"]["application"],
        "blocked_by_source_drift"
    );
    assert_eq!(metadata["coherence"]["validity"]["decision"], "refresh");
    assert_eq!(fixture.schema_versions().last(), Some(&current_schema()));
}

// Found by the manual v0.1.2 upgrade check (WP10b): a run made by 0.1.2 has
// `outcome.work_result == "pending"` after migration, and `check`, `refresh`
// and the live `Coherence:` line in `status` all require `ready`, so they refuse
// it ("not a ready, unapplied result") although `apply` handles it correctly.
#[test]
fn a_migrated_v012_run_can_be_checked() {
    let fixture = Fixture::pre_coherence();
    fixture.strip_coherence();
    fixture.mark_work_result_pending();
    fs::write(fixture.source.join("notes.txt"), "someone else\n").unwrap();

    let check = fixture.dispatch(&["check", &fixture.run_id]).success();

    assert!(
        Fixture::stdout(&check).contains("Coherence: CONTINUE"),
        "{}",
        Fixture::stdout(&check)
    );
}

// `runs.run_mode` accepts `'native'` and `'attached'`, and
// `RunRecord.attachment` is optional. The rebuilds of `runs` themselves
// (migrations 21 and 24, with child rows referencing the migrated run) are
// proven in `src/db.rs`'s own migration-fixture tests, which have direct
// access to the private `MIGRATIONS` array this crate's tests cannot reach.
// These two tests cover what is reachable from here: a run this binary
// produces has no `attachment` in its stored record, and the CHECK really
// does accept and round-trip `run_mode = 'attached'`.
#[test]
fn a_run_this_binary_produces_has_no_attachment() {
    let fixture = Fixture::pre_coherence();
    assert_eq!(fixture.schema_versions().last(), Some(&current_schema()));
    assert!(fixture.metadata().get("attachment").is_none());
    let projection: String = fixture
        .db()
        .query_row(
            "SELECT run_projection_json FROM runs WHERE id = ?1",
            [&fixture.run_id],
            |row| row.get(0),
        )
        .unwrap();
    let projection: Value = serde_json::from_str(&projection).unwrap();
    assert!(projection.get("attachment").is_none(), "{projection}");
}

#[test]
fn a_run_mode_attached_row_can_be_inserted_after_migration_and_read_back() {
    let fixture = Fixture::pre_coherence();
    let db = fixture.db();
    let source_id: i64 = db
        .query_row(
            "SELECT source_id FROM runs WHERE id = ?1",
            [&fixture.run_id],
            |row| row.get(0),
        )
        .unwrap();
    db.execute(
        "INSERT INTO runs(\
            id, source_id, task, exact_prompt, baseline_path, baseline_commit, status, \
            created_at, dispatch_version, os, architecture, execution_backend, \
            timeout_secs, cpus, memory, max_parallel, run_mode, outcome_json\
        ) VALUES (\
            'attached-fixture-run', ?1, 'attached task', 'attached task', '/baseline', \
            'commit', 'running', '2026-09-22T00:00:00Z', '0.4.0', 'test', 'test', 'local', \
            30, 1.0, '1g', 1, 'attached', '{}'\
        )",
        [source_id],
    )
    .unwrap();
    let stored_mode: String = db
        .query_row(
            "SELECT run_mode FROM runs WHERE id = 'attached-fixture-run'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_mode, "attached");
}

/// The schema version this binary creates; a run it produced needs no migration.
fn current_schema() -> i64 {
    dispatch::db::Database::open_in_memory()
        .unwrap()
        .schema_version()
        .unwrap()
}
