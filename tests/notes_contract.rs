//! What a note says, tested against its contract rather than its code: which
//! interactions and landings earn a note, one note per key, at most one note
//! per Work per 10 minutes with nothing dropped, plain fixed text, and no I/O.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, TimeZone, Utc};
use dispatch::coherence::{
    interactions::{
        Analysis, Change, Edge, Interaction, Participant, Projection, Rule, Side, Target,
    },
    notes::{self, Landing, Note, Sent},
};

const WORK: &str = "01M6P8A133GBFFAF40WPY89Z2K";
const OTHER: &str = "01M6Q2RS7TXBC4D5E6F7G8H9JK";
const THIRD: &str = "01M6R3MN4PQ5R6S7T8V9W0XYZA";
const LANDED: &str = "01M6S4AB5CD6EF7GH8JK9MN0PQ";
const SHA: &str = "4f1c2a9be07d35e6c8a1f0b2d4e6a8c0e2f4a6b8";

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 5, 9, 0, 0).unwrap()
}

/// The one call into the module, so only names change if its API does.
fn compose(
    work: &str,
    projection: &Projection,
    landings: &[Landing],
    sent: &Sent,
    now: DateTime<Utc>,
) -> (Vec<Note>, Sent) {
    let runtimes = BTreeMap::from([
        (OTHER.into(), "codex".into()),
        (THIRD.into(), "hermes".into()),
    ]);
    notes::compose(work, projection, &runtimes, landings, sent, now)
}

fn symbol(path: &str, name: &str) -> Target {
    Target::Symbol {
        path: path.into(),
        name: name.into(),
    }
}

fn file(path: &str) -> Target {
    Target::File { path: path.into() }
}

fn interaction(
    rule: Rule,
    writer: Option<Side>,
    target: Target,
    change: Option<Change>,
) -> Interaction {
    Interaction {
        rule,
        writer,
        target,
        change,
        lines: (rule == Rule::TextualOverlap).then_some((12, 18)),
    }
}

fn same_declaration() -> Interaction {
    interaction(
        Rule::SameDeclaration,
        None,
        symbol("src/auth.rs", "validate"),
        None,
    )
}

/// The other Work changes the signature of a declaration `WORK` uses. Seen
/// from `WORK`'s side, as `Projection::of` and `explain` see it.
fn uses_theirs() -> Interaction {
    interaction(
        Rule::Uses,
        Some(Side::B),
        symbol("src/auth.rs", "validate"),
        Some(Change::Signature),
    )
}

/// `WORK` removes a declaration the other Work uses.
fn uses_ours() -> Interaction {
    interaction(
        Rule::Uses,
        Some(Side::A),
        symbol("src/api.rs", "serve"),
        Some(Change::Removes),
    )
}

fn textual() -> Interaction {
    interaction(Rule::TextualOverlap, None, file("src/config.py"), None)
}

fn whole_file() -> Interaction {
    interaction(Rule::File, None, file("docs/notes.md"), None)
}

fn participant(run_id: &str) -> Participant {
    Participant {
        run_id: run_id.into(),
        delta_sha256: format!("sha-of-{run_id}"),
        analysis: Analysis::Analyzed,
        unresolved: 0,
    }
}

/// A projection of `WORK`, `OTHER` and `THIRD` with these edges, each given
/// from `a`'s side.
fn projection(edges: Vec<(&str, &str, Vec<Interaction>)>) -> Projection {
    Projection {
        version: 1,
        computed_at: t0(),
        participants: [WORK, OTHER, THIRD, LANDED].map(participant).to_vec(),
        edges: edges
            .into_iter()
            .map(|(a, b, interactions)| Edge {
                a: a.into(),
                b: b.into(),
                interactions,
            })
            .collect(),
    }
}

fn empty() -> Projection {
    projection(Vec::new())
}

fn landing(interactions: Vec<Interaction>) -> Landing {
    Landing {
        run_id: LANDED.into(),
        runtime: "claude".into(),
        interactions,
        commit: SHA.into(),
        signatures: BTreeMap::new(),
    }
}

fn first(work: &str, projection: &Projection, landings: &[Landing]) -> Vec<Note> {
    compose(work, projection, landings, &Sent::default(), t0()).0
}

