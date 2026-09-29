//! Work against Work: where two pieces of Work that are not yet integrated
//! touch each other's changes or assumptions. This is advisory evidence and
//! never a verdict: CONTINUE / REFRESH / STOP stay the World's business.
//!
//! A footprint is derived from one Work's own (S0, Δ) through the facts layer
//! (`facts::derive_facts`), so it is always recomputable and needs no index.
//! Identities are root-relative paths plus qualified declaration names, never
//! display text, so Work with different S0s is compared by what it names.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    FactKind, FactOrigin,
    coherence::{
        WorkView,
        facts::{self, Contract, DeltaStatus},
        symbols::lang_for_path,
    },
};

/// Something Work reads or writes: a declaration, or a whole file where no
/// declaration-level analysis is possible.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    Symbol { path: String, name: String },
    File { path: String },
}

impl Target {
    pub fn path(&self) -> &str {
        match self {
            Target::Symbol { path, .. } | Target::File { path } => path,
        }
    }
}

/// What Work does to a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Write {
    /// Changes something that exists at S0. For a declaration, `contract` is
    /// false when what its users rely on (its signature) is kept.
    Changes {
        contract: bool,
    },
    Removes,
    Adds,
    Deletes,
}

/// One supported file's text edits against the S0 blob they were made to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEdits {
    pub blob: String,
    /// S0 lines removed or replaced.
    pub removed: Vec<(u32, u32)>,
    /// Pure insertions, as the S0 line the text goes after.
    pub insertions: Vec<u32>,
    /// The S0 lines each hunk covers, context included.
    pub spans: Vec<(u32, u32)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Footprint {
    pub writes: BTreeMap<Target, Write>,
    pub reads: BTreeSet<Target>,
    /// Keyed by path.
    pub text: BTreeMap<String, TextEdits>,
    /// Referenced names left unbound (ambiguous or too common).
    pub unresolved: u32,
    /// Supported files whose post-image does not parse cleanly.
    pub unparsed: Vec<String>,
}

/// How far a footprint can be trusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Analysis {
    Analyzed,
    /// Live Work is mid-edit; this is its last cleanly parsed footprint.
    LastSeen,
    /// Live Work is mid-edit and has never parsed cleanly: nothing is claimed.
    Pending,
}

/// The footprint of `work`'s patch against its own S0.
pub fn footprint(work: &WorkView) -> Result<Footprint> {
    let patch = fs::read(work.delta_patch)
        .with_context(|| format!("failed to read delta {}", work.delta_patch.display()))?;
    let delta = facts::parse_patch(&patch);
    let derived = facts::derive_facts(work, &delta)?;
    let changed: BTreeMap<String, DeltaStatus> = facts::changed_paths(&patch).into_iter().collect();
    let mut found = Footprint {
        unresolved: derived.unbound,
        unparsed: derived.unparsed.clone(),
        ..Footprint::default()
    };
    let mut file_level: BTreeSet<&str> = BTreeSet::new();
    for fact in &derived.facts {
        match (fact.kind, fact.origin) {
            (FactKind::Signature, FactOrigin::Modified) => {
                let key = (fact.path.clone(), fact.subject.clone());
                let write = match derived.contracts.get(&key) {
                    Some(Contract::Kept) => Write::Changes { contract: false },
                    Some(Contract::Removed) => Write::Removes,
                    // Changed, or not known because the post-image did not
                    // parse; `settle` decides what an unparsed file means.
                    Some(Contract::Changed) | None => Write::Changes { contract: true },
                };
                let (path, name) = key;
                found.writes.insert(Target::Symbol { path, name }, write);
            }
            (FactKind::Signature, _) => {
                found.reads.insert(Target::Symbol {
                    path: fact.path.clone(),
                    name: fact.subject.clone(),
                });
            }
            // A file fact on a delta path is a file the work changes without
            // declaration-level analysis; elsewhere, a file its code names.
            (FactKind::File, _) if changed.contains_key(&fact.path) => {
                file_level.insert(&fact.path);
            }
            (FactKind::File, _) => {
                found.reads.insert(Target::File {
                    path: fact.path.clone(),
                });
            }
        }
    }
    let parsed: BTreeSet<&str> = delta.iter().map(|file| file.path.as_str()).collect();
    for (path, status) in &changed {
        let write = match status {
            DeltaStatus::Added => Write::Adds,
            DeltaStatus::Deleted => Write::Deletes,
            DeltaStatus::Modified => {
                // A supported, parsed file is covered by its declarations and
                // its text edits; anything else is judged as a whole file.
                let whole = lang_for_path(path).is_none()
                    || !parsed.contains(path.as_str())
                    || file_level.contains(path.as_str())
                    || (derived.uncertain.contains(path) && !derived.unparsed.contains(path));
                if !whole {
                    continue;
                }
                Write::Changes { contract: true }
            }
        };
        found
            .writes
            .insert(Target::File { path: path.clone() }, write);
    }
    for (path, name) in &derived.introduced {
        found
            .writes
            .entry(Target::Symbol {
                path: path.clone(),
                name: name.clone(),
            })
            .or_insert(Write::Adds);
    }
    for file in &delta {
        if file.status != DeltaStatus::Modified || lang_for_path(&file.path).is_none() {
            continue;
        }
        if let Some(blob) = &file.old_blob {
            found.text.insert(
                file.path.clone(),
                TextEdits {
                    blob: blob.clone(),
                    removed: file.old_ranges.clone(),
                    insertions: file.insertions.clone(),
                    spans: file.spans(),
                },
            );
        }
    }
    Ok(found)
}

