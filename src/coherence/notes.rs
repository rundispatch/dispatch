//! What a note says: fixed text telling a live Work which other Work touches
//! it, from the owner's projection and from landings. Each item is sent once
//! per key, and a Work gets at most one note per ten minutes; items found in
//! between, or past what one delivery takes, wait and are merged into the
//! next note. Pure: no I/O, no clock, no randomness.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::interactions::{self, Change, Interaction, Projection, Rule, Side, Target};
use crate::orchestrator::inbox::MAX_TEXT;

const WINDOW: Duration = Duration::minutes(10);
/// Room kept for the line that says how many items wait.
const MORE: usize = 40;
/// The most characters of a signature a note shows, so that every item fits
/// one delivery.
const SIGNATURE: usize = 500;

/// Other Work that landed while `work` was in progress.
#[derive(Clone, Debug)]
pub struct Landing {
    pub run_id: String,
    /// The runtime it ran in, as the person knows it.
    pub runtime: String,
    /// The recorded source commit (`refs/dispatch/source`) that includes it.
    pub commit: String,
    /// Its interactions with `work` at the landing, seen from the landed
    /// run's side (`Side::A` is the landed run), as `measure::Counterpart`
    /// records them.
    pub interactions: Vec<Interaction>,
    /// Old and new signature text of a declaration whose signature it
    /// changed, when the caller knows it: facts keep only a fingerprint.
    pub signatures: BTreeMap<Target, (String, String)>,
}

/// One note for a Work: the items waiting when its window opened, as many
/// as one delivery takes (`MAX_TEXT`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    /// A digest of `keys`: the same items give the same id.
    pub id: String,
    /// Each item's key: the other Work or the landing, the target, the change.
    pub keys: Vec<String>,
    pub text: String,
}

/// What was already sent to one Work. The owner keeps one per Work, since a
/// key does not name the Work it was sent to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sent {
    /// When each key was sent.
    pub at: BTreeMap<String, DateTime<Utc>>,
    /// Landing items waiting for the window, by key. Items about Work in
    /// progress are not kept: each projection finds them again, or finds them
    /// gone.
    pub pending: BTreeMap<String, String>,
    pub last_note_at: Option<DateTime<Utc>>,
}

/// The note for `work` at `now`, if its window is open and anything unsent
/// remains, and what is then sent. `runtimes` names the runtime of each Work
/// in the projection.
pub fn compose(
    work: &str,
    projection: &Projection,
    runtimes: &BTreeMap<String, String>,
    landings: &[Landing],
    sent: &Sent,
    now: DateTime<Utc>,
) -> (Vec<Note>, Sent) {
    let edges = projection
        .edges
        .iter()
        .flat_map(|e| [e.a.as_str(), e.b.as_str()]);
    let mut ids: Vec<&str> = projection
        .participants
        .iter()
        .map(|p| p.run_id.as_str())
        .chain(edges)
        .chain(landings.iter().map(|l| l.run_id.as_str()))
        .collect();
    ids.push(work);
    let short = crate::state::short_ids(&ids);
    let mut next = sent.clone();
    for landing in landings.iter().filter(|landing| landing.run_id != work) {
        for interaction in landing.interactions.iter().filter(|i| eligible(i)) {
            let key = item_key("landing", &landing.run_id, interaction);
            if next.at.contains_key(&key) {
                continue;
            }
            let mut seen = interaction.clone();
            seen.writer = seen.writer.map(|side| match side {
                Side::A => Side::B,
                Side::B => Side::A,
            });
            let mut item = one_line(&format!(
                "{} ({}), landed: {}",
                short[landing.run_id.as_str()],
                landing.runtime,
                interactions::explain(&seen)
            ));
            if interaction.change == Some(Change::Signature)
                && let Some((old, new)) = landing.signatures.get(&interaction.target)
            {
                item += &format!("\nSignature: {} → {}", cap(old), cap(new));
            }
            item += &format!(
                "\nThe source with this change is commit {} (refs/dispatch/source).",
                landing.commit.get(..8).unwrap_or(&landing.commit)
            );
            next.pending.entry(key).or_insert(item);
        }
    }
    let mut items = next.pending.clone();
    for (other, interaction) in projection.of(work) {
        let key = item_key("work", &other, &interaction);
        if eligible(&interaction) && !next.at.contains_key(&key) {
            let runtime = runtimes
                .get(&other)
                .map_or("unknown runtime", String::as_str);
            let item = format!(
                "{} ({runtime}): {}",
                short[other.as_str()],
                interactions::explain(&interaction)
            );
            items.entry(key).or_insert_with(|| one_line(&item));
        }
    }
    if items.is_empty() || next.last_note_at.is_some_and(|last| now - last < WINDOW) {
        return (Vec::new(), next);
    }
    // A note fits one delivery; items past it are not sent yet, and wait
    // for the next note.
    let mut text = format!(
        "Dispatch: other Work touches this Work ({}), your changes in this workspace.",
        short[work]
    );
    let mut length = text.chars().count() + MORE;
    let mut keys = Vec::new();
    for (key, item) in &items {
        length += 1 + item.chars().count();
        if length > MAX_TEXT && !keys.is_empty() {
            break;
        }
        text += "\n";
        text += item;
        keys.push(key.clone());
    }
    if keys.len() < items.len() {
        text += &format!("\nAnd {} more in the next note.", items.len() - keys.len());
    }
    let id = hex::encode(Sha256::digest(keys.join("\n").as_bytes()))[..16].to_owned();
    next.at.extend(keys.iter().map(|key| (key.clone(), now)));
    next.pending.retain(|key, _| !next.at.contains_key(key));
    next.last_note_at = Some(now);
    (vec![Note { id, keys, text }], next)
}

