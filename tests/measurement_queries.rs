//! The documented interaction queries (`docs/queries/interactions-measured.sql`)
//! run read-only against a real state database and give the counts derived by
//! hand from `tests/fixtures/measurement/events.jsonl`.

use std::{collections::BTreeSet, path::Path};

use dispatch::{EventRecord, db::Database};
use rusqlite::{Batch, Connection, OpenFlags, fallible_iterator::FallibleIterator, types::Value};

const QUERIES: &str = include_str!("../docs/queries/interactions-measured.sql");
const EVENTS: &str = include_str!("fixtures/measurement/events.jsonl");

/// A state database with the real schema, holding `events` and a minimal run
/// row for each run they are recorded on.
fn state_with(dir: &Path, events: &str) -> std::path::PathBuf {
    let path = dir.join("dispatch.db");
    let database = Database::open(&path).unwrap();
    let records: Vec<EventRecord> = events
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();

    let runs = Connection::open(&path).unwrap();
    runs.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    runs.execute(
        "INSERT INTO sources(id, path, kind, fingerprint, created_at) \
         VALUES (1, '/source', 'directory', 'fingerprint', '2026-10-06T00:00:00Z')",
        [],
    )
    .unwrap();
    let run_ids: BTreeSet<&str> = records.iter().map(|event| event.run_id.as_str()).collect();
    for run_id in run_ids {
        runs.execute(
            "INSERT INTO runs(\
                id, source_id, task, exact_prompt, baseline_path, baseline_commit, status, \
                created_at, dispatch_version, os, architecture, execution_backend, \
                timeout_secs, cpus, memory, max_parallel, outcome_json\
            ) VALUES (\
                ?1, 1, 'task', 'task', '/baseline', 'commit', 'applied', \
                '2026-10-06T00:00:00Z', '0.4.9', 'test', 'test', 'local', 30, 1.0, '1g', 1, '{}'\
            )",
            [run_id],
        )
        .unwrap();
    }
    drop(runs);

    for record in &records {
        database.record_event(record).unwrap();
    }
    path
}

/// Each statement's rows, as text, from a read-only connection: NULL for
/// SQL NULL, and reals in Rust's debug form (`0.5`, `1.0`).
fn run_queries(path: &Path) -> Vec<Vec<Vec<String>>> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut batch = Batch::new(&connection, QUERIES);
    let mut sets = Vec::new();
    while let Some(mut statement) = batch.next().unwrap() {
        assert!(statement.readonly(), "{:?}", statement.expanded_sql());
        let columns = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                (0..columns)
                    .map(|i| {
                        Ok(match row.get::<_, Value>(i)? {
                            Value::Null => "NULL".to_string(),
                            Value::Integer(n) => n.to_string(),
                            Value::Real(r) => format!("{r:?}"),
                            Value::Text(text) => text,
                            Value::Blob(_) => panic!("unexpected blob"),
                        })
                    })
                    .collect::<rusqlite::Result<Vec<String>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        sets.push(rows);
    }
    sets
}

/// Expected rows, one line each, values separated by whitespace.
fn rows(expected: &[&str]) -> Vec<Vec<String>> {
    expected
        .iter()
        .map(|row| row.split_whitespace().map(str::to_string).collect())
        .collect()
}

/// Derivation from `events.jsonl` (runs shortened to their distinct letters):
///
/// Landings (status: counterparts, with their interaction rules):
///   TA1 observed:    B1 [uses], C1 [], D1 [same_declaration], E1 []  -> 4
///   TA2 unwatched:   none                                            -> 0
///   TA3 stale:       F1 [uses]                                       -> 1
///   TA4 observed:    G1 [textual_overlap]                            -> 1
///   TA5 unavailable: none                                            -> 0
///   TA6 observed:    H1 [], I1 [file], J1 []                         -> 3
/// So observed 3 landings / 8 counterparts, the rest 1 landing each.
///
/// Outcomes (counterpart: class, predicted, decision):
///   TA1/B1 scorable           true  refresh  -> predicted, invalidated
///   TA1/C1 scorable           false refresh  -> a miss
///   TA1/D1 scorable           true  continue -> predicted, valid
///   TA1/E1 already_invalid    false refresh  -> not scored
///   TA3/F1 landing_unobserved true  refresh  -> not scored
///   TA6/H1 scorable           false continue -> unpredicted, valid
///   TA6/I1 world_moved_on     true  refresh  -> not scored
///   TA6/J1 counterpart_gone   false null     -> not scored
/// So by class: scorable 4, and already_invalid, counterpart_gone,
/// landing_unobserved and world_moved_on 1 each.
///
/// Scorable 2x2: B1 predicted+invalidated, D1 predicted+valid, C1
/// unpredicted+invalidated, H1 unpredicted+valid: 1 each. Predicted 2, so
/// precision = 1 / 2 = 0.5; misses = 1 (C1).
///
/// By rule, scorable and predicted only: uses (B1, refresh) 1 of 1 -> 1.0;
/// same_declaration (D1, continue) 0 of 1 -> 0.0. F1 and I1 are not
/// scorable, and G1 has no outcome, so file and textual_overlap have no row.
///
/// Without outcomes: every counterpart of TA1, TA3 and TA6 has an outcome;
/// TA2 and TA5 list none; TA4's only counterpart, G1, has none.
#[test]
fn the_documented_queries_give_the_hand_derived_counts() {
    let dir = tempfile::tempdir().unwrap();
    let sets = run_queries(&state_with(dir.path(), EVENTS));
    assert_eq!(sets.len(), 5);

    assert_eq!(
        sets[0],
        rows(&[
            // status, landings, counterparts
            "landings_by_status observed    3 8",
            "landings_by_status stale       1 1",
            "landings_by_status unavailable 1 0",
            "landings_by_status unwatched   1 0",
        ])
    );
    assert_eq!(
        sets[1],
        rows(&[
            // classifier_version, class, outcomes
            "outcomes_by_class 1 already_invalid    1",
            "outcomes_by_class 1 counterpart_gone   1",
            "outcomes_by_class 1 landing_unobserved 1",
            "outcomes_by_class 1 scorable           4",
            "outcomes_by_class 1 world_moved_on     1",
        ])
    );
    assert_eq!(
        sets[2],
        rows(&[
            // classifier_version, scorable, predicted_invalidated, predicted_valid,
            // unpredicted_invalidated, unpredicted_valid, predicted, precision, misses
            "scorable_predicted_by_invalidated 1 4 1 1 1 1 2 0.5 1",
        ])
    );
    assert_eq!(
        sets[3],
        rows(&[
            // classifier_version, rule, predicted, predicted_invalidated,
            // predicted_valid, precision
            "scorable_predicted_by_rule 1 same_declaration 1 0 1 0.0",
            "scorable_predicted_by_rule 1 uses             1 1 0 1.0",
        ])
    );
    assert_eq!(
        sets[4],
        rows(&[
            // landed_run_id, status, landed_at, counterparts, without_outcome,
            // missing_counterparts
            "landings_without_outcomes 01J9EVTA400000000000000000 observed \
             2026-10-06T10:30:00.000000000Z 1 1 01J9EVG1000000000000000000",
        ])
    );
}

/// A state with no measurement events gives five empty result sets, not an
/// error and not a zero precision.
#[test]
fn the_documented_queries_report_nothing_without_measurements() {
    let dir = tempfile::tempdir().unwrap();
    let sets = run_queries(&state_with(dir.path(), ""));
    assert_eq!(sets, vec![Vec::<Vec<String>>::new(); 5]);
}