/// The footprint to compare for Work whose fresh footprint is `fresh`.
/// A supported file whose post-image does not parse means different things:
/// frozen Work will not change again, so the file is judged as a whole, as the
/// facts layer does; live Work is taken to be mid-edit, so its last clean
/// footprint (`last`) stands, or nothing is claimed until it parses.
pub fn settle(
    mut fresh: Footprint,
    frozen: bool,
    last: Option<&Footprint>,
) -> (Footprint, Analysis) {
    if fresh.unparsed.is_empty() {
        return (fresh, Analysis::Analyzed);
    }
    if frozen {
        for path in fresh.unparsed.clone() {
            fresh
                .writes
                .insert(Target::File { path }, Write::Changes { contract: true });
        }
        return (fresh, Analysis::Analyzed);
    }
    match last.filter(|last| last.unparsed.is_empty()) {
        Some(last) => (last.clone(), Analysis::LastSeen),
        None => (Footprint::default(), Analysis::Pending),
    }
}

/// The rule that found an interaction, strongest evidence first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    /// Both change the same declaration.
    SameDeclaration,
    /// One changes the contract of, or removes, a declaration the other uses.
    Uses,
    /// Both touch a file at least one of them is judged on as a whole.
    File,
    /// Both edit the same S0 text: one's changed lines fall inside a hunk of
    /// the other's, context included. Evidence of overlap, not a prediction
    /// that `git apply` will fail.
    TextualOverlap,
}

/// What the writing side does, when only one side writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Signature,
    Removes,
    Adds,
    Deletes,
    Changes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    A,
    B,
}

/// One piece of evidence that two pieces of Work touch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interaction {
    pub rule: Rule,
    /// The side whose write the other relies on; `None` when both write.
    pub writer: Option<Side>,
    pub target: Target,
    /// What the writer does; for both sides, set only when both add or both
    /// delete.
    pub change: Option<Change>,
    /// The S0 lines where the text overlaps.
    pub lines: Option<(u32, u32)>,
}

/// Every interaction between `a` and `b`. Read/read never interacts, nor do
/// different declarations of one file whose hunks keep apart, nor a change to
/// a body alone that the other only uses.
pub fn between(a: &Footprint, b: &Footprint) -> Vec<Interaction> {
    let mut found = Vec::new();
    // Rule 3 first: a path judged as a whole needs no finer evidence there.
    let touches = |fp: &Footprint, path: &str| {
        fp.writes.keys().any(|t| t.path() == path) || fp.reads.iter().any(|t| t.path() == path)
    };
    let mut coarse = BTreeSet::new();
    for (side, writer, other) in [(Side::A, a, b), (Side::B, b, a)] {
        for (target, write) in &writer.writes {
            let Target::File { path } = target else {
                continue;
            };
            if !touches(other, path) || coarse.contains(path.as_str()) {
                continue;
            }
            let both = other.writes.keys().any(|t| t.path() == path);
            let change = if both {
                match (write, other.writes.get(target)) {
                    (Write::Adds, Some(Write::Adds)) => Some(Change::Adds),
                    (Write::Deletes, Some(Write::Deletes)) => Some(Change::Deletes),
                    _ => None,
                }
            } else {
                // A file has no signature: judged whole, it simply changes.
                Some(match write {
                    Write::Changes { .. } => Change::Changes,
                    other => change_of(*other),
                })
            };
            found.push(Interaction {
                rule: Rule::File,
                writer: (!both).then_some(side),
                target: target.clone(),
                change,
                lines: None,
            });
            coarse.insert(path.as_str());
        }
    }
    let mut declared = BTreeSet::new();
    for (target, write) in &a.writes {
        if !matches!(target, Target::Symbol { .. }) || coarse.contains(target.path()) {
            continue;
        }
        if let Some(other) = b.writes.get(target) {
            found.push(Interaction {
                rule: Rule::SameDeclaration,
                writer: None,
                target: target.clone(),
                change: (*write == Write::Adds && *other == Write::Adds).then_some(Change::Adds),
                lines: None,
            });
            declared.insert(target.path());
        }
    }
    for (side, writer, reader) in [(Side::A, a, b), (Side::B, b, a)] {
        for (target, write) in &writer.writes {
            let breaks = matches!(write, Write::Changes { contract: true } | Write::Removes);
            if breaks
                && matches!(target, Target::Symbol { .. })
                && !coarse.contains(target.path())
                && reader.reads.contains(target)
                && !reader.writes.contains_key(target)
            {
                found.push(Interaction {
                    rule: Rule::Uses,
                    writer: Some(side),
                    target: target.clone(),
                    change: Some(change_of(*write)),
                    lines: None,
                });
            }
        }
    }
    for (path, edits) in &a.text {
        if coarse.contains(path.as_str()) || declared.contains(path.as_str()) {
            continue;
        }
        let Some(other) = b.text.get(path).filter(|other| other.blob == edits.blob) else {
            continue;
        };
        if let Some(lines) = overlap(edits, other).or_else(|| overlap(other, edits)) {
            found.push(Interaction {
                rule: Rule::TextualOverlap,
                writer: None,
                target: Target::File { path: path.clone() },
                change: None,
                lines: Some(lines),
            });
        }
    }
    found.sort_by(|x, y| (x.rule, &x.target).cmp(&(y.rule, &y.target)));
    found
}