fn only(notes: &[Note]) -> &Note {
    assert_eq!(notes.len(), 1, "expected exactly one note: {notes:#?}");
    &notes[0]
}

fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap()
}

// Eligibility.

#[test]
fn same_declaration_uses_and_textual_overlap_each_earn_a_note() {
    for found in [same_declaration(), uses_theirs(), uses_ours(), textual()] {
        let view = projection(vec![(WORK, OTHER, vec![found.clone()])]);
        let note = only(&first(WORK, &view, &[])).text.clone();
        assert!(note.contains(found.target.path()), "{found:?}: {note}");
    }
}

#[test]
fn the_other_side_of_an_edge_hears_of_it_too() {
    let view = projection(vec![(OTHER, WORK, vec![uses_theirs()])]);
    assert!(only(&first(WORK, &view, &[])).text.contains("src/auth.rs"));
    assert!(only(&first(OTHER, &view, &[])).text.contains("src/auth.rs"));
}

#[test]
fn file_level_interactions_never_earn_a_note() {
    let view = projection(vec![(WORK, OTHER, vec![whole_file()])]);
    assert!(first(WORK, &view, &[]).is_empty());
    for change in [
        Some(Change::Changes),
        Some(Change::Deletes),
        Some(Change::Adds),
    ] {
        let one_sided = interaction(Rule::File, Some(Side::B), file("docs/notes.md"), change);
        let view = projection(vec![(WORK, OTHER, vec![one_sided.clone()])]);
        assert!(first(WORK, &view, &[]).is_empty(), "{one_sided:?}");
        assert!(first(WORK, &empty(), &[landing(vec![one_sided])]).is_empty());
    }
}

#[test]
fn a_file_level_fact_beside_a_declaration_is_left_out_of_the_note() {
    let view = projection(vec![(WORK, OTHER, vec![whole_file(), same_declaration()])]);
    let text = only(&first(WORK, &view, &[])).text.clone();
    assert!(text.contains("validate"), "{text}");
    assert!(!text.contains("docs/notes.md"), "{text}");

    let text = only(&first(
        WORK,
        &empty(),
        &[landing(vec![whole_file(), uses_theirs()])],
    ))
    .text
    .clone();
    assert!(text.contains("validate"), "{text}");
    assert!(!text.contains("docs/notes.md"), "{text}");
}

#[test]
fn edges_between_other_work_are_not_this_works_business() {
    let view = projection(vec![(OTHER, THIRD, vec![same_declaration()])]);
    assert!(first(WORK, &view, &[]).is_empty());
}

#[test]
fn a_landing_earns_a_note_for_each_eligible_rule() {
    for found in [same_declaration(), uses_theirs(), textual()] {
        let note = only(&first(WORK, &empty(), &[landing(vec![found.clone()])]))
            .text
            .clone();
        assert!(note.contains(found.target.path()), "{found:?}: {note}");
    }
}

#[test]
fn a_landing_with_no_interaction_earns_no_note() {
    assert!(first(WORK, &empty(), &[landing(Vec::new())]).is_empty());
}

// Text.

#[test]
fn an_item_names_the_other_work_by_short_id_and_runtime_the_target_and_the_change() {
    let text = only(&first(WORK, &empty(), &[landing(vec![uses_theirs()])]))
        .text
        .clone();
    assert!(text.contains(&LANDED[..8]), "{text}");
    assert!(
        !text.contains(LANDED),
        "a short id, not the whole one: {text}"
    );
    assert!(text.contains("claude"), "{text}");
    assert!(text.contains("src/auth.rs"), "{text}");
    assert!(text.contains("validate"), "{text}");
    assert!(text.contains("signature"), "{text}");

    let view = projection(vec![(WORK, OTHER, vec![same_declaration()])]);
    let text = only(&first(WORK, &view, &[])).text.clone();
    assert!(text.contains(&OTHER[..8]), "{text}");
    assert!(
        !text.contains(OTHER),
        "a short id, not the whole one: {text}"
    );
}