/// Declaration and text evidence only: never a whole-file fact.
fn eligible(interaction: &Interaction) -> bool {
    match interaction.rule {
        Rule::File => false,
        Rule::SameDeclaration | Rule::Uses => matches!(interaction.target, Target::Symbol { .. }),
        Rule::TextualOverlap => true,
    }
}

fn item_key(kind: &str, other: &str, interaction: &Interaction) -> String {
    let (path, name) = match &interaction.target {
        Target::Symbol { path, name } => (path.as_str(), Some(name.as_str())),
        Target::File { path } => (path.as_str(), None),
    };
    serde_json::to_string(&(kind, other, path, name, interaction.change))
        .expect("item key serializes")
}

fn one_line(value: &str) -> String {
    value.replace(['\n', '\r'], " ")
}

/// A signature on one line, cut at `SIGNATURE` characters with "…".
fn cap(value: &str) -> String {
    let value = one_line(value);
    match value.char_indices().nth(SIGNATURE) {
        Some((end, _)) => format!("{}…", &value[..end]),
        None => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coherence::interactions::{Analysis, Edge, Participant};

    const WORK: &str = "aaaaaaaa-1111";
    const OTHER: &str = "bbbbbbbb-2222";
    const HEADER: &str =
        "Dispatch: other Work touches this Work (aaaaaaaa), your changes in this workspace.";

    fn time(minutes: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + minutes * 60, 0).unwrap()
    }

    fn interaction(rule: Rule, target: Target, change: Option<Change>) -> Interaction {
        Interaction {
            rule,
            writer: Some(Side::B),
            target,
            change,
            lines: None,
        }
    }

    fn symbol() -> Target {
        Target::Symbol {
            path: "src/auth.rs".into(),
            name: "validate".into(),
        }
    }

    fn projection(interactions: Vec<Interaction>) -> Projection {
        Projection {
            version: 1,
            computed_at: time(0),
            participants: [WORK, OTHER]
                .map(|run_id| Participant {
                    run_id: run_id.into(),
                    delta_sha256: String::new(),
                    analysis: Analysis::Analyzed,
                    unresolved: 0,
                })
                .into(),
            edges: vec![Edge {
                a: WORK.into(),
                b: OTHER.into(),
                interactions,
            }],
        }
    }

    fn runtimes() -> BTreeMap<String, String> {
        BTreeMap::from([(OTHER.into(), "Claude Code".into())])
    }

    /// `OTHER` landed, changing the signature of `validate`, which `WORK`
    /// uses: `Side::A` is the landed run.
    fn landed() -> Landing {
        Landing {
            run_id: OTHER.into(),
            runtime: "Hermes".into(),
            commit: "1234567890abcdef".into(),
            interactions: vec![Interaction {
                writer: Some(Side::A),
                ..interaction(Rule::Uses, symbol(), Some(Change::Signature))
            }],
            signatures: BTreeMap::new(),
        }
    }

    fn compose_at(
        projection: &Projection,
        landings: &[Landing],
        sent: &Sent,
        minutes: i64,
    ) -> (Vec<Note>, Sent) {
        compose(WORK, projection, &runtimes(), landings, sent, time(minutes))
    }

    #[test]
    fn exact_templates_for_the_three_rules() {
        let cases = [
            (
                interaction(Rule::SameDeclaration, symbol(), None),
                "both change validate (src/auth.rs)",
            ),
            (
                interaction(Rule::Uses, symbol(), Some(Change::Signature)),
                "it changes the signature of validate (src/auth.rs), which this Work uses",
            ),
            (
                Interaction {
                    lines: Some((4, 8)),
                    ..interaction(
                        Rule::TextualOverlap,
                        Target::File {
                            path: "src/auth.rs".into(),
                        },
                        None,
                    )
                },
                "the edits overlap as text at lines 4–8 of src/auth.rs",
            ),
        ];
        for (item, expected) in cases {
            let (notes, _) = compose_at(&projection(vec![item]), &[], &Sent::default(), 0);
            assert_eq!(
                notes[0].text,
                format!("{HEADER}\nbbbbbbbb (Claude Code): {expected}")
            );
        }
    }

    #[test]
    fn landing_template_has_signature_and_recorded_source_commit() {
        let landing = Landing {
            signatures: BTreeMap::from([(
                symbol(),
                ("fn validate()".into(), "fn validate(token: &str)".into()),
            )]),
            ..landed()
        };
        let (notes, _) = compose_at(&projection(vec![]), &[landing], &Sent::default(), 0);
        assert_eq!(
            notes[0].text,
            format!(
                "{HEADER}\nbbbbbbbb (Hermes), landed: it changes the signature of validate \
                 (src/auth.rs), which this Work uses\n\
                 Signature: fn validate() → fn validate(token: &str)\n\
                 The source with this change is commit 12345678 (refs/dispatch/source)."
            )
        );
    }

    #[test]
    fn multiline_signature_stays_within_three_item_lines() {
        let landing = Landing {
            signatures: BTreeMap::from([(
                symbol(),
                (
                    "fn validate(\n)".into(),
                    "fn validate(\n token: &str\n)".into(),
                ),
            )]),
            ..landed()
        };
        let (notes, _) = compose_at(&projection(vec![]), &[landing], &Sent::default(), 0);
        assert_eq!(notes[0].text.lines().count(), 4);
        assert!(
            notes[0]
                .text
                .contains("Signature: fn validate( ) → fn validate(  token: &str )")
        );
    }

    #[test]
    fn a_signature_longer_than_a_delivery_is_cut_and_the_note_is_sent() {
        let (old, new) = ("x".repeat(20_000), "y".repeat(20_000));
        let landing = Landing {
            signatures: BTreeMap::from([(symbol(), (old, new))]),
            ..landed()
        };
        let (notes, sent) = compose_at(&projection(vec![]), &[landing], &Sent::default(), 0);
        let text = &notes[0].text;
        assert!(text.chars().count() <= MAX_TEXT, "{}", text.chars().count());
        let shown = format!("Signature: {}… → {}…\n", "x".repeat(500), "y".repeat(500));
        assert!(text.contains(&shown), "{text}");
        assert!(
            sent.pending.is_empty(),
            "the item is sent, not left waiting"
        );
    }

    #[test]
    fn whole_file_facts_do_not_make_notes() {
        let file = Target::File {
            path: "src/auth.rs".into(),
        };
        let (notes, state) = compose_at(
            &projection(vec![
                interaction(Rule::File, file.clone(), None),
                interaction(Rule::Uses, file, Some(Change::Signature)),
            ]),
            &[],
            &Sent::default(),
            0,
        );
        assert!(notes.is_empty());
        assert_eq!(state, Sent::default());
    }

    #[test]
    fn a_landing_of_the_work_itself_makes_no_note() {
        let own = Landing {
            run_id: WORK.into(),
            ..landed()
        };
        let (notes, _) = compose_at(&projection(vec![]), &[own], &Sent::default(), 0);
        assert!(notes.is_empty());
    }

    #[test]
    fn duplicate_items_wait_and_merge_at_the_window_boundary() {
        let first = projection(vec![interaction(
            Rule::Uses,
            symbol(),
            Some(Change::Signature),
        )]);
        let both = projection(vec![
            interaction(Rule::Uses, symbol(), Some(Change::Signature)),
            interaction(Rule::SameDeclaration, symbol(), None),
        ]);
        let (initial, state) = compose_at(&first, &[], &Sent::default(), 0);
        let (duplicate, state) = compose_at(&first, &[], &state, 1);
        assert!(duplicate.is_empty());
        let (waiting, state) = compose_at(&both, &[landed()], &state, 9);
        assert!(waiting.is_empty());
        assert_eq!(state.pending.len(), 1, "the landing item is kept");
        let (merged, state) = compose_at(&both, &[], &state, 10);
        assert_eq!(merged[0].text.lines().count(), 4);
        assert_eq!(merged[0].keys.len(), 2);
        assert_ne!(initial[0].id, merged[0].id);
        let (again, state) = compose_at(&both, &[landed()], &state, 20);
        assert!(again.is_empty(), "neither item nor landing is sent again");
        assert_eq!(state.at.len(), 3);
        let restored: Sent = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(restored, state);
    }

    #[test]
    fn work_that_leaves_the_projection_before_the_window_opens_is_not_named() {
        let first = projection(vec![interaction(Rule::SameDeclaration, symbol(), None)]);
        let (_, state) = compose_at(&first, &[], &Sent::default(), 0);
        let later = projection(vec![
            interaction(Rule::SameDeclaration, symbol(), None),
            interaction(Rule::Uses, symbol(), Some(Change::Removes)),
        ]);
        let (waiting, state) = compose_at(&later, &[], &state, 5);
        assert!(waiting.is_empty());
        let (notes, _) = compose_at(&projection(vec![]), &[], &state, 10);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn work_from_one_wave_is_told_apart() {
        let (work, other) = ("01M6P8A133GBFFAF40WPY89Z2K", "01M6P8A133AS2NYPEHGR1F7NWA");
        let mut view = projection(vec![interaction(Rule::SameDeclaration, symbol(), None)]);
        view.participants[0].run_id = work.into();
        view.participants[1].run_id = other.into();
        view.edges[0].a = work.into();
        view.edges[0].b = other.into();
        let (notes, _) = compose(work, &view, &runtimes(), &[], &Sent::default(), time(0));
        assert!(notes[0].text.starts_with(
            "Dispatch: other Work touches this Work (01M6P8A133G), your changes in this workspace.\n\
             01M6P8A133A (unknown runtime): both change"
        ));
    }

    #[test]
    fn work_named_only_by_an_edge_is_named_too() {
        let mut view = projection(vec![interaction(Rule::SameDeclaration, symbol(), None)]);
        view.participants.truncate(1);
        let (notes, _) = compose_at(&view, &[], &Sent::default(), 0);
        assert!(
            notes[0]
                .text
                .contains("\nbbbbbbbb (Claude Code): both change")
        );
    }

    #[test]
    fn a_burst_past_one_delivery_is_sent_over_windows_and_nothing_is_lost() {
        let overlap = |n: u32| Interaction {
            lines: Some((n, n)),
            ..interaction(
                Rule::TextualOverlap,
                Target::File {
                    path: format!("src/{}file_{n}.rs", "nested/".repeat(10)),
                },
                None,
            )
        };
        let view = projection((1..=200).map(overlap).collect());
        let mut state = Sent::default();
        let mut sent = Vec::new();
        for window in 0..10 {
            let (early, _) = compose_at(&view, &[landed()], &state, window * 10 - 1);
            assert!(window == 0 || early.is_empty(), "one note per window");
            let (notes, next) = compose_at(&view, &[landed()], &state, window * 10);
            state = next;
            let Some(note) = notes.first() else { break };
            assert!(note.text.chars().count() <= MAX_TEXT, "{}", note.text.len());
            assert_eq!(
                note.text.lines().nth(1).unwrap().contains("landed"),
                window == 0
            );
            sent.extend(note.keys.clone());
        }
        assert_eq!(sent.len(), 201, "every item, the landing first");
        assert_eq!(state.at.len(), 201);
        assert!(state.pending.is_empty());
        let (first, _) = compose_at(&view, &[landed()], &Sent::default(), 0);
        let waiting = 201 - first[0].keys.len();
        assert!(waiting > 0);
        assert!(
            first[0]
                .text
                .ends_with(&format!("\nAnd {waiting} more in the next note."))
        );
    }

    #[test]
    fn ids_are_stable_across_calls_and_input_order() {
        let first = interaction(Rule::Uses, symbol(), Some(Change::Signature));
        let second = interaction(
            Rule::TextualOverlap,
            Target::File {
                path: "src/auth.rs".into(),
            },
            None,
        );
        let make =
            |items, minutes| compose_at(&projection(items), &[], &Sent::default(), minutes).0;
        let a = make(vec![first.clone(), second.clone()], 0);
        let b = make(vec![second, first], 60);
        assert_eq!(a[0], b[0]);
        assert_eq!(a[0].id.len(), 16);
    }
}