fn change_of(write: Write) -> Change {
    match write {
        Write::Changes { contract: true } => Change::Signature,
        Write::Changes { contract: false } => Change::Changes,
        Write::Removes => Change::Removes,
        Write::Adds => Change::Adds,
        Write::Deletes => Change::Deletes,
    }
}

/// The S0 lines where `edits` change text inside one of `other`'s hunks: a
/// removed line within the hunk, an insertion strictly between two of its
/// lines, or an insertion at the very place `other` inserts too.
fn overlap(edits: &TextEdits, other: &TextEdits) -> Option<(u32, u32)> {
    other.spans.iter().find_map(|&(start, end)| {
        let removed = edits
            .removed
            .iter()
            .any(|&(low, high)| low <= end && start <= high);
        let inserted = edits.insertions.iter().any(|&after| {
            (start <= after && after < end)
                || (other.insertions.contains(&after) && start <= after + 1 && after <= end)
        });
        (removed || inserted).then_some((start, end))
    })
}

/// The interactions of every pair among `works`, each footprint compared once
/// with every other: O(n²) pairs, each a few set lookups.
pub fn edges(works: &[(String, Footprint)]) -> Vec<(&str, &str, Vec<Interaction>)> {
    let mut edges = Vec::new();
    for (i, (a, fa)) in works.iter().enumerate() {
        for (b, fb) in &works[i + 1..] {
            let found = between(fa, fb);
            if !found.is_empty() {
                edges.push((a.as_str(), b.as_str(), found));
            }
        }
    }
    edges
}

/// The owner's view of the project's unintegrated Work and where it
/// interacts: derived, disposable and recomputable from each Work's (S0, Δ).
/// It is written only by the project owner and read only while that owner
/// holds the project; nothing about it is canonical.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Projection {
    pub version: u32,
    pub computed_at: DateTime<Utc>,
    pub participants: Vec<Participant>,
    pub edges: Vec<Edge>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Participant {
    pub run_id: String,
    pub delta_sha256: String,
    pub analysis: Analysis,
    pub unresolved: u32,
}

/// The interactions between Work `a` and Work `b`; `Side::A` is `a`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub a: String,
    pub b: String,
    pub interactions: Vec<Interaction>,
}

impl Projection {
    /// The interactions of `run_id`, each with the other Work's id, seen from
    /// `run_id`'s side: its `writer` is `Side::A` when `run_id` writes.
    pub fn of(&self, run_id: &str) -> Vec<(String, Interaction)> {
        let mut found = Vec::new();
        for edge in &self.edges {
            let (other, flip) = if edge.a == run_id {
                (&edge.b, false)
            } else if edge.b == run_id {
                (&edge.a, true)
            } else {
                continue;
            };
            for interaction in &edge.interactions {
                let mut interaction = interaction.clone();
                if flip {
                    interaction.writer = interaction.writer.map(|side| match side {
                        Side::A => Side::B,
                        Side::B => Side::A,
                    });
                }
                found.push((other.clone(), interaction));
            }
        }
        found
    }
}

impl Projection {
    fn participant(&self, run_id: &str) -> Option<&Participant> {
        self.participants.iter().find(|p| p.run_id == run_id)
    }

    /// How other Work is named: long enough to tell every participant apart.
    fn label(&self, run_id: &str) -> String {
        let ids: Vec<&str> = self
            .participants
            .iter()
            .map(|p| p.run_id.as_str())
            .collect();
        crate::state::short_ids(&ids)
            .get(run_id)
            .map_or_else(|| run_id.to_owned(), |short| (*short).to_owned())
    }

    /// The project view's segment for `run_id`: whom it interacts with.
    pub fn summary(&self, run_id: &str) -> Option<String> {
        let mut others: Vec<String> = self.of(run_id).into_iter().map(|(id, _)| id).collect();
        others.sort();
        others.dedup();
        match others.as_slice() {
            [] => None,
            [one] => Some(format!("interacts with {}", self.label(one))),
            many => Some(format!("interacts with {} Work", many.len())),
        }
    }