#[test]
fn a_landing_note_names_the_recorded_commit() {
    let text = only(&first(WORK, &empty(), &[landing(vec![same_declaration()])]))
        .text
        .clone();
    let line = text
        .lines()
        .find(|line| line.starts_with("The source with this change is commit "))
        .unwrap_or_else(|| panic!("no commit line: {text}"));
    let short = line
        .strip_prefix("The source with this change is commit ")
        .and_then(|rest| rest.strip_suffix(" (refs/dispatch/source)."))
        .unwrap_or_else(|| panic!("{line}"));
    assert!(short.len() >= 7 && short.len() < SHA.len(), "{line}");
    assert!(SHA.starts_with(short), "{line}");
}

#[test]
fn every_note_opens_with_one_fixed_line_naming_dispatch() {
    let notes = [
        first(
            WORK,
            &projection(vec![(WORK, OTHER, vec![same_declaration()])]),
            &[],
        ),
        first(WORK, &projection(vec![(WORK, THIRD, vec![textual()])]), &[]),
        first(WORK, &empty(), &[landing(vec![uses_theirs()])]),
    ];
    let heads: Vec<&str> = notes
        .iter()
        .map(|n| only(n).text.lines().next().unwrap())
        .collect();
    assert!(heads[0].contains("Dispatch"), "{}", heads[0]);
    assert!(heads.iter().all(|head| *head == heads[0]), "{heads:?}");
}

#[test]
fn each_item_takes_at_most_three_lines_and_gives_no_instructions() {
    let all = vec![same_declaration(), uses_theirs(), uses_ours(), textual()];
    let landing_items = vec![same_declaration(), uses_theirs(), textual()];
    let mut notes = Vec::new();
    for found in &all {
        let view = projection(vec![(WORK, OTHER, vec![found.clone()])]);
        notes.push((1, only(&first(WORK, &view, &[])).clone()));
    }
    for found in &landing_items {
        notes.push((
            1,
            only(&first(WORK, &empty(), &[landing(vec![found.clone()])])).clone(),
        ));
    }
    let view = projection(vec![(WORK, OTHER, all.clone())]);
    notes.push((
        all.len() + landing_items.len(),
        only(&first(WORK, &view, &[landing(landing_items)])).clone(),
    ));
    for (items, note) in notes {
        let lines = note.text.lines().count();
        assert!(
            lines <= 1 + 3 * items,
            "{lines} lines for {items} items:\n{}",
            note.text
        );
        let words: Vec<String> = note
            .text
            .split(|c: char| !c.is_alphanumeric())
            .map(str::to_lowercase)
            .collect();
        for word in ["must", "should", "please"] {
            assert!(
                !words.iter().any(|w| w == word),
                "{word:?} in:\n{}",
                note.text
            );
        }
    }
}

// One note per key.

#[test]
fn a_key_already_sent_is_never_sent_again() {
    let view = projection(vec![(WORK, OTHER, vec![same_declaration()])]);
    let (notes, sent) = compose(WORK, &view, &[], &Sent::default(), t0());
    assert_eq!(notes.len(), 1);
    for later in [Duration::minutes(11), Duration::hours(5)] {
        let (again, _) = compose(WORK, &view, &[], &sent, t0() + later);
        assert!(again.is_empty(), "{again:#?}");
    }
}

#[test]
fn the_key_is_the_other_work_the_target_and_the_change() {
    let view = projection(vec![(WORK, OTHER, vec![uses_theirs()])]);
    let (_, sent) = compose(WORK, &view, &[], &Sent::default(), t0());
    let later = t0() + Duration::minutes(11);

    let other_change = interaction(
        Rule::Uses,
        Some(Side::B),
        symbol("src/auth.rs", "validate"),
        Some(Change::Removes),
    );
    let view = projection(vec![(WORK, OTHER, vec![other_change])]);
    assert_eq!(compose(WORK, &view, &[], &sent, later).0.len(), 1);

    let other_work = projection(vec![(WORK, THIRD, vec![uses_theirs()])]);
    assert_eq!(compose(WORK, &other_work, &[], &sent, later).0.len(), 1);

    let other_target = interaction(
        Rule::Uses,
        Some(Side::B),
        symbol("src/auth.rs", "refresh"),
        Some(Change::Signature),
    );
    let view = projection(vec![(WORK, OTHER, vec![other_target])]);
    assert_eq!(compose(WORK, &view, &[], &sent, later).0.len(), 1);
}

