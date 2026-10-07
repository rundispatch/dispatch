//! Unresolved-name audit over real runs (measurement only, ignored by default).
//!
//! `DISPATCH_AUDIT_RUNS` is a colon-separated list of run directories, each
//! holding `baseline/` (a Git repository) and `delta.patch`. The baseline commit
//! is `metadata.json`'s `baseline_commit` when present, else the baseline's HEAD.
//!
//! ```text
//! DISPATCH_AUDIT_RUNS=runA:runB cargo test --test name_audit -- --ignored --nocapture
//! ```

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use dispatch::{
    FactOrigin,
    coherence::{
        WorkView,
        facts::{NameCause, audit_names, derive_facts, parse_patch},
    },
};

const EXAMPLES: usize = 20;

struct Run {
    label: String,
    causes: BTreeMap<String, NameCause>,
    unbound: u32,
    /// Base names of the S0 declarations the patch changes, with their
    /// qualified names.
    changed: BTreeMap<String, BTreeSet<String>>,
}

fn baseline_commit(dir: &Path) -> String {
    let recorded = fs::read(dir.join("metadata.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|meta| meta["baseline_commit"].as_str().map(str::to_owned));
    recorded.unwrap_or_else(|| {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir.join("baseline"))
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(output.status.success(), "no baseline commit in {dir:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    })
}

/// `facts::base_name`: `Point::new` -> `new`, `<S as T>::f` -> `f`.
fn base_name(name: &str) -> &str {
    let name = match (name.starts_with('<'), name.rfind(">::")) {
        (true, Some(at)) => &name[at + 3..],
        _ => name,
    };
    name.rsplit([':', '.']).next().unwrap_or(name)
}

fn audit(dir: &Path) -> Run {
    let baseline = dir.join("baseline");
    let patch_path = dir.join("delta.patch");
    let commit = baseline_commit(dir);
    let work = WorkView {
        source: &baseline,
        delta_patch: &patch_path,
        baseline: &baseline,
        baseline_commit: &commit,
    };
    let delta = parse_patch(&fs::read(&patch_path).unwrap());
    let causes = audit_names(&work, &delta).unwrap();
    let derived = derive_facts(&work, &delta).unwrap();
    let unbound = causes
        .values()
        .filter(|cause| matches!(cause, NameCause::Ambiguous | NameCause::Widespread))
        .count();
    assert_eq!(unbound, derived.unbound as usize, "{dir:?}");
    let mut changed: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for fact in derived
        .facts
        .iter()
        .filter(|fact| fact.origin == FactOrigin::Modified)
    {
        changed
            .entry(base_name(&fact.subject).to_owned())
            .or_default()
            .insert(format!("{}:{}", fact.path, fact.subject));
    }
    Run {
        label: dir.file_name().unwrap().to_string_lossy().into_owned(),
        causes,
        unbound: derived.unbound,
        changed,
    }
}

#[test]
#[ignore = "reads real runs named by DISPATCH_AUDIT_RUNS"]
fn audit_unresolved_names_in_real_runs() {
    let list = std::env::var("DISPATCH_AUDIT_RUNS").expect("set DISPATCH_AUDIT_RUNS");
    let runs: Vec<Run> = list
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| audit(&PathBuf::from(dir)))
        .collect();
    for run in &runs {
        let mut by_cause: BTreeMap<NameCause, Vec<&str>> = BTreeMap::new();
        for (name, cause) in &run.causes {
            by_cause.entry(*cause).or_default().push(name);
        }
        let counts: Vec<String> = by_cause
            .iter()
            .map(|(cause, names)| format!("{cause:?}={}", names.len()))
            .collect();
        println!("== {}", run.label);
        println!(
            "names={} unbound={} {}",
            run.causes.len(),
            run.unbound,
            counts.join(" ")
        );
        for (cause, names) in &by_cause {
            let shown: Vec<&str> = names.iter().take(EXAMPLES).copied().collect();
            println!("  {cause:?}: {}", shown.join(" "));
        }
        let mut checked = 0;
        for (name, cause) in &run.causes {
            if !matches!(
                cause,
                NameCause::Ambiguous | NameCause::Widespread | NameCause::NotFound
            ) {
                continue;
            }
            checked += 1;
            for other in runs.iter().filter(|other| other.label != run.label) {
                if let Some(decls) = other.changed.get(name) {
                    let decls: Vec<&str> = decls.iter().map(String::as_str).collect();
                    println!(
                        "  possibly hid an interaction: {name} ({cause:?}) changed by {}: {}",
                        other.label,
                        decls.join(" ")
                    );
                }
            }
        }
        println!("  unresolved names checked against the other runs: {checked}");
    }
}