    /// What `status` and `watch` say about `run_id` among the project's Work
    /// in progress; `None` when it takes no part (applied, closed, lost).
    pub fn details(&self, run_id: &str) -> Option<Vec<String>> {
        let participant = self.participant(run_id)?;
        if participant.analysis == Analysis::Pending {
            return Some(vec![
                "not known yet: its files do not parse while it is being edited".into(),
            ]);
        }
        let mut lines: Vec<String> = self
            .of(run_id)
            .iter()
            .map(|(other, interaction)| {
                format!("with {}: {}", self.label(other), explain(interaction))
            })
            .collect();
        if lines.is_empty() {
            lines.push("no interaction with other Work in progress".into());
        }
        if participant.analysis == Analysis::LastSeen {
            lines.push("as last seen while its files parsed; it is being edited".into());
        }
        if participant.unresolved > 0 {
            lines.push(format!(
                "{} name{} it uses could not be tied to one declaration",
                participant.unresolved,
                if participant.unresolved == 1 { "" } else { "s" }
            ));
        }
        Some(lines)
    }

    /// The stable JSON form of `run_id`'s interactions.
    pub fn json(&self, run_id: &str) -> Vec<serde_json::Value> {
        self.of(run_id)
            .iter()
            .map(|(other, i)| {
                let (path, symbol) = match &i.target {
                    Target::Symbol { path, name } => (path, Some(name)),
                    Target::File { path } => (path, None),
                };
                serde_json::json!({
                    "with": other,
                    "direction": match i.writer {
                        None => "both",
                        Some(Side::A) => "this_affects_theirs",
                        Some(Side::B) => "theirs_affects_this",
                    },
                    "rule": i.rule,
                    "path": path,
                    "symbol": symbol,
                    "change": i.change,
                    "evidence": match i.rule {
                        Rule::SameDeclaration | Rule::Uses => "symbol",
                        Rule::File => "file",
                        Rule::TextualOverlap => "text",
                    },
                    "lines": i.lines,
                })
            })
            .collect()
    }
}

