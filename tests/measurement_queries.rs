//! The documented queries run read-only against a real state database and
//! give the counts derived by hand from their fixtures: the interaction
//! queries (`docs/queries/interactions-measured.sql`) from
//! `tests/fixtures/measurement/events.jsonl`, and the note queries
//! (`docs/queries/notes-measured.sql`) from `notes.jsonl` beside it.

use std::{collections::BTreeSet, path::Path};

use dispatch::{EventRecord, db::Database};
use rusqlite::{Batch, Connection, OpenFlags, fallible_iterator::FallibleIterator, types::Value};

const QUERIES: &str = include_str!("../docs/queries/interactions-measured.sql");
const EVENTS: &str = include_str!("fixtures/measurement/events.jsonl");
const NOTE_QUERIES: &str = include_str!("../docs/queries/notes-measured.sql");
const NOTE_EVENTS: &str = include_str!("fixtures/measurement/notes.jsonl");

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
fn run_queries(path: &Path, queries: &str) -> Vec<Vec<Vec<String>>> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut batch = Batch::new(&connection, queries);
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
    let sets = run_queries(&state_with(dir.path(), EVENTS), QUERIES);
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
    let sets = run_queries(&state_with(dir.path(), ""), QUERIES);
    assert_eq!(sets, vec![Vec::<Vec<String>>::new(); 5]);
}

/// Derivation from `notes.jsonl` (Works W1 and W2 finished, W3 working):
///
/// Delivered (note: then touched after delivery, S0 moved after it, next
/// verdict after it):
///   W1 n1 10:01: touched 10:05, moved 10:06, checked continue 10:07
///   W1 n2 10:31: touched 10:30:10 and stop 10:30:30 come before it, the move
///                too; the accept gate's refresh 10:40 is next
///   W2 n3 11:01: the touch at 11:02 is n9's; no verdict after it
///   W3 n7 12:05: the failed application's stop at 12:10
/// So continue 1 (touched 1, moved 1), refresh 1, stop 1, none 1; agreed
/// (refresh or stop) 2.
///
/// Neither delivered nor expired: W2's n5 (W2 finished) and W3's n6 (W3 in
/// progress). n4 expired.
#[test]
fn the_note_queries_give_the_hand_derived_counts() {
    let dir = tempfile::tempdir().unwrap();
    let path = state_with(dir.path(), NOTE_EVENTS);
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE runs SET outcome_json = '{\"lifecycle\": \"working\"}' WHERE id = ?1",
            ["01J9NOTEW30000000000000000"],
        )
        .unwrap();
    let sets = run_queries(&path, NOTE_QUERIES);
    assert_eq!(sets.len(), 3);
    assert_eq!(
        sets[0],
        rows(&[
            // run_id, note, via, delivered_at, touched, s0_moved, next_verdict
            "notes_delivered 01J9NOTEW10000000000000000 n1 PostToolUse \
             2026-11-05T10:01:00.000000000Z 1 1 continue",
            "notes_delivered 01J9NOTEW10000000000000000 n2 pre_llm_call \
             2026-11-05T10:31:00.000000000Z 0 0 refresh",
            "notes_delivered 01J9NOTEW20000000000000000 n3 PostToolUse \
             2026-11-05T11:01:00.000000000Z 0 0 none",
            "notes_delivered 01J9NOTEW30000000000000000 n7 SessionStart \
             2026-11-05T12:05:00.000000000Z 0 0 stop",
        ])
    );
    assert_eq!(
        sets[1],
        rows(&[
            // next_verdict, delivered, agreed, touched, s0_moved
            "notes_by_next_verdict continue 1 0 1 1",
            "notes_by_next_verdict refresh  1 1 0 0",
            "notes_by_next_verdict stop     1 1 0 0",
            "notes_by_next_verdict none     1 0 0 0",
        ])
    );
    // unsettled, pending
    assert_eq!(sets[2], rows(&["notes_unsettled 1 1"]));
}

/// Without notes: no rows, and nothing unsettled.
#[test]
fn the_note_queries_report_nothing_without_notes() {
    let dir = tempfile::tempdir().unwrap();
    let sets = run_queries(&state_with(dir.path(), ""), NOTE_QUERIES);
    assert_eq!(sets, [vec![], vec![], rows(&["notes_unsettled 0 0"])]);
}
