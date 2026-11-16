//! The documented queries run read-only against a real state database and
//! give the counts derived by hand from their fixtures: the interaction
//! queries (`docs/queries/interactions-measured.sql`) from
//! `tests/fixtures/measurement/events.jsonl`, the note queries
//! (`docs/queries/notes-measured.sql`) from `notes.jsonl` beside it, and the
//! catch-up and batch queries (`catchup-measured.sql`, `batches-measured.sql`)
//! from `catchup.jsonl` and `batches.jsonl`.

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

const CATCHUP_QUERIES: &str = include_str!("../docs/queries/catchup-measured.sql");
const CATCHUP_EVENTS: &str = include_str!("fixtures/measurement/catchup.jsonl");
const BATCH_QUERIES: &str = include_str!("../docs/queries/batches-measured.sql");
const BATCH_EVENTS: &str = include_str!("fixtures/measurement/batches.jsonl");

/// Derivation from `catchup.jsonl`. L1, L2 and L3 were accepted alone, each
/// source recorded at hh:00:00.200, after its `result.applied`; B1 and B2
/// landed in batch `be`, its source recorded at 11:00:00.500, before either
/// `result.applied`.
///
/// Written 12, one landing each: W2 and W6 twice, every other Work once. W6
/// is asked about L3 again: 1 repeated. Its first ask names a file: 1 file
/// target, and 12 symbols.
/// Delivered 7: stop W1, W3 and W6; turn W5; verify W4; wake W2 (L1 and L2
/// in one ask) and W7, 3 landings. Expired: left_working W8, s0_moved W9.
///
/// Each delivered ask, and the first move forward of its Work's S0 after it
/// to a source holding every landing it names:
///   W1 [L1] 09:10:00: L1's source at 09:15:00, 300 s. The refresh at 09:12
///      came before the move; the continue recorded after it at 09:15:00 is
///      next.
///   W3 [L1] 10:00:31.5: S0 had moved past L1 before; the move after it is
///      back.
///   W2 [L1, L2] 10:05:00: L1's source at 10:06 is older than L2; L2's at
///      10:08:00, 180 s, then refresh.
///   W4 [L2] 10:10:00: L1's source only, then back to where it began.
///   W5 [B1] 11:10:00: the batch's source, named by B2, at 11:12:00, 120 s,
///      then continue.
///   W7 [L3] 12:00:05.002, 2 ms before the owner recorded it written: the
///      root's clean HEAD at 12:20:00, 1194.998 s, rounded to 1195, then
///      continue.
///   W6 [L3] 12:05:00: no move, so its refresh at 12:06 is no next verdict.
///
/// Unsettled: WA's L3, though other Works' deliveries and expiry name it;
/// W6's second L3, written after the delivery that settled its first.
#[test]
fn the_catchup_queries_give_the_hand_derived_counts() {
    let dir = tempfile::tempdir().unwrap();
    let sets = run_queries(&state_with(dir.path(), CATCHUP_EVENTS), CATCHUP_QUERIES);
    assert_eq!(sets.len(), 3);
    assert_eq!(
        sets[0],
        rows(&[
            // kind, detail, events, landings, repeated_landings, symbol_targets,
            // file_targets
            "catchup_counts written   NULL         12 12 1    12   1",
            "catchup_counts delivered stop         3  3  NULL NULL NULL",
            "catchup_counts delivered turn         1  1  NULL NULL NULL",
            "catchup_counts delivered verify       1  1  NULL NULL NULL",
            "catchup_counts delivered wake         2  3  NULL NULL NULL",
            "catchup_counts expired   left_working 1  1  NULL NULL NULL",
            "catchup_counts expired   s0_moved     1  1  NULL NULL NULL",
        ])
    );
    assert_eq!(
        sets[1],
        rows(&[
            // run_id, landings, via, delivered_at, moved_at, seconds, next_verdict
            "catchup_s0 W1 [\"L1\"] stop 2026-11-05T09:10:00.000000000Z \
             2026-11-05T09:15:00.000000000Z 300 continue",
            "catchup_s0 W3 [\"L1\"] stop 2026-11-05T10:00:31.500000000Z NULL NULL NULL",
            "catchup_s0 W2 [\"L1\",\"L2\"] wake 2026-11-05T10:05:00.000000000Z \
             2026-11-05T10:08:00.000000000Z 180 refresh",
            "catchup_s0 W4 [\"L2\"] verify 2026-11-05T10:10:00.000000000Z NULL NULL NULL",
            "catchup_s0 W5 [\"B1\"] turn 2026-11-05T11:10:00.000000000Z \
             2026-11-05T11:12:00.000000000Z 120 continue",
            "catchup_s0 W7 [\"L3\"] wake 2026-11-05T12:00:05.002000000Z \
             2026-11-05T12:20:00.000000000Z 1195 continue",
            "catchup_s0 W6 [\"L3\"] stop 2026-11-05T12:05:00.000000000Z NULL NULL NULL",
        ])
    );
    assert_eq!(
        sets[2],
        rows(&[
            // run_id, written_at, landings
            "catchup_unsettled WA 2026-11-05T12:00:05.000000000Z [\"L3\"]",
            "catchup_unsettled W6 2026-11-05T12:30:00.000000000Z [\"L3\"]",
        ])
    );
}

/// Without asks, empty sets, also beside other events.
#[test]
fn the_catchup_queries_report_nothing_without_catchups() {
    for events in ["", NOTE_EVENTS] {
        let dir = tempfile::tempdir().unwrap();
        let sets = run_queries(&state_with(dir.path(), events), CATCHUP_QUERIES);
        assert_eq!(sets, vec![Vec::<Vec<String>>::new(); 3]);
    }
}