/// One interaction in words, from this Work's side (`Side::A`); "it" is the
/// other Work.
pub fn explain(i: &Interaction) -> String {
    let what = match &i.target {
        Target::Symbol { path, name } => format!("{name} ({path})"),
        Target::File { path } => path.clone(),
    };
    let verb = |change: Option<Change>| match change {
        Some(Change::Signature) => "changes the signature of",
        Some(Change::Removes) => "removes",
        Some(Change::Adds) => "adds",
        Some(Change::Deletes) => "deletes",
        Some(Change::Changes) | None => "changes",
    };
    let both = |change: Option<Change>| match change {
        Some(Change::Adds) => "both add",
        Some(Change::Deletes) => "both delete",
        _ => "both change",
    };
    match (i.rule, i.writer) {
        (Rule::SameDeclaration, _) => format!("{} {what}", both(i.change)),
        (Rule::Uses, Some(Side::B)) => {
            format!("it {} {what}, which this Work uses", verb(i.change))
        }
        (Rule::Uses, _) => format!("this Work {} {what}, which it uses", verb(i.change)),
        (Rule::File, None) => format!("{} {what} (whole file)", both(i.change)),
        (Rule::File, Some(Side::B)) => format!(
            "it {} {what}, which this Work relies on (whole file)",
            verb(i.change)
        ),
        (Rule::File, Some(Side::A)) => format!(
            "this Work {} {what}, which it relies on (whole file)",
            verb(i.change)
        ),
        (Rule::TextualOverlap, _) => match i.lines {
            Some((start, end)) => {
                format!("the edits overlap as text at lines {start}–{end} of {what}")
            }
            None => format!("the edits overlap as text in {what}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::*;
    use crate::source::{
        SourceSnapshot, collect_diff, create_candidate_workspace, create_snapshot,
    };

    fn write(path: &Path, contents: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// A source snapshotted like a run: each `work` is a candidate workspace
    /// of that S0 with some edits, and its collected patch.
    pub(super) struct Base {
        root: TempDir,
        source: PathBuf,
        snapshot: SourceSnapshot,
        works: std::cell::Cell<u32>,
    }

    pub(super) struct Work<'a> {
        base: &'a Base,
        patch: PathBuf,
    }

    impl Base {
        pub(super) fn new(files: &[(&str, &str)]) -> Self {
            let root = TempDir::new().unwrap();
            let source = fs::canonicalize(root.path()).unwrap().join("source");
            for (path, contents) in files {
                write(&source.join(path), contents.as_bytes());
            }
            let snapshot = create_snapshot(&source, &root.path().join("run")).unwrap();
            Self {
                root,
                source,
                snapshot,
                works: std::cell::Cell::new(0),
            }
        }

        /// Work with these edits: `Some` is new contents, `None` deletes.
        pub(super) fn work(&self, edits: &[(&str, Option<&[u8]>)]) -> Work<'_> {
            let n = self.works.get() + 1;
            self.works.set(n);
            let workspace = create_candidate_workspace(
                &self.snapshot.baseline_path,
                &self.root.path().join(format!("workspace{n}")),
            )
            .unwrap();
            for (path, contents) in edits {
                match contents {
                    Some(contents) => write(&workspace.join(path), contents),
                    None => fs::remove_file(workspace.join(path)).unwrap(),
                }
            }
            let patch = self.root.path().join(format!("delta{n}.patch"));
            collect_diff(&self.snapshot.baseline_path, &workspace, &patch).unwrap();
            Work { base: self, patch }
        }
    }

    impl Work<'_> {
        pub(super) fn footprint(&self) -> Footprint {
            footprint(&WorkView {
                source: &self.base.source,
                delta_patch: &self.patch,
                baseline: &self.base.snapshot.baseline_path,
                baseline_commit: &self.base.snapshot.baseline_commit,
            })
            .unwrap()
        }
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

    const AUTH: &str = "pub fn validate(token: &str) -> bool {\n    !token.is_empty()\n}\n\n\
        pub fn refresh(token: &str) -> String {\n    token.to_owned()\n}\n";

    #[test]
    fn a_body_change_keeps_the_contract_and_a_signature_change_does_not() {
        let base = Base::new(&[("src/auth.rs", AUTH)]);
        let body = AUTH.replace("!token.is_empty()", "token.len() > 3");
        let fp = base
            .work(&[("src/auth.rs", Some(body.as_bytes()))])
            .footprint();
        assert_eq!(
            fp.writes.get(&symbol("src/auth.rs", "validate")),
            Some(&Write::Changes { contract: false })
        );
        assert!(!fp.writes.contains_key(&symbol("src/auth.rs", "refresh")));
        assert!(!fp.writes.contains_key(&file("src/auth.rs")), "{fp:?}");

        let signature = AUTH.replace(
            "validate(token: &str)",
            "validate(token: &str, strict: bool)",
        );
        let fp = base
            .work(&[("src/auth.rs", Some(signature.as_bytes()))])
            .footprint();
        assert_eq!(
            fp.writes.get(&symbol("src/auth.rs", "validate")),
            Some(&Write::Changes { contract: true })
        );

        let removed = AUTH.replace(
            "pub fn validate(token: &str) -> bool {\n    !token.is_empty()\n}\n\n",
            "",
        );
        let fp = base
            .work(&[("src/auth.rs", Some(removed.as_bytes()))])
            .footprint();
        assert_eq!(
            fp.writes.get(&symbol("src/auth.rs", "validate")),
            Some(&Write::Removes)
        );
    }

    #[test]
    fn a_compatible_python_signature_keeps_the_contract() {
        let base = Base::new(&[("auth.py", "def validate(token):\n    return bool(token)\n")]);
        let fp = base
            .work(&[(
                "auth.py",
                Some(b"def validate(token, strict=False):\n    return bool(token)\n"),
            )])
            .footprint();
        assert_eq!(
            fp.writes.get(&symbol("auth.py", "validate")),
            Some(&Write::Changes { contract: false })
        );
    }

    #[test]
    fn calls_are_reads_and_new_declarations_are_adds() {
        let base = Base::new(&[("src/auth.rs", AUTH), ("src/api.rs", "pub fn serve() {}\n")]);
        let fp = base
            .work(&[(
                "src/api.rs",
                Some(b"pub fn serve() {}\n\npub fn login(t: &str) -> bool {\n    validate(t)\n}\n"),
            )])
            .footprint();
        assert!(
            fp.reads.contains(&symbol("src/auth.rs", "validate")),
            "{fp:?}"
        );
        assert_eq!(
            fp.writes.get(&symbol("src/api.rs", "login")),
            Some(&Write::Adds)
        );
        assert!(!fp.writes.contains_key(&symbol("src/api.rs", "serve")));
        let text = &fp.text["src/api.rs"];
        assert_eq!(text.blob.len(), 40, "{text:?}");
        assert!(!text.spans.is_empty());
    }

    #[test]
    fn files_without_declarations_are_whole_files() {
        let base = Base::new(&[
            ("config.yml", "a: 1\n"),
            ("gone.py", "def f():\n    pass\n"),
            ("logo.bin", "\0\x01\x02"),
        ]);
        let fp = base
            .work(&[
                ("config.yml", Some(b"a: 2\n")),
                ("gone.py", None),
                ("new.txt", Some(b"hello\n")),
                ("logo.bin", Some(b"\0\x03\x04")),
            ])
            .footprint();
        assert_eq!(
            fp.writes.get(&file("config.yml")),
            Some(&Write::Changes { contract: true })
        );
        assert_eq!(fp.writes.get(&file("gone.py")), Some(&Write::Deletes));
        assert_eq!(fp.writes.get(&file("new.txt")), Some(&Write::Adds));
        assert_eq!(
            fp.writes.get(&file("logo.bin")),
            Some(&Write::Changes { contract: true })
        );
    }

    #[test]
    fn an_unparsed_post_image_waits_while_live_and_is_a_whole_file_once_frozen() {
        let base = Base::new(&[("src/auth.rs", AUTH)]);
        let clean = AUTH.replace("!token.is_empty()", "token.len() > 3");
        let last = base
            .work(&[("src/auth.rs", Some(clean.as_bytes()))])
            .footprint();
        let broken = AUTH.replace("!token.is_empty()\n}", "!token.is_empty(\n");
        let fresh = base
            .work(&[("src/auth.rs", Some(broken.as_bytes()))])
            .footprint();
        assert_eq!(fresh.unparsed, vec!["src/auth.rs".to_owned()]);

        let (shown, analysis) = settle(fresh.clone(), false, None);
        assert_eq!(analysis, Analysis::Pending);
        assert_eq!(shown, Footprint::default());

        let (shown, analysis) = settle(fresh.clone(), false, Some(&last));
        assert_eq!(analysis, Analysis::LastSeen);
        assert_eq!(shown, last);

        let (shown, analysis) = settle(fresh, true, Some(&last));
        assert_eq!(analysis, Analysis::Analyzed);
        assert_eq!(
            shown.writes.get(&file("src/auth.rs")),
            Some(&Write::Changes { contract: true })
        );
    }

    // ---- the interaction matrix ------------------------------------------

    /// A Rust library: `validate` and `refresh` far apart, `first` and
    /// `second` side by side, plus an API and a CLI module that call nothing.
    fn library() -> String {
        let filler: String = (1..=8)
            .map(|n| format!("pub fn filler_{n}() {{}}\n"))
            .collect();
        format!(
            "use std::fmt;\n\npub fn validate(token: &str) -> bool {{\n    !token.is_empty()\n}}\n\n\
             {filler}\npub fn refresh(token: &str) -> String {{\n    token.to_owned()\n}}\n\n\
             {filler}\npub fn first() -> i32 {{\n    1\n}}\npub fn second() -> i32 {{\n    2\n}}\n"
        )
    }

    fn project() -> Base {
        let lib = library();
        Base::new(&[
            ("src/auth.rs", lib.as_str()),
            ("src/api.rs", "pub fn serve() {}\n"),
            ("src/cli.rs", "pub fn main() {}\n"),
            ("config.yml", "a: 1\nb: 2\n"),
        ])
    }

    fn edit(from: &str, to: &str) -> String {
        let lib = library();
        assert!(lib.contains(from), "{from}");
        lib.replacen(from, to, 1)
    }

    fn rules(found: &[Interaction]) -> Vec<(Rule, Option<Side>, String)> {
        found
            .iter()
            .map(|i| (i.rule, i.writer, format!("{:?}", i.target)))
            .collect()
    }

    fn pair(
        base: &Base,
        a: &[(&str, Option<&[u8]>)],
        b: &[(&str, Option<&[u8]>)],
    ) -> Vec<Interaction> {
        between(&base.work(a).footprint(), &base.work(b).footprint())
    }

    const CALLER: &[u8] =
        b"pub fn serve() {}\n\npub fn login(t: &str) -> bool {\n    validate(t)\n}\n";
    const CLI_CALLER: &[u8] =
        b"pub fn main() {}\n\npub fn check(t: &str) -> bool {\n    validate(t)\n}\n";

    #[test]
    fn m1_both_change_the_same_declaration() {
        let base = project();
        let a = edit("!token.is_empty()", "token.len() > 1");
        let b = edit("!token.is_empty()", "token.len() > 2");
        let found = pair(
            &base,
            &[("src/auth.rs", Some(a.as_bytes()))],
            &[("src/auth.rs", Some(b.as_bytes()))],
        );
        assert_eq!(
            rules(&found),
            vec![(
                Rule::SameDeclaration,
                None,
                format!("{:?}", symbol("src/auth.rs", "validate"))
            )]
        );
    }

    #[test]
    fn m2_m3_a_signature_change_meets_its_callers_in_either_direction() {
        let base = project();
        let a = edit(
            "validate(token: &str)",
            "validate(token: &str, strict: bool)",
        );
        let a = base
            .work(&[("src/auth.rs", Some(a.as_bytes()))])
            .footprint();
        let b = base.work(&[("src/api.rs", Some(CALLER))]).footprint();
        let found = between(&a, &b);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            (found[0].rule, found[0].writer, found[0].change),
            (Rule::Uses, Some(Side::A), Some(Change::Signature))
        );
        let found = between(&b, &a);
        assert_eq!(
            (found[0].rule, found[0].writer),
            (Rule::Uses, Some(Side::B))
        );
    }

    #[test]
    fn m2b_removing_a_used_declaration_interacts() {
        let base = project();
        let a = edit(
            "pub fn validate(token: &str) -> bool {\n    !token.is_empty()\n}\n",
            "",
        );
        let found = pair(
            &base,
            &[("src/auth.rs", Some(a.as_bytes()))],
            &[("src/api.rs", Some(CALLER))],
        );
        assert_eq!(
            (found[0].rule, found[0].change),
            (Rule::Uses, Some(Change::Removes)),
            "{found:?}"
        );
    }

    #[test]
    fn m4_both_only_calling_the_same_declaration_is_no_interaction() {
        let base = project();
        assert_eq!(
            pair(
                &base,
                &[("src/api.rs", Some(CALLER))],
                &[("src/cli.rs", Some(CLI_CALLER))]
            ),
            vec![]
        );
    }

    #[test]
    fn m5_different_declarations_of_one_file_apart_do_not_interact() {
        let base = project();
        let a = edit("!token.is_empty()", "token.len() > 1");
        let b = edit("token.to_owned()", "token.to_string()");
        assert_eq!(
            pair(
                &base,
                &[("src/auth.rs", Some(a.as_bytes()))],
                &[("src/auth.rs", Some(b.as_bytes()))]
            ),
            vec![]
        );
    }

    #[test]
    fn m6_unrelated_files_do_not_interact() {
        let base = project();
        assert_eq!(
            pair(
                &base,
                &[("src/api.rs", Some(b"pub fn serve() { }\n"))],
                &[("src/cli.rs", Some(b"pub fn main() { }\n"))]
            ),
            vec![]
        );
    }

    #[test]
    fn m7_the_same_file_without_declarations_interacts_as_a_whole_file() {
        let base = project();
        let found = pair(
            &base,
            &[("config.yml", Some(b"a: 3\nb: 2\n"))],
            &[("config.yml", Some(b"a: 1\nb: 4\n"))],
        );
        assert_eq!(
            rules(&found),
            vec![(Rule::File, None, format!("{:?}", file("config.yml")))]
        );
    }

    #[test]
    fn m8_deleting_a_file_meets_whoever_changes_or_uses_it() {
        let base = Base::new(&[
            ("util.py", "def helper(x):\n    return x\n"),
            ("main.py", "def run():\n    pass\n"),
        ]);
        let changed = pair(
            &base,
            &[("util.py", None)],
            &[("util.py", Some(b"def helper(x):\n    return x + 1\n"))],
        );
        assert_eq!(
            rules(&changed),
            vec![(Rule::File, None, format!("{:?}", file("util.py")))]
        );
        let used = pair(
            &base,
            &[("util.py", None)],
            &[("main.py", Some(b"def run():\n    return helper(1)\n"))],
        );
        assert_eq!(
            (used[0].rule, used[0].writer, used[0].change),
            (Rule::File, Some(Side::A), Some(Change::Deletes)),
            "{used:?}"
        );
    }

    #[test]
    fn m9_both_adding_the_same_path_interacts_once() {
        let base = project();
        let found = pair(
            &base,
            &[("src/new.rs", Some(b"pub fn made() {}\n"))],
            &[("src/new.rs", Some(b"pub fn made() {}\npub fn other() {}\n"))],
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            (found[0].rule, found[0].change),
            (Rule::File, Some(Change::Adds))
        );
    }

    #[test]
    fn m9b_both_adding_imports_at_the_top_overlap_as_text() {
        let base = project();
        let a = format!("use std::io;\n{}", library());
        let b = format!("use std::fs;\n{}", library());
        let found = pair(
            &base,
            &[("src/auth.rs", Some(a.as_bytes()))],
            &[("src/auth.rs", Some(b.as_bytes()))],
        );
        assert_eq!(
            rules(&found),
            vec![(
                Rule::TextualOverlap,
                None,
                format!("{:?}", file("src/auth.rs"))
            )]
        );
    }

    #[test]
    fn m9c_adjacent_edits_of_different_declarations_overlap_as_text_only() {
        let base = project();
        let a = edit("    1\n", "    10\n");
        let b = edit("    2\n", "    20\n");
        let found = pair(
            &base,
            &[("src/auth.rs", Some(a.as_bytes()))],
            &[("src/auth.rs", Some(b.as_bytes()))],
        );
        assert_eq!(
            rules(&found),
            vec![(
                Rule::TextualOverlap,
                None,
                format!("{:?}", file("src/auth.rs"))
            )],
            "{found:?}"
        );
    }

    #[test]
    fn m9d_a_body_change_does_not_meet_its_callers() {
        let base = project();
        let a = edit("!token.is_empty()", "token.len() > 1");
        assert_eq!(
            pair(
                &base,
                &[("src/auth.rs", Some(a.as_bytes()))],
                &[("src/api.rs", Some(CALLER))]
            ),
            vec![]
        );
    }

    #[test]
    fn m9f_a_compatible_python_signature_does_not_meet_its_callers() {
        let base = Base::new(&[
            ("auth.py", "def validate(token):\n    return bool(token)\n"),
            ("app.py", "def run():\n    pass\n"),
        ]);
        let found = pair(
            &base,
            &[(
                "auth.py",
                Some(b"def validate(token, strict=False):\n    return bool(token)\n"),
            )],
            &[("app.py", Some(b"def run():\n    return validate('x')\n"))],
        );
        assert_eq!(found, vec![]);
    }

    #[test]
    fn m9g_a_shared_declaration_explains_the_file_without_line_evidence() {
        let base = project();
        let a = format!(
            "use std::io;\n{}",
            edit("!token.is_empty()", "token.len() > 1")
        );
        let b = format!(
            "use std::fs;\n{}",
            edit("!token.is_empty()", "token.len() > 2")
        );
        let found = pair(
            &base,
            &[("src/auth.rs", Some(a.as_bytes()))],
            &[("src/auth.rs", Some(b.as_bytes()))],
        );
        assert_eq!(
            rules(&found),
            vec![(
                Rule::SameDeclaration,
                None,
                format!("{:?}", symbol("src/auth.rs", "validate"))
            )]
        );
    }

    #[test]
    fn m14_work_from_different_s0s_is_compared_by_name_and_never_by_line() {
        let before = project();
        let moved = library().replace("token.to_owned()", "String::from(token)");
        let after = Base::new(&[
            ("src/auth.rs", moved.as_str()),
            ("src/api.rs", "pub fn serve() {}\n"),
        ]);
        let a = edit("!token.is_empty()", "token.len() > 1");
        let a = before
            .work(&[("src/auth.rs", Some(a.as_bytes()))])
            .footprint();
        let b = moved.replace("!token.is_empty()", "token.len() > 2");
        let b = after
            .work(&[("src/auth.rs", Some(b.as_bytes()))])
            .footprint();
        let found = between(&a, &b);
        assert_eq!(
            rules(&found),
            vec![(
                Rule::SameDeclaration,
                None,
                format!("{:?}", symbol("src/auth.rs", "validate"))
            )]
        );

        // Adjacent edits that would overlap as text from one S0 claim nothing
        // across two: their line numbers are not comparable.
        let a = before
            .work(&[("src/auth.rs", Some(edit("    1\n", "    10\n").as_bytes()))])
            .footprint();
        let b = after
            .work(&[(
                "src/auth.rs",
                Some(moved.replacen("    2\n", "    20\n", 1).as_bytes()),
            )])
            .footprint();
        assert_eq!(between(&a, &b), vec![]);

        let signature = edit(
            "validate(token: &str)",
            "validate(token: &str, strict: bool)",
        );
        let a = before
            .work(&[("src/auth.rs", Some(signature.as_bytes()))])
            .footprint();
        let b = after.work(&[("src/api.rs", Some(CALLER))]).footprint();
        assert_eq!(between(&a, &b)[0].rule, Rule::Uses);
    }

    #[test]
    fn edges_lists_only_pairs_that_interact() {
        let base = project();
        let signature = edit(
            "validate(token: &str)",
            "validate(token: &str, strict: bool)",
        );
        let works = vec![
            (
                "A".to_owned(),
                base.work(&[("src/auth.rs", Some(signature.as_bytes()))])
                    .footprint(),
            ),
            (
                "B".to_owned(),
                base.work(&[("src/api.rs", Some(CALLER))]).footprint(),
            ),
            (
                "D".to_owned(),
                base.work(&[("src/cli.rs", Some(b"pub fn main() { }\n"))])
                    .footprint(),
            ),
        ];
        let found = edges(&works);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].0, found[0].1), ("A", "B"));
    }

    #[test]
    fn a_projection_speaks_from_each_side() {
        let uses = Interaction {
            rule: Rule::Uses,
            writer: Some(Side::A),
            target: symbol("src/auth.rs", "validate"),
            change: Some(Change::Signature),
            lines: None,
        };
        let text = Interaction {
            rule: Rule::TextualOverlap,
            writer: None,
            target: file("src/api.rs"),
            change: None,
            lines: Some((1, 4)),
        };
        let participant = |id: &str, analysis| Participant {
            run_id: id.into(),
            delta_sha256: String::new(),
            analysis,
            unresolved: 0,
        };
        let projection = Projection {
            version: 1,
            computed_at: Utc::now(),
            participants: vec![
                participant("AAAAAAAAAA", Analysis::Analyzed),
                participant("BBBBBBBBBB", Analysis::Analyzed),
                participant("CCCCCCCCCC", Analysis::Pending),
            ],
            edges: vec![Edge {
                a: "AAAAAAAAAA".into(),
                b: "BBBBBBBBBB".into(),
                interactions: vec![uses, text],
            }],
        };
        assert_eq!(
            projection.details("AAAAAAAAAA").unwrap(),
            vec![
                "with BBBBBBBB: this Work changes the signature of validate (src/auth.rs), which it uses",
                "with BBBBBBBB: the edits overlap as text at lines 1–4 of src/api.rs",
            ]
        );
        assert_eq!(
            projection.details("BBBBBBBB"),
            None,
            "only a full id takes part"
        );
        assert_eq!(
            projection.details("BBBBBBBBBB").unwrap()[0],
            "with AAAAAAAA: it changes the signature of validate (src/auth.rs), which this Work uses"
        );
        assert_eq!(
            projection.summary("BBBBBBBBBB").as_deref(),
            Some("interacts with AAAAAAAA")
        );
        assert_eq!(projection.summary("CCCCCCCCCC"), None);
        assert!(projection.details("CCCCCCCCCC").unwrap()[0].starts_with("not known yet"));
        let json = projection.json("BBBBBBBBBB");
        assert_eq!(json[0]["direction"], "theirs_affects_this");
        assert_eq!(json[0]["rule"], "uses");
        assert_eq!(json[0]["symbol"], "validate");
        assert_eq!(json[0]["change"], "signature");
        assert_eq!(json[0]["evidence"], "symbol");
        assert_eq!(json[1]["evidence"], "text");
        assert_eq!(json[1]["lines"], serde_json::json!([1, 4]));
    }

    #[test]
    fn a_whole_file_changes_and_has_no_signature() {
        let base = project();
        let found = pair(
            &base,
            &[("config.yml", Some(b"a: 3\nb: 2\n"))],
            &[(
                "src/api.rs",
                Some(b"pub fn serve() {\n    let _ = \"config.yml\";\n}\n"),
            )],
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].change, Some(Change::Changes));
        assert_eq!(
            explain(&found[0]),
            "this Work changes config.yml, which it relies on (whole file)"
        );
    }
}
