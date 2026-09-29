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
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
}