#[test]
fn a_landing_is_its_own_key_even_after_a_note_on_the_same_target() {
    let view = projection(vec![(WORK, LANDED, vec![uses_theirs()])]);
    let (_, sent) = compose(WORK, &view, &[], &Sent::default(), t0());
    let later = t0() + Duration::minutes(11);
    let (notes, sent) = compose(
        WORK,
        &empty(),
        &[landing(vec![uses_theirs()])],
        &sent,
        later,
    );
    assert!(only(&notes).text.contains("refs/dispatch/source"));
    let much_later = later + Duration::minutes(11);
    let (again, _) = compose(
        WORK,
        &empty(),
        &[landing(vec![uses_theirs()])],
        &sent,
        much_later,
    );
    assert!(again.is_empty(), "{again:#?}");
}

#[test]
fn sent_survives_serialization() {
    let view = projection(vec![(WORK, OTHER, vec![same_declaration()])]);
    let (_, sent) = compose(WORK, &view, &[], &Sent::default(), t0());
    let kept: Sent = serde_json::from_str(&json(&sent)).unwrap();
    assert_eq!(json(&kept), json(&sent));
    let later = t0() + Duration::minutes(11);
    assert!(compose(WORK, &view, &[], &kept, later).0.is_empty());
}

// Rate.

#[test]
fn a_burst_is_merged_into_one_note_and_nothing_is_dropped() {
    let view = projection(vec![
        (WORK, OTHER, vec![same_declaration(), uses_ours()]),
        (THIRD, WORK, vec![textual()]),
    ]);
    let (notes, sent) = compose(
        WORK,
        &view,
        &[landing(vec![uses_theirs()])],
        &Sent::default(),
        t0(),
    );
    let text = only(&notes).text.clone();
    for part in [
        "validate",
        "src/api.rs",
        "src/config.py",
        "refs/dispatch/source",
    ] {
        assert!(text.contains(part), "{part} missing:\n{text}");
    }
    assert_eq!(
        text.lines().filter(|l| l.contains("Dispatch")).count(),
        1,
        "{text}"
    );
    let later = t0() + Duration::minutes(11);
    assert!(
        compose(WORK, &view, &[landing(vec![uses_theirs()])], &sent, later)
            .0
            .is_empty()
    );
}

#[test]
fn a_note_inside_the_window_waits_and_is_sent_when_it_opens() {
    let first_view = projection(vec![(WORK, OTHER, vec![same_declaration()])]);
    let (notes, sent) = compose(WORK, &first_view, &[], &Sent::default(), t0());
    assert_eq!(notes.len(), 1);

    let view = projection(vec![
        (WORK, OTHER, vec![same_declaration()]),
        (WORK, THIRD, vec![textual()]),
    ]);
    let early = t0() + Duration::minutes(9) + Duration::seconds(59);
    let (waiting, sent) = compose(WORK, &view, &[], &sent, early);
    assert!(waiting.is_empty(), "{waiting:#?}");

    let open = t0() + Duration::minutes(10) + Duration::seconds(1);
    let (notes, sent) = compose(WORK, &view, &[], &sent, open);
    let text = only(&notes).text.clone();
    assert!(text.contains("src/config.py"), "{text}");
    assert!(!text.contains("validate"), "sent once already:\n{text}");

    let later = open + Duration::minutes(30);
    assert!(compose(WORK, &view, &[], &sent, later).0.is_empty());
}

/// A key names the other Work, the target and the change, not the Work told:
/// what was sent is kept per Work, and each Work hears of the same landing.
#[test]
fn what_was_sent_is_kept_per_work() {
    let view = projection(vec![(WORK, OTHER, vec![same_declaration()])]);
    let (to_work, work_sent) = compose(WORK, &view, &[], &Sent::default(), t0());
    let (to_other, other_sent) = compose(OTHER, &view, &[], &Sent::default(), t0());
    assert_eq!((to_work.len(), to_other.len()), (1, 1));
    let landed = [landing(vec![same_declaration()])];
    let later = t0() + Duration::minutes(11);
    assert_eq!(
        compose(WORK, &empty(), &landed, &work_sent, later).0.len(),
        1
    );
    assert_eq!(
        compose(OTHER, &empty(), &landed, &other_sent, later)
            .0
            .len(),
        1
    );
}