/// Derivation from `batches.jsonl` and the `checks` rows added here.
///
/// Batches: bq proposed P1 and P2 and never landed. bp was offered twice,
/// left X1 and X2 out each time, and landed P1 to P3 on one merged run. bf
/// failed whole (4), passed with its first 2 (a reused pass) and failed
/// with 3: F1 and F2 landed. bz failed whole and with Z1 alone, alike in two
/// accepts, and landed nothing. br reused the whole batch's pass, accepted
/// with no offer recorded; bu has no checks configured.
///
/// Suite runs 17:
/// - 13 merged runs: finish P1 and P2; check S1 (its other check could not
///   build its tree); accept bp 1, bf 2, bz 4, and Z2, S1 and N1 alone;
/// - N1's baseline and verify phases, 2;
/// - the in-place finishes of F2 (two commands) and X1, which never landed,
///   2. P1's and P2's rows are their merged runs' logs, P3's its reuse's,
///   and N1's its phases'.
///
/// Reuses 3: P3's finish, bf's first 2 and br's whole batch; the batches'
/// records for P2, P3, F2 and R2 are not reuses.
/// Landed 13, 9 of them in 4 batches. Approvals 7: one per batch, and Z2, S1
/// and N1 alone; A1 was auto-applied.
#[test]
fn the_batch_queries_give_the_hand_derived_counts() {
    let dir = tempfile::tempdir().unwrap();
    let path = state_with(dir.path(), BATCH_EVENTS);
    let connection = Connection::open(&path).unwrap();
    for (run, name, stdout) in [
        ("P1", "verify 1", "/runs/P1/m0800/verify-01.stdout.log"),
        ("P2", "verify 1", "/runs/P2/m0805/verify-01.stdout.log"),
        ("P3", "verify 1", "/runs/M1/memo/verify-01.stdout.log"),
        ("X1", "verify 1", "/runs/X1/checks/verify-01.stdout.log"),
        ("F2", "verify 1", "/runs/F2/checks/verify-01.stdout.log"),
        ("F2", "verify 2", "/runs/F2/checks/verify-02.stdout.log"),
        ("N1", "verify 1", "/runs/N1/checks/verify-01.stdout.log"),
    ] {
        connection
            .execute(
                "INSERT INTO checks(run_id, name, phase, command, status, exit_code, \
                 duration_ms, stdout_path, stderr_path) \
                 VALUES (?1, ?2, 'verify', 'cargo test', 'passed', 0, 1, ?3, '/stderr')",
                [run, name, stdout],
            )
            .unwrap();
    }
    let sets = run_queries(&path, BATCH_QUERIES);
    assert_eq!(sets.len(), 3);
    let left_out = |run: &str, code: &str| {
        format!(
            "batch_members bp left_out {run} NULL refresh \
             [{{\"code\":\"{code}\",\"detail\":\"x\",\"fact_id\":null,\"path\":null}}]"
        )
    };
    assert_eq!(
        sets[0],
        rows(&[
            // batch, kind, run_id, position, decision, why
            "batch_members bf proposed F1 1 NULL first",
            "batch_members bf proposed F2 2 NULL second",
            "batch_members bf proposed F3 3 NULL third",
            "batch_members bf proposed F4 4 NULL fourth",
            "batch_members bf landed   F1 1 NULL NULL",
            "batch_members bf landed   F2 2 NULL NULL",
            "batch_members bp proposed P1 1 NULL first",
            "batch_members bp proposed P2 2 NULL second",
            "batch_members bp proposed P3 3 NULL third",
            &left_out("X1", "patch_conflict"),
            &left_out("X2", "fact_broken"),
            "batch_members bp landed   P1 1 NULL NULL",
            "batch_members bp landed   P2 2 NULL NULL",
            "batch_members bp landed   P3 3 NULL NULL",
            "batch_members bq proposed P1 1 NULL first",
            "batch_members bq proposed P2 2 NULL second",
            "batch_members br landed   R1 1 NULL NULL",
            "batch_members br landed   R2 2 NULL NULL",
            "batch_members bu landed   U1 1 NULL NULL",
            "batch_members bu landed   U2 2 NULL NULL",
            "batch_members bz proposed Z1 1 NULL first",
            "batch_members bz proposed Z2 2 NULL second",
        ])
    );
    assert_eq!(
        sets[1],
        rows(&[
            // batch, passed, lands, failed, verifications, full_runs, bisection_runs
            "batch_check_runs bp 1    3 NULL 1 1 0",
            "batch_check_runs bf 0    2 F3   1 1 1",
            "batch_check_runs bz 0    0 Z1   2 2 2",
            "batch_check_runs br 1    2 NULL 1 0 0",
            "batch_check_runs bu NULL 2 NULL 1 0 0",
        ])
    );
    // landed_works, in_batches, batches, approvals, suite_runs, reuses
    assert_eq!(sets[2], rows(&["landing_counts 13 9 4 7 17 3"]));
}

/// Without batches or landings, empty sets, also beside other events.
#[test]
fn the_batch_queries_report_nothing_without_batches_or_accepts() {
    for events in ["", EVENTS] {
        let dir = tempfile::tempdir().unwrap();
        let sets = run_queries(&state_with(dir.path(), events), BATCH_QUERIES);
        assert_eq!(sets, vec![Vec::<Vec<String>>::new(); 3]);
    }
}