#[test]
fn a_steady_stream_of_changes_gets_at_most_one_note_per_ten_minutes() {
    // Every 30 s for an hour, the other Work changes one more declaration
    // `WORK` also changes; the owner calls each tick.
    let mut sent = Sent::default();
    let mut found = Vec::new();
    let mut times = Vec::new();
    let mut delivered = String::new();
    let mut now = t0();
    for tick in 0..120 {
        found.push(interaction(
            Rule::SameDeclaration,
            None,
            symbol("src/auth.rs", &format!("step_{tick:03}")),
            None,
        ));
        let view = projection(vec![(WORK, OTHER, found.clone())]);
        let (notes, next) = compose(WORK, &view, &[], &sent, now);
        assert!(notes.len() <= 1);
        if let Some(note) = notes.first() {
            times.push(now);
            delivered.push_str(&note.text);
            delivered.push('\n');
        }
        sent = next;
        now += Duration::seconds(30);
    }
    let gaps: Vec<i64> = times
        .windows(2)
        .map(|w| (w[1] - w[0]).num_seconds())
        .collect();
    assert!(gaps.iter().all(|gap| *gap >= 600), "{gaps:?}");
    let mut sorted = gaps.clone();
    sorted.sort();
    assert!(sorted[sorted.len() / 2] >= 600, "median gap {gaps:?}");
    assert!(times.len() <= 7, "{} notes in an hour", times.len());

    // The window after the last tick delivers what is left: nothing dropped.
    let view = projection(vec![(WORK, OTHER, found.clone())]);
    let (notes, _) = compose(WORK, &view, &[], &sent, now + Duration::minutes(10));
    delivered.extend(notes.iter().map(|n| n.text.clone()));
    for tick in 0..120 {
        assert!(
            delivered.contains(&format!("step_{tick:03}")),
            "step_{tick:03} dropped"
        );
    }
}

// Ids and purity.

#[test]
fn ids_are_derived_from_keys_and_stable() {
    let view = projection(vec![(WORK, OTHER, vec![same_declaration()])]);
    let a = only(&compose(WORK, &view, &[], &Sent::default(), t0()).0).clone();
    let b = only(
        &compose(
            WORK,
            &view,
            &[],
            &Sent::default(),
            t0() + Duration::hours(3),
        )
        .0,
    )
    .clone();
    assert_eq!(a.id, b.id);
    assert_eq!(a.keys, b.keys);
    assert_eq!(a.text, b.text);

    let other = projection(vec![(WORK, THIRD, vec![same_declaration()])]);
    let c = only(&first(WORK, &other, &[])).clone();
    assert_ne!(a.id, c.id);
    let d = only(&first(WORK, &empty(), &[landing(vec![same_declaration()])])).clone();
    assert_ne!(a.id, d.id);
    assert!(!a.id.is_empty());
}

#[test]
fn compose_is_deterministic() {
    let view = projection(vec![
        (WORK, OTHER, vec![same_declaration(), uses_ours()]),
        (THIRD, WORK, vec![textual()]),
    ]);
    let landings = [landing(vec![uses_theirs()])];
    let (a, sent_a) = compose(WORK, &view, &landings, &Sent::default(), t0());
    let (b, sent_b) = compose(WORK, &view, &landings, &Sent::default(), t0());
    assert_eq!(json(&sent_a), json(&sent_b));
    assert_eq!(a.len(), b.len());
    for (a, b) in a.iter().zip(&b) {
        assert_eq!((&a.id, &a.text), (&b.id, &b.text));
    }
}

#[test]
fn the_module_does_no_io() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/coherence/notes.rs"),
    )
    .unwrap();
    let production = source.split("#[cfg(test)]").next().unwrap();
    for forbidden in [
        "std::fs",
        "fs::",
        "std::process",
        "Command",
        "std::net",
        "std::io",
        "Utc::now",
        "Local::now",
        "SystemTime",
        "Instant",
        "rand::",
        "env::",
    ] {
        assert!(!production.contains(forbidden), "{forbidden} in notes.rs");
    }
}
