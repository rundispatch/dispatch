//! Deterministic scenario matrix for work coherence.
//!
//! Every scenario builds a real S0 (a Git repository or a plain directory),
//! takes a real Dispatch snapshot, edits a real candidate workspace, collects
//! the real delta patch with `collect_diff`, then moves the source (the
//! "world") and asks three detectors whether the work is still valid:
//!
//! * strict today: the whole-tree fingerprint moved (what `safe_apply` does);
//! * file overlap: a changed world file is also a file the delta touches;
//! * coherence: `coherence::evaluate` (L0 file/patch checks, then L1 symbol
//!   and file facts).
//!
//! `expected` is the verdict the evaluator must give; `expected_l0` is kept
//! equal to it now that L1 exists. A row whose two differ would be
//! `pending_symbols`: reported, not asserted. Every row except the `known_gap`
//! ones is gated. All calls into the coherence API live in `verdict`.
#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use anyhow::{Context, Result, ensure};
use assert_cmd::cargo_bin_cmd;
use dispatch::{
    Decision, DiffStats, SourceKind,
    coherence::{self, WorkView, world},
    source::{self, SourceSnapshot},
};
use serde_json::{Value, json};
use tempfile::TempDir;

type Files<'a> = &'a [(&'a str, &'a str)];

#[derive(Clone, Copy)]
enum Lang {
    Rust,
    Python,
    Text,
}

/// One change the world makes to the source after the run's snapshot.
#[derive(Clone, Copy)]
enum Op<'a> {
    Write(&'a str, &'a str),
    Delete(&'a str),
    /// Make an existing file executable (a mode-only change).
    Chmod(&'a str),
    /// Create or replace a symlink: (path, target).
    Symlink(&'a str, &'a str),
}

struct Scenario {
    name: &'static str,
    lang: Lang,
    s0: Files<'static>,
    /// Directories inside S0 that are their own Git repositories.
    nested_repos: &'static [&'static str],
    delta: Files<'static>,
    world: &'static [Op<'static>],
    expected: Decision,
    expected_l0: Decision,
    /// True exactly when `expected != expected_l0`.
    pending_symbols: bool,
    /// An honestly reported limitation of `observe`/`evaluate` for Git sources
    /// (Directory sources are still asserted), not asserted.
    known_gap: bool,
    note: &'static str,
}

/// The prepared world: source already moved, delta already collected.
struct Ctx {
    _root: TempDir,
    source: PathBuf,
    snapshot: SourceSnapshot,
    diff: PathBuf,
    stats: DiffStats,
    /// Whether the whole-tree fingerprint differs from the snapshot.
    strict_moved: bool,
    snapshot_ms: f64,
}

struct Verdict {
    decision: Decision,
    reason: String,
    changed: Vec<String>,
    observe_ms: f64,
    eval_ms: f64,
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

// ---------------------------------------------------------------------------
// Harness

fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .context("failed to run git")?;
    ensure!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn write_file(path: &Path, contents: &str) -> Result<()> {
    fs::create_dir_all(path.parent().context("path has no parent")?)?;
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

fn commit_all(dir: &Path) -> Result<()> {
    git(dir, &["init", "--quiet"])?;
    git(dir, &["add", "-A"])?;
    git(dir, &["commit", "--quiet", "-m", "initial"])
}

fn apply_world(source: &Path, world: &[Op]) -> Result<()> {
    for op in world {
        match *op {
            Op::Write(path, contents) => write_file(&source.join(path), contents)?,
            Op::Delete(path) => fs::remove_file(source.join(path))?,
            Op::Chmod(path) => {
                let file = source.join(path);
                let mode = fs::metadata(&file)?.permissions().mode();
                fs::set_permissions(&file, fs::Permissions::from_mode(mode | 0o111))?;
            }
            Op::Symlink(path, target) => {
                let link = source.join(path);
                fs::create_dir_all(link.parent().context("path has no parent")?)?;
                if fs::symlink_metadata(&link).is_ok() {
                    fs::remove_file(&link)?;
                }
                symlink(target, &link)?;
            }
        }
    }
    Ok(())
}

/// Build S0, snapshot it, collect the delta from a real candidate workspace,
/// and only then move the source.
fn build(git_source: bool, s0: Files, nested: &[&str], delta: Files, world: &[Op]) -> Result<Ctx> {
    let root = TempDir::new()?;
    let source = root.path().join("source");
    fs::create_dir_all(&source)?;
    let source = fs::canonicalize(source)?;
    for (path, contents) in s0 {
        write_file(&source.join(path), contents)?;
    }
    for directory in nested {
        commit_all(&source.join(directory))?;
    }
    if git_source {
        commit_all(&source)?;
    }

    let started = Instant::now();
    let snapshot = source::create_snapshot(&source, &root.path().join("run"))?;
    let snapshot_ms = ms(started);
    let workspace = source::create_candidate_workspace(
        &snapshot.baseline_path,
        &root.path().join("workspace"),
    )?;
    for (path, contents) in delta {
        write_file(&workspace.join(path), contents)?;
    }
    let diff = root.path().join("delta.patch");
    let stats = source::collect_diff(&snapshot.baseline_path, &workspace, &diff)?;
    ensure!(
        fs::metadata(&diff)?.len() > 0,
        "scenario delta produced an empty patch"
    );

    apply_world(&source, world)?;
    let strict_moved = source::fingerprint_tree(&source)? != snapshot.fingerprint;
    Ok(Ctx {
        _root: root,
        source,
        snapshot,
        diff,
        stats,
        strict_moved,
        snapshot_ms,
    })
}

/// The only place that calls the coherence API.
fn verdict(ctx: &Ctx) -> Result<Verdict> {
    let started = Instant::now();
    let observation = world::observe(
        &ctx.source,
        &ctx.snapshot.baseline_path,
        &ctx.snapshot.baseline_commit,
        &ctx.snapshot.kind,
    )?;
    let observe_ms = ms(started);
    let started = Instant::now();
    let validity = coherence::evaluate(
        &observation,
        &WorkView {
            source: &ctx.source,
            delta_patch: &ctx.diff,
            baseline: &ctx.snapshot.baseline_path,
            baseline_commit: &ctx.snapshot.baseline_commit,
        },
    )?;
    let eval_ms = ms(started);
    Ok(Verdict {
        decision: validity.decision,
        reason: validity
            .reasons
            .first()
            .map(|reason| format!("{:?}", reason.code))
            .unwrap_or_default(),
        changed: observation
            .changes
            .iter()
            .map(|change| change.path.clone())
            .collect(),
        observe_ms,
        eval_ms,
    })
}

fn strict_detector(ctx: &Ctx) -> Decision {
    if ctx.strict_moved {
        Decision::Refresh
    } else {
        Decision::Continue
    }
}

fn overlap_detector(ctx: &Ctx, changed: &[String]) -> Decision {
    let touched = |path: &String| {
        ctx.stats.changed_files.contains(path) || ctx.stats.untracked_files.contains(path)
    };
    if changed.iter().any(touched) {
        Decision::Refresh
    } else {
        Decision::Continue
    }
}

// ---------------------------------------------------------------------------
// Results

struct Row {
    name: &'static str,
    source: &'static str,
    lang: &'static str,
    strict: Option<Decision>,
    overlap: Option<Decision>,
    coherence: Option<Decision>,
    expected: Decision,
    expected_l0: Decision,
    pending: bool,
    known_gap: bool,
    reason: String,
    error: Option<String>,
    /// Paths `observe` reported as changed in the world.
    changed: Vec<String>,
    observe_ms: f64,
    eval_ms: f64,
    snapshot_ms: f64,
    note: &'static str,
}

impl Row {
    fn gated(&self) -> bool {
        !self.pending && !self.known_gap
    }

    fn passes(&self) -> bool {
        self.coherence == Some(self.expected_l0)
    }

    fn status(&self) -> &'static str {
        if self.known_gap {
            "known_gap"
        } else if self.pending {
            "pending"
        } else if self.passes() {
            "ok"
        } else {
            "FAIL"
        }
    }
}

fn short(decision: Option<Decision>) -> &'static str {
    match decision {
        Some(Decision::Continue) => "CONT",
        Some(Decision::Refresh) => "REFR",
        Some(Decision::Stop) => "STOP",
        None => "ERR",
    }
}

fn run_scenario(scenario: &Scenario, git_source: bool) -> Row {
    assert_eq!(
        scenario.pending_symbols,
        scenario.expected != scenario.expected_l0,
        "{}: pending_symbols must match expected != expected_l0",
        scenario.name
    );
    let mut row = Row {
        name: scenario.name,
        source: if git_source { "git" } else { "dir" },
        lang: match scenario.lang {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::Text => "text",
        },
        strict: None,
        overlap: None,
        coherence: None,
        expected: scenario.expected,
        expected_l0: scenario.expected_l0,
        pending: scenario.pending_symbols,
        known_gap: scenario.known_gap && git_source,
        reason: String::new(),
        error: None,
        changed: Vec::new(),
        observe_ms: 0.0,
        eval_ms: 0.0,
        snapshot_ms: 0.0,
        note: scenario.note,
    };
    let result = build(
        git_source,
        scenario.s0,
        scenario.nested_repos,
        scenario.delta,
        scenario.world,
    )
    .and_then(|ctx| verdict(&ctx).map(|verdict| (ctx, verdict)));
    match result {
        Ok((ctx, verdict)) => {
            row.strict = Some(strict_detector(&ctx));
            row.overlap = Some(overlap_detector(&ctx, &verdict.changed));
            row.coherence = Some(verdict.decision);
            row.reason = verdict.reason;
            row.changed = verdict.changed;
            row.observe_ms = verdict.observe_ms;
            row.eval_ms = verdict.eval_ms;
            row.snapshot_ms = ctx.snapshot_ms;
        }
        Err(error) => row.error = Some(format!("{error:#}")),
    }
    row
}

#[derive(Default)]
struct Counts {
    rows: usize,
    should_invalidate: usize,
    invalidated: usize,
    should_continue: usize,
    false_continue: usize,
    false_refresh: usize,
}

impl Counts {
    fn recall(&self) -> String {
        percent(self.invalidated, self.should_invalidate)
    }

    fn false_refresh_rate(&self) -> String {
        percent(self.false_refresh, self.should_continue)
    }
}

fn percent(part: usize, whole: usize) -> String {
    if whole == 0 {
        "n/a".into()
    } else {
        format!(
            "{:.0}% ({part}/{whole})",
            part as f64 * 100.0 / whole as f64
        )
    }
}

/// Score one detector against `target` (the verdict the finished design gives).
fn count(
    rows: &[&Row],
    detector: fn(&Row) -> Option<Decision>,
    target: fn(&Row) -> Decision,
) -> Counts {
    let mut counts = Counts::default();
    for row in rows {
        let Some(got) = detector(row) else { continue };
        counts.rows += 1;
        if target(row) == Decision::Continue {
            counts.should_continue += 1;
            counts.false_refresh += usize::from(got != Decision::Continue);
        } else {
            counts.should_invalidate += 1;
            counts.invalidated += usize::from(got != Decision::Continue);
            counts.false_continue += usize::from(got == Decision::Continue);
        }
    }
    counts
}

type Detector = fn(&Row) -> Option<Decision>;

const DETECTORS: [(&str, Detector); 3] = [
    ("strict today", |row| row.strict),
    ("file overlap", |row| row.overlap),
    ("coherence", |row| row.coherence),
];

fn print_table(rows: &[Row]) -> String {
    let mut table = format!(
        "{:<40} {:<4} {:<6} {:<6} {:<7} {:<9} {:<6} {:<6} {:<9} {:>5} {:>7} {:>7}\n",
        "scenario",
        "src",
        "lang",
        "strict",
        "overlap",
        "coherence",
        "expect",
        "l0exp",
        "status",
        "wchg",
        "eval ms",
        "obs ms"
    );
    for row in rows {
        table.push_str(&format!(
            "{:<40} {:<4} {:<6} {:<6} {:<7} {:<9} {:<6} {:<6} {:<9} {:>5} {:>7.1} {:>7.1}\n",
            row.name,
            row.source,
            row.lang,
            short(row.strict),
            short(row.overlap),
            short(row.coherence),
            short(Some(row.expected)),
            short(Some(row.expected_l0)),
            row.status(),
            row.changed.len(),
            row.eval_ms,
            row.observe_ms,
        ));
        if let Some(error) = &row.error {
            table.push_str(&format!("    ERROR: {error}\n"));
        }
        if row.known_gap || row.error.is_some() {
            table.push_str(&format!("    note: {}\n", row.note));
            table.push_str(&format!("    world changes seen: {:?}\n", row.changed));
        }
    }
    table
}

fn summary_block(title: &str, rows: &[&Row], target: fn(&Row) -> Decision) -> (String, String) {
    let mut text = format!("{title} ({} rows)\n", rows.len());
    text.push_str(&format!(
        "  {:<14} {:>16} {:>16} {:>6} {:>6}\n",
        "detector", "invalidation rcl", "false-refresh", "FC", "FR"
    ));
    let mut json_rows = Vec::new();
    for (name, detector) in DETECTORS {
        let counts = count(rows, detector, target);
        text.push_str(&format!(
            "  {:<14} {:>16} {:>16} {:>6} {:>6}\n",
            name,
            counts.recall(),
            counts.false_refresh_rate(),
            counts.false_continue,
            counts.false_refresh
        ));
        json_rows.push(json!({
            "detector": name,
            "rows": counts.rows,
            "should_invalidate": counts.should_invalidate,
            "invalidated": counts.invalidated,
            "should_continue": counts.should_continue,
            "false_continue": counts.false_continue,
            "false_refresh": counts.false_refresh,
        }));
    }
    (text, serde_json::Value::Array(json_rows).to_string())
}

fn report(rows: &[Row]) {
    let table = print_table(rows);
    let gated: Vec<&Row> = rows.iter().filter(|row| row.gated()).collect();
    let all: Vec<&Row> = rows.iter().collect();
    let (gated_text, gated_json) = summary_block(
        "Non-pending, non-known_gap rows, scored against expected_l0 (gated)",
        &gated,
        |row| row.expected_l0,
    );
    let (all_text, all_json) = summary_block(
        "ALL rows incl. pending, scored against the target `expected` (what WP6 must improve)",
        &all,
        |row| row.expected,
    );
    println!("\n{table}\n{gated_text}\n{all_text}");
    let pending: Vec<_> = rows.iter().filter(|row| row.pending).collect();
    println!("Pending rows (expected != L0):");
    for row in pending.iter().filter(|row| row.source == "git") {
        println!("  {:<40} {}", row.name, row.note);
    }

    let json_rows: Vec<_> = rows
        .iter()
        .map(|row| {
            json!({
                "scenario": row.name, "source": row.source, "lang": row.lang,
                "strict": short(row.strict), "overlap": short(row.overlap),
                "coherence": short(row.coherence),
                "expected": short(Some(row.expected)), "expected_l0": short(Some(row.expected_l0)),
                "status": row.status(), "reason": row.reason, "error": row.error,
                "world_changes": row.changed,
                "observe_ms": row.observe_ms, "eval_ms": row.eval_ms,
                "snapshot_ms": row.snapshot_ms, "note": row.note,
            })
        })
        .collect();
    let document = json!({
        "rows": json_rows,
        "summary_gated": serde_json::from_str::<serde_json::Value>(&gated_json).unwrap(),
        "summary_all": serde_json::from_str::<serde_json::Value>(&all_json).unwrap(),
    });
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("coherence-report.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    println!("JSON report: {}", path.display());
}

// ---------------------------------------------------------------------------
// Fixture contents (never compiled; they only need to look like real code)

const README: &str = "# demo\n\nA small demo project.\n";
const README_EDITED: &str = "# demo\n\nA small demo project.\n\nSee docs/ for details.\n";

const STATS: &str = "\
/// Sum of all values.
pub fn total(values: &[i64]) -> i64 {
    let mut sum = 0;
    for value in values {
        sum += value;
    }
    sum
}

// Helpers below do not depend on totals.
// They live in the same file for now.
// Keep them sorted by name.
// Add new ones at the end.

/// Largest value, if any.
pub fn largest(values: &[i64]) -> Option<i64> {
    let mut best = None;
    for value in values {
        if best.map_or(true, |b| *value > b) {
            best = Some(*value);
        }
    }
    best
}
";
/// Delta for `STATS`: skip negative values when summing.
const STATS_DELTA: &str = "\
/// Sum of all values.
pub fn total(values: &[i64]) -> i64 {
    let mut sum = 0;
    for value in values {
        if *value >= 0 {
            sum += value;
        }
    }
    sum
}

// Helpers below do not depend on totals.
// They live in the same file for now.
// Keep them sorted by name.
// Add new ones at the end.

/// Largest value, if any.
pub fn largest(values: &[i64]) -> Option<i64> {
    let mut best = None;
    for value in values {
        if best.map_or(true, |b| *value > b) {
            best = Some(*value);
        }
    }
    best
}
";
/// World edit far from `total`: a different comparison in `largest`.
const STATS_WORLD_LARGEST: &str = "\
/// Sum of all values.
pub fn total(values: &[i64]) -> i64 {
    let mut sum = 0;
    for value in values {
        sum += value;
    }
    sum
}

// Helpers below do not depend on totals.
// They live in the same file for now.
// Keep them sorted by name.
// Add new ones at the end.

/// Largest value, if any.
pub fn largest(values: &[i64]) -> Option<i64> {
    let mut best = None;
    for value in values {
        if best.map_or(true, |b| *value >= b) {
            best = Some(*value);
        }
    }
    best
}
";
/// World edit on the very line the delta edits, done differently.
const STATS_WORLD_SAME_LINE: &str = "\
/// Sum of all values.
pub fn total(values: &[i64]) -> i64 {
    let mut sum = 0;
    for value in values {
        sum = sum.saturating_add(*value);
    }
    sum
}

// Helpers below do not depend on totals.
// They live in the same file for now.
// Keep them sorted by name.
// Add new ones at the end.

/// Largest value, if any.
pub fn largest(values: &[i64]) -> Option<i64> {
    let mut best = None;
    for value in values {
        if best.map_or(true, |b| *value > b) {
            best = Some(*value);
        }
    }
    best
}
";
/// World edit far away that leaves the file unparsable (mid-edit save).
const STATS_WORLD_BROKEN: &str = "\
/// Sum of all values.
pub fn total(values: &[i64]) -> i64 {
    let mut sum = 0;
    for value in values {
        sum += value;
    }
    sum
}

// Helpers below do not depend on totals.
// They live in the same file for now.
// Keep them sorted by name.
// Add new ones at the end.

/// Largest value, if any.
pub fn largest(values: &[i64]) -> Option<i64> {
    let mut best = None;
    for value in values {
        if best.map_or(true, |b| *value > b {
            best = Some(*value;
    }
    best
";

const STATS_PY: &str = "\
def total(values):
    \"\"\"Sum of all values.\"\"\"
    result = 0
    for value in values:
        result += value
    return result


# Helpers below do not depend on totals.
# They live in the same file for now.
# Keep them sorted by name.
# Add new ones at the end.


def largest(values):
    \"\"\"Largest value, if any.\"\"\"
    best = None
    for value in values:
        if best is None or value > best:
            best = value
    return best
";
const STATS_PY_DELTA: &str = "\
def total(values):
    \"\"\"Sum of all values.\"\"\"
    result = 0
    for value in values:
        if value >= 0:
            result += value
    return result


# Helpers below do not depend on totals.
# They live in the same file for now.
# Keep them sorted by name.
# Add new ones at the end.


def largest(values):
    \"\"\"Largest value, if any.\"\"\"
    best = None
    for value in values:
        if best is None or value > best:
            best = value
    return best
";
const STATS_PY_WORLD: &str = "\
def total(values):
    \"\"\"Sum of all values.\"\"\"
    result = 0
    for value in values:
        result += value
    return result


# Helpers below do not depend on totals.
# They live in the same file for now.
# Keep them sorted by name.
# Add new ones at the end.


def largest(values):
    \"\"\"Largest value, if any.\"\"\"
    best = None
    for value in values:
        if best is None or value >= best:
            best = value
    return best
";

const UTIL: &str = "\
pub fn clamp(value: i64, low: i64, high: i64) -> i64 {
    value.max(low).min(high)
}
";
const UTIL_WORLD: &str = "\
pub fn clamp(value: i64, low: i64, high: i64) -> i64 {
    if value < low { low } else if value > high { high } else { value }
}
";

// Four functions, eight lines apart: the delta edits the odd ones and the
// world the even ones, so their hunks never share context lines.
const FOUR: &str = "\
pub fn f1(x: i64) -> i64 {
    x + 1
}

// -- section two --
// Each section is independent.
// Edits to one never need another.

pub fn f2(x: i64) -> i64 {
    x + 2
}

// -- section three --
// Each section is independent.
// Edits to one never need another.

pub fn f3(x: i64) -> i64 {
    x + 3
}

// -- section four --
// Each section is independent.
// Edits to one never need another.

pub fn f4(x: i64) -> i64 {
    x + 4
}
";
const FOUR_DELTA: &str = "\
pub fn f1(x: i64) -> i64 {
    x + 10
}

// -- section two --
// Each section is independent.
// Edits to one never need another.

pub fn f2(x: i64) -> i64 {
    x + 2
}

// -- section three --
// Each section is independent.
// Edits to one never need another.

pub fn f3(x: i64) -> i64 {
    x + 30
}

// -- section four --
// Each section is independent.
// Edits to one never need another.

pub fn f4(x: i64) -> i64 {
    x + 4
}
";
const FOUR_WORLD: &str = "\
pub fn f1(x: i64) -> i64 {
    x + 1
}

// -- section two --
// Each section is independent.
// Edits to one never need another.

pub fn f2(x: i64) -> i64 {
    x * 2
}

// -- section three --
// Each section is independent.
// Edits to one never need another.

pub fn f3(x: i64) -> i64 {
    x + 3
}

// -- section four --
// Each section is independent.
// Edits to one never need another.

pub fn f4(x: i64) -> i64 {
    x * 4
}
";
const FOUR_PY: &str = "\
def f1(x):
    return x + 1


# -- section two --
# Each section is independent.
# Edits to one never need another.


def f2(x):
    return x + 2


# -- section three --
# Each section is independent.
# Edits to one never need another.


def f3(x):
    return x + 3


# -- section four --
# Each section is independent.
# Edits to one never need another.


def f4(x):
    return x + 4
";
const FOUR_PY_DELTA: &str = "\
def f1(x):
    return x + 10


# -- section two --
# Each section is independent.
# Edits to one never need another.


def f2(x):
    return x + 2


# -- section three --
# Each section is independent.
# Edits to one never need another.


def f3(x):
    return x + 30


# -- section four --
# Each section is independent.
# Edits to one never need another.


def f4(x):
    return x + 4
";
const FOUR_PY_WORLD: &str = "\
def f1(x):
    return x + 1


# -- section two --
# Each section is independent.
# Edits to one never need another.


def f2(x):
    return x * 2


# -- section three --
# Each section is independent.
# Edits to one never need another.


def f3(x):
    return x + 3


# -- section four --
# Each section is independent.
# Edits to one never need another.


def f4(x):
    return x * 4
";

const CONFIG_RS: &str = "\
pub struct Config {
    pub retries: u32,
}

/// Milliseconds to wait between attempts.
pub fn backoff_ms(config: &Config) -> u64 {
    u64::from(config.retries) * 100
}
";
const CONFIG_RS_WORLD: &str = "\
pub struct Config {
    pub retries: u32,
}

/// Milliseconds to wait between attempts.
pub fn backoff_ms(config: &Config) -> u64 {
    u64::from(config.retries) * 250
}
";
const CLIENT_RS: &str = "\
use crate::config::Config;

pub fn fetch(url: &str, config: &Config) -> Result<Vec<u8>, String> {
    let mut last = String::new();
    for _attempt in 0..config.retries {
        match transport::get(url) {
            Ok(body) => return Ok(body),
            Err(error) => last = error,
        }
    }
    Err(last)
}
";
const CLIENT_RS_DELTA: &str = "\
use crate::config::{self, Config};
use std::{thread, time::Duration};

pub fn fetch(url: &str, config: &Config) -> Result<Vec<u8>, String> {
    let mut last = String::new();
    for _attempt in 0..config.retries {
        match transport::get(url) {
            Ok(body) => return Ok(body),
            Err(error) => last = error,
        }
        thread::sleep(Duration::from_millis(config::backoff_ms(config)));
    }
    Err(last)
}
";
const CONFIG_PY: &str = "\
class Config:
    def __init__(self, retries):
        self.retries = retries


def backoff_ms(config):
    \"\"\"Milliseconds to wait between attempts.\"\"\"
    return config.retries * 100
";
const CONFIG_PY_WORLD: &str = "\
class Config:
    def __init__(self, retries):
        self.retries = retries


def backoff_ms(config):
    \"\"\"Milliseconds to wait between attempts.\"\"\"
    return config.retries * 250
";
const CLIENT_PY: &str = "\
from config import Config


def fetch(url, config):
    last = None
    for _attempt in range(config.retries):
        try:
            return transport.get(url)
        except OSError as error:
            last = error
    raise last
";
const CLIENT_PY_DELTA: &str = "\
import time

from config import Config, backoff_ms


def fetch(url, config):
    last = None
    for _attempt in range(config.retries):
        try:
            return transport.get(url)
        except OSError as error:
            last = error
            time.sleep(backoff_ms(config) / 1000)
    raise last
";

const AUTH_RS: &str = "\
pub struct Token(pub String);
pub struct User {
    pub name: String,
}

pub fn validate(token: &Token) -> Result<User, AuthError> {
    let name = decode(&token.0)?;
    Ok(User { name })
}
";
const AUTH_RS_WORLD: &str = "\
pub struct Token(pub String);
pub struct User {
    pub name: String,
}

pub fn validate(ctx: &AuthContext, token: &Token) -> Result<User, AuthError> {
    let name = decode(ctx, &token.0)?;
    Ok(User { name })
}
";
const HANDLER_RS: &str = "\
use crate::auth::Token;

pub fn handle(request: &Request) -> Response {
    let token = Token(request.header(\"authorization\").to_owned());
    Response::ok(&token.0)
}
";
const HANDLER_RS_DELTA: &str = "\
use crate::auth::{self, Token};

pub fn handle(request: &Request) -> Response {
    let token = Token(request.header(\"authorization\").to_owned());
    match auth::validate(&token) {
        Ok(user) => Response::ok(&user.name),
        Err(_) => Response::unauthorized(),
    }
}
";
const AUTH_PY: &str = "\
class Token:
    def __init__(self, value):
        self.value = value


def validate(token):
    \"\"\"Return the user name for a token.\"\"\"
    return decode(token.value)
";
const AUTH_PY_WORLD: &str = "\
class Token:
    def __init__(self, value):
        self.value = value


def validate(ctx, token):
    \"\"\"Return the user name for a token.\"\"\"
    return decode(ctx, token.value)
";
const HANDLER_PY: &str = "\
from auth import Token


def handle(request):
    token = Token(request.headers[\"authorization\"])
    return response_ok(token.value)
";
const HANDLER_PY_DELTA: &str = "\
from auth import Token, validate


def handle(request):
    token = Token(request.headers[\"authorization\"])
    try:
        return response_ok(validate(token))
    except ValueError:
        return response_unauthorized()
";

const MODEL_RS: &str = "\
#[derive(Debug, Clone)]
pub struct Job {
    pub id: u64,
    pub name: String,
}
";
const MODEL_RS_WORLD: &str = "\
#[derive(Debug, Clone)]
pub struct Job {
    pub id: u64,
    pub name: String,
    pub priority: u8,
}
";
const QUEUE_RS: &str = "\
use crate::model::Job;

pub fn enqueue(queue: &mut Vec<Job>, name: &str) {
    let id = queue.len() as u64;
    let _ = name;
    let _ = id;
}
";
const QUEUE_RS_DELTA: &str = "\
use crate::model::Job;

pub fn enqueue(queue: &mut Vec<Job>, name: &str) {
    let id = queue.len() as u64;
    queue.push(Job {
        id,
        name: name.to_owned(),
    });
}
";
const MODEL_PY: &str = "\
from dataclasses import dataclass


@dataclass
class Job:
    id: int
    name: str
";
const MODEL_PY_WORLD: &str = "\
from dataclasses import dataclass


@dataclass
class Job:
    id: int
    name: str
    priority: int
";
const QUEUE_PY: &str = "\
from model import Job


def enqueue(queue, name):
    job_id = len(queue)
    return job_id
";
const QUEUE_PY_DELTA: &str = "\
from model import Job


def enqueue(queue, name):
    job_id = len(queue)
    queue.append(Job(id=job_id, name=name))
    return job_id
";

const RATES_RS: &str = "\
/// Price of one unit.
pub fn unit_rate() -> f64 {
    1.0
}
";
const RATES_RS_WORLD: &str = "\
/// Price of one unit.
pub fn unit_rate() -> f64 {
    1.2
}
";
const PRICING_RS: &str = "\
use crate::rates;

pub fn price(units: u32) -> f64 {
    rates::unit_rate() * f64::from(units) + 1.0
}
";
const MAIN_QUOTE_RS: &str = "\
mod pricing;
mod rates;

fn main() {
    println!(\"hello\");
}
";
const MAIN_QUOTE_RS_DELTA: &str = "\
mod pricing;
mod rates;

fn main() {
    let quote = pricing::price(3);
    println!(\"quote: {quote}\");
}
";

const SCHEMA_JSON: &str = "\
{
  \"type\": \"object\",
  \"properties\": {
    \"name\": { \"type\": \"string\" },
    \"email\": { \"type\": \"string\" }
  },
  \"required\": [\"name\"]
}
";
const SCHEMA_JSON_WORLD: &str = "\
{
  \"type\": \"object\",
  \"properties\": {
    \"name\": { \"type\": \"string\" },
    \"contact_email\": { \"type\": \"string\" }
  },
  \"required\": [\"name\"]
}
";
const LOADER_RS: &str = "\
/// Parse a user document described by schema/user.json.
pub fn load(text: &str) -> User {
    let value: serde_json::Value = serde_json::from_str(text).unwrap();
    User {
        name: value[\"name\"].as_str().unwrap().to_owned(),
    }
}
";
const LOADER_RS_DELTA: &str = "\
/// Parse a user document described by schema/user.json.
pub fn load(text: &str) -> User {
    let value: serde_json::Value = serde_json::from_str(text).unwrap();
    User {
        name: value[\"name\"].as_str().unwrap().to_owned(),
        email: value[\"email\"].as_str().map(str::to_owned),
    }
}

/// The schema this loader is written against.
pub const USER_SCHEMA: &str = include_str!(\"../schema/user.json\");
";

const SUMMARIZE: &str = "\
pub fn summarize(rows: &[Row]) -> Summary {
    let mut summary = Summary::default();
    for row in rows {
        summary.count += 1;
        summary.bytes += row.bytes;
    }
    summary.min = rows.iter().map(|row| row.bytes).min().unwrap_or(0);
    summary.max = rows.iter().map(|row| row.bytes).max().unwrap_or(0);
    summary.mean = if summary.count == 0 {
        0
    } else {
        summary.bytes / summary.count
    };
    summary.spread = summary.max - summary.min;
    summary.skewed = summary.mean > summary.min + summary.spread / 2;
    summary.empty = rows.is_empty();
    summary.label = format!(\"{} rows\", summary.count);
    summary
}
";
const SUMMARIZE_DELTA: &str = "\
pub fn summarize(rows: &[Row]) -> Summary {
    let mut summary = Summary::default();
    for row in rows {
        summary.count += 1;
        summary.bytes += row.bytes + row.overhead;
    }
    summary.min = rows.iter().map(|row| row.bytes).min().unwrap_or(0);
    summary.max = rows.iter().map(|row| row.bytes).max().unwrap_or(0);
    summary.mean = if summary.count == 0 {
        0
    } else {
        summary.bytes / summary.count
    };
    summary.spread = summary.max - summary.min;
    summary.skewed = summary.mean > summary.min + summary.spread / 2;
    summary.empty = rows.is_empty();
    summary.label = format!(\"{} rows\", summary.count);
    summary
}
";
const SUMMARIZE_WORLD: &str = "\
pub fn summarize(rows: &[Row]) -> Summary {
    let mut summary = Summary::default();
    for row in rows {
        summary.count += 1;
        summary.bytes += row.bytes;
    }
    summary.min = rows.iter().map(|row| row.bytes).min().unwrap_or(0);
    summary.max = rows.iter().map(|row| row.bytes).max().unwrap_or(0);
    summary.mean = if summary.count == 0 {
        0
    } else {
        summary.bytes / summary.count
    };
    summary.spread = summary.max - summary.min;
    summary.skewed = summary.mean > summary.min + summary.spread / 2;
    summary.empty = rows.is_empty();
    summary.label = format!(\"{} rows, {} bytes\", summary.count, summary.bytes);
    summary
}
";
const SUMMARIZE_PY: &str = "\
def summarize(rows):
    summary = Summary()
    for row in rows:
        summary.count += 1
        summary.bytes += row.bytes
    summary.min = min((row.bytes for row in rows), default=0)
    summary.max = max((row.bytes for row in rows), default=0)
    summary.mean = summary.bytes // summary.count if summary.count else 0
    summary.spread = summary.max - summary.min
    summary.skewed = summary.mean > summary.min + summary.spread // 2
    summary.empty = not rows
    summary.label = f\"{summary.count} rows\"
    return summary
";
const SUMMARIZE_PY_DELTA: &str = "\
def summarize(rows):
    summary = Summary()
    for row in rows:
        summary.count += 1
        summary.bytes += row.bytes + row.overhead
    summary.min = min((row.bytes for row in rows), default=0)
    summary.max = max((row.bytes for row in rows), default=0)
    summary.mean = summary.bytes // summary.count if summary.count else 0
    summary.spread = summary.max - summary.min
    summary.skewed = summary.mean > summary.min + summary.spread // 2
    summary.empty = not rows
    summary.label = f\"{summary.count} rows\"
    return summary
";
const SUMMARIZE_PY_WORLD: &str = "\
def summarize(rows):
    summary = Summary()
    for row in rows:
        summary.count += 1
        summary.bytes += row.bytes
    summary.min = min((row.bytes for row in rows), default=0)
    summary.max = max((row.bytes for row in rows), default=0)
    summary.mean = summary.bytes // summary.count if summary.count else 0
    summary.spread = summary.max - summary.min
    summary.skewed = summary.mean > summary.min + summary.spread // 2
    summary.empty = not rows
    summary.label = f\"{summary.count} rows, {summary.bytes} bytes\"
    return summary
";

const RENDER_RS: &str = "\
/// Render one table row.
pub fn render_row(cells: &[String]) -> String {
    cells.join(\" | \")
}
";
const RENDER_RS_WORLD: &str = "\
/// Render one table row.
pub fn render_row(cells: &[String], width: usize) -> String {
    cells
        .iter()
        .map(|cell| format!(\"{cell:<width$}\"))
        .collect::<Vec<_>>()
        .join(\" | \")
}
";
const REPORT_RS_NEW: &str = "\
use crate::render::render_row;

pub fn report(rows: &[Vec<String>]) -> String {
    let lines: Vec<String> = rows.iter().map(|row| render_row(row)).collect();
    lines.join(\"\\n\")
}
";

const APP_RS: &str = "\
use crate::billing;

pub fn run(order: &Order) -> Receipt {
    let receipt = billing::process(order);
    receipt
}
";
const APP_RS_DELTA: &str = "\
use crate::billing;

pub fn run(order: &Order) -> Receipt {
    let receipt = billing::process(order);
    println!(\"billed order {}\", order.id);
    receipt
}
";
const BILLING_RS: &str = "\
pub fn process(order: &Order) -> Receipt {
    Receipt::for_total(order.total())
}
";
const AUDIT_RS: &str = "\
pub fn process(entry: &Entry) -> Verdict {
    Verdict::from_flags(entry.flags())
}
";
const AUDIT_RS_WORLD: &str = "\
pub fn process(entry: &Entry, strict: bool) -> Verdict {
    Verdict::from_flags(entry.flags(), strict)
}
";

const TABLE_RS: &str = "\
pub struct Table {
    pub rows: Vec<Vec<String>>,
}

pub fn width(table: &Table) -> usize {
    table.rows.iter().map(|row| row.len()).max().unwrap_or(0)
}

pub fn height(table: &Table) -> usize {
    table.rows.len()
}

pub fn transpose(table: &Table) -> Table {
    let mut columns = vec![Vec::new(); width(table)];
    for row in &table.rows {
        for (index, cell) in row.iter().enumerate() {
            columns[index].push(cell.clone());
        }
    }
    Table { rows: columns }
}

// Everything above is layout; everything below is lookup.
// Keep the two halves apart.
// Do not add new helpers between them.
// New lookups go at the very end.

pub fn last_row(table: &Table) -> Option<&Vec<String>> {
    let rows = &table.rows;
    if rows.is_empty() {
        return None;
    }
    rows.last()
}
";
const TABLE_RS_DELTA: &str = "\
pub struct Table {
    pub rows: Vec<Vec<String>>,
}

pub fn width(table: &Table) -> usize {
    table.rows.iter().map(|row| row.len()).max().unwrap_or(0)
}

pub fn height(table: &Table) -> usize {
    table.rows.len()
}

pub fn transpose(table: &Table) -> Table {
    let mut columns = vec![Vec::new(); width(table)];
    for row in &table.rows {
        for (index, cell) in row.iter().enumerate() {
            columns[index].push(cell.clone());
        }
    }
    Table { rows: columns }
}

// Everything above is layout; everything below is lookup.
// Keep the two halves apart.
// Do not add new helpers between them.
// New lookups go at the very end.

pub fn last_row(table: &Table) -> Option<&Vec<String>> {
    let rows = &table.rows;
    if rows.is_empty() {
        return None;
    }
    rows.last()
}

pub fn banner() -> &'static str {
    \"table v2\"
}
";
const TABLE_RS_WORLD: &str = "\
pub struct Table {
    pub rows: Vec<Vec<String>>,
    pub header: Option<Vec<String>>,
}

pub fn width(table: &Table, include_header: bool) -> usize {
    let body = table.rows.iter().map(|row| row.len()).max().unwrap_or(0);
    match (&table.header, include_header) {
        (Some(header), true) => body.max(header.len()),
        _ => body,
    }
}

pub fn height(table: &Table, include_header: bool) -> usize {
    table.rows.len() + usize::from(include_header && table.header.is_some())
}

pub fn transpose(table: &Table) -> Table {
    let mut columns = vec![Vec::new(); width(table, false)];
    for row in &table.rows {
        for (index, cell) in row.iter().enumerate() {
            columns[index].push(cell.clone());
        }
    }
    Table {
        rows: columns,
        header: None,
    }
}

// Everything above is layout; everything below is lookup.
// Keep the two halves apart.
// Do not add new helpers between them.
// New lookups go at the very end.

pub fn last_row(table: &Table) -> Option<&Vec<String>> {
    let rows = &table.rows;
    if rows.is_empty() {
        return None;
    }
    rows.last()
}
";

const STORE_PY: &str = "\
class Store:
    def __init__(self, path):
        self.path = path
        self.data = {}

    def save(self, key, value):
        self.data[key] = value

    def load(self, key):
        return self.data[key]
";
const STORE_PY_WORLD: &str = "\
class Store:
    def __init__(self, path):
        self.path = path
        self.data = {}

    def save(self, key, value, ttl):
        self.data[key] = (value, ttl)

    def load(self, key):
        return self.data[key]
";
const SERVICE_PY: &str = "\
from store import Store


def persist(path, items):
    store = Store(path)
    for key, value in items:
        pass
    return store
";
const SERVICE_PY_DELTA: &str = "\
from store import Store


def persist(path, items):
    store = Store(path)
    for key, value in items:
        store.save(key, value)
    return store
";

const STORE22_PY: &str = "\
class Store:
    def __init__(self):
        self.data = {}

    def save(self, key, value):
        self.data[key] = value

    def load(self, key):
        return self.data[key]

    def drop(self, key):
        return self.data.pop(key, None)
";
const STORE22_PY_DELTA: &str = "\
class Store:
    def __init__(self):
        self.data = {}

    def save(self, key, value):
        self.data[key] = (value, 1)

    def load(self, key):
        return self.data[key]

    def drop(self, key):
        return self.data.pop(key, None)
";
const STORE22_PY_WORLD: &str = "\
class Store:
    def __init__(self):
        self.data = {}

    def save(self, key, value):
        self.data[key] = value

    def load(self, key):
        return self.data[key]

    def drop(self, key):
        del self.data[key]
";

const AB_RS: &str = "\
pub fn a(x: i64) -> i64 {
    let y = x + 1;
    y * 2
}
pub fn b(x: i64) -> i64 {
    let mut total = x;
    total += 1;
    total += 2;
    total += 3;
    total += 4;
    total += 5;
    total
}
";
const AB_RS_DELTA: &str = "\
pub fn a(x: i64) -> i64 {
    let y = x + 1;
    y * 3
}
pub fn b(x: i64) -> i64 {
    let mut total = x;
    total += 1;
    total += 2;
    total += 3;
    total += 4;
    total += 5;
    total
}
";
const AB_RS_WORLD: &str = "\
pub fn a(x: i64) -> i64 {
    let y = x + 1;
    y * 2
}
pub fn b(x: i64) -> i64 {
    let mut total = x;
    total += 1;
    total += 2;
    total += 3;
    total += 4;
    total += 50;
    total
}
";

const APP24_RS: &str = "\
pub fn run(order: &Order) -> Receipt {
    Receipt::empty(order.id)
}
";
const APP24_RS_DELTA: &str = "\
fn process(order: &Order) -> Receipt {
    Receipt::for_total(order.total())
}

pub fn run(order: &Order) -> Receipt {
    process(order)
}
";
const BILLING24_RS: &str = "\
pub fn process(job: &Job) -> Outcome {
    Outcome::done(job.id)
}
";
const BILLING24_RS_WORLD: &str = "\
pub fn process(job: &Job, retries: u32) -> Outcome {
    Outcome::done_after(job.id, retries)
}
";

const SHAPE_RS: &str = "\
pub trait Shape {
    fn area(&self) -> f64;

    fn describe(&self) -> String {
        let area = self.area();
        format!(\"shape with area {area}\")
    }

    // Keep the required methods above and the provided ones below.
    // New provided methods go after this comment.
    // Do not reorder.
    // Thanks.
    fn is_large(&self) -> bool {
        self.area() > 100.0
    }
}
";
const SHAPE_RS_DELTA: &str = "\
pub trait Shape {
    fn area(&self) -> f64;

    fn describe(&self) -> String {
        let area = self.area();
        format!(\"a shape with area {area:.2}\")
    }

    // Keep the required methods above and the provided ones below.
    // New provided methods go after this comment.
    // Do not reorder.
    // Thanks.
    fn is_large(&self) -> bool {
        self.area() > 100.0
    }
}
";
const SHAPE_RS_WORLD: &str = "\
pub trait Shape {
    fn area(&self) -> f64;

    fn describe(&self) -> String {
        let area = self.area();
        format!(\"shape with area {area}\")
    }

    // Keep the required methods above and the provided ones below.
    // New provided methods go after this comment.
    // Do not reorder.
    // Thanks.
    fn is_large(&self) -> bool {
        self.area() > 100.0
    }

    fn perimeter(&self) -> f64;
}
";

const STORE26_RS: &str = "\
pub struct Store {
    pub data: Vec<u8>,
}

impl Store {
    pub fn save(&mut self, value: u8) {
        self.data.push(value);
        self.data.push(0);
    }

    // The two halves below are independent.
    // Callers use one or the other.
    // Keep them apart.
    // Really.
    pub fn drop(&mut self) {
        self.data.clear();
    }
}
";
const STORE26_RS_DELTA: &str = "\
pub struct Store {
    pub data: Vec<u8>,
}

impl Store {
    pub fn save(&mut self, value: u8) {
        self.data.push(value);
        self.data.push(1);
    }

    // The two halves below are independent.
    // Callers use one or the other.
    // Keep them apart.
    // Really.
    pub fn drop(&mut self) {
        self.data.clear();
    }
}
";
const STORE26_RS_WORLD: &str = "\
pub struct Store {
    pub data: Vec<u8>,
}

impl Store {
    pub fn save(&mut self, value: u8) {
        self.data.push(value);
        self.data.push(0);
    }

    // The two halves below are independent.
    // Callers use one or the other.
    // Keep them apart.
    // Really.
    pub fn drop(&mut self, keep: usize) {
        self.data.truncate(keep);
    }
}
";

const IGNORE: &str = "target/\n__pycache__/\n";
const LOGO_OLD: &str = "\u{0}PNG-old\u{1}\u{2}";
const LOGO_NEW: &str = "\u{0}PNG-new\u{1}\u{3}\u{4}";
const RUN_SH: &str = "#!/bin/sh\necho running\n";
const VENDOR_LIB: &str = "pub fn vendored() -> u8 {\n    1\n}\n";
const VENDOR_LIB_WORLD: &str = "pub fn vendored() -> u8 {\n    2\n}\n";

// ---------------------------------------------------------------------------
// The matrix

use Decision::{Continue, Refresh, Stop};

/// A scenario whose target verdict is what the evaluator must give (L0 + L1).
fn settled(
    name: &'static str,
    lang: Lang,
    s0: Files<'static>,
    delta: Files<'static>,
    world: &'static [Op<'static>],
    expected: Decision,
    note: &'static str,
) -> Scenario {
    Scenario {
        name,
        lang,
        s0,
        nested_repos: &[],
        delta,
        world,
        expected,
        expected_l0: expected,
        pending_symbols: false,
        known_gap: false,
        note,
    }
}

fn scenarios() -> Vec<Scenario> {
    let rust = Lang::Rust;
    let py = Lang::Python;
    vec![
        settled(
            "01 unrelated doc change",
            Lang::Text,
            &[("README.md", README), ("src/stats.rs", STATS)],
            &[("src/stats.rs", STATS_DELTA)],
            &[Op::Write("README.md", README_EDITED)],
            Continue,
            "README edited; the delta touches only code.",
        ),
        settled(
            "02 unrelated code file changed",
            rust,
            &[("src/stats.rs", STATS), ("src/util.rs", UTIL)],
            &[("src/stats.rs", STATS_DELTA)],
            &[Op::Write("src/util.rs", UTIL_WORLD)],
            Continue,
            "World edits util.rs; delta edits stats.rs.",
        ),
        settled(
            "03 same file, unrelated symbol",
            rust,
            &[("src/stats.rs", STATS)],
            &[("src/stats.rs", STATS_DELTA)],
            &[Op::Write("src/stats.rs", STATS_WORLD_LARGEST)],
            Continue,
            "Same file, far-apart functions; file overlap falsely refreshes.",
        ),
        settled(
            "03 same file, unrelated symbol",
            py,
            &[("stats.py", STATS_PY)],
            &[("stats.py", STATS_PY_DELTA)],
            &[Op::Write("stats.py", STATS_PY_WORLD)],
            Continue,
            "Python variant of row 3.",
        ),
        settled(
            "04 callee body changed, same sig",
            rust,
            &[("src/config.rs", CONFIG_RS), ("src/client.rs", CLIENT_RS)],
            &[("src/client.rs", CLIENT_RS_DELTA)],
            &[Op::Write("src/config.rs", CONFIG_RS_WORLD)],
            Continue,
            "Delta calls backoff_ms; only its body changes underneath.",
        ),
        settled(
            "04 callee body changed, same sig",
            py,
            &[("config.py", CONFIG_PY), ("client.py", CLIENT_PY)],
            &[("client.py", CLIENT_PY_DELTA)],
            &[Op::Write("config.py", CONFIG_PY_WORLD)],
            Continue,
            "Python variant of row 4.",
        ),
        settled(
            "05 callee signature changed",
            rust,
            &[("src/auth.rs", AUTH_RS), ("src/handler.rs", HANDLER_RS)],
            &[("src/handler.rs", HANDLER_RS_DELTA)],
            &[Op::Write("src/auth.rs", AUTH_RS_WORLD)],
            Refresh,
            "Delta calls validate(&token); world adds a ctx parameter. False CONTINUE for L0 and file overlap.",
        ),
        settled(
            "05 callee signature changed",
            py,
            &[("auth.py", AUTH_PY), ("handler.py", HANDLER_PY)],
            &[("handler.py", HANDLER_PY_DELTA)],
            &[Op::Write("auth.py", AUTH_PY_WORLD)],
            Refresh,
            "Python variant of row 5.",
        ),
        settled(
            "06 shared struct field added",
            rust,
            &[("src/model.rs", MODEL_RS), ("src/queue.rs", QUEUE_RS)],
            &[("src/queue.rs", QUEUE_RS_DELTA)],
            &[Op::Write("src/model.rs", MODEL_RS_WORLD)],
            Refresh,
            "Delta constructs Job {id, name}; world adds a required field.",
        ),
        settled(
            "06 shared struct field added",
            py,
            &[("model.py", MODEL_PY), ("queue.py", QUEUE_PY)],
            &[("queue.py", QUEUE_PY_DELTA)],
            &[Op::Write("model.py", MODEL_PY_WORLD)],
            Refresh,
            "Python variant of row 6.",
        ),
        settled(
            "07 transitive behavior change",
            rust,
            &[
                ("src/rates.rs", RATES_RS),
                ("src/pricing.rs", PRICING_RS),
                ("src/main.rs", MAIN_QUOTE_RS),
            ],
            &[("src/main.rs", MAIN_QUOTE_RS_DELTA)],
            &[Op::Write("src/rates.rs", RATES_RS_WORLD)],
            Continue,
            "Delta calls price -> unit_rate, whose behavior changed. Symbol facts see nothing; an integration-check oracle (L2) catches it later.",
        ),
        settled(
            "08 contract file (json schema)",
            rust,
            &[
                ("schema/user.json", SCHEMA_JSON),
                ("src/loader.rs", LOADER_RS),
            ],
            &[("src/loader.rs", LOADER_RS_DELTA)],
            &[Op::Write("schema/user.json", SCHEMA_JSON_WORLD)],
            Refresh,
            "Delta relies on the `email` property; the world renames it. Needs a file-level fact.",
        ),
        settled(
            "09 identical patch already landed",
            rust,
            &[("src/stats.rs", STATS)],
            &[("src/stats.rs", STATS_DELTA)],
            &[Op::Write("src/stats.rs", STATS_DELTA)],
            Stop,
            "Another Work applied the same patch byte for byte.",
        ),
        settled(
            "10 same goal, different lines",
            rust,
            &[("src/stats.rs", STATS)],
            &[("src/stats.rs", STATS_DELTA)],
            &[Op::Write("src/stats.rs", STATS_WORLD_SAME_LINE)],
            Refresh,
            "Both sides rewrite the same line differently: PatchConflict.",
        ),
        settled(
            "11 same symbol, disjoint hunks",
            rust,
            &[("src/summary.rs", SUMMARIZE)],
            &[("src/summary.rs", SUMMARIZE_DELTA)],
            &[Op::Write("src/summary.rs", SUMMARIZE_WORLD)],
            Refresh,
            "Both edit summarize() in far-apart lines; git merges cleanly but the symbol was edited twice.",
        ),
        settled(
            "11 same symbol, disjoint hunks",
            py,
            &[("summary.py", SUMMARIZE_PY)],
            &[("summary.py", SUMMARIZE_PY_DELTA)],
            &[Op::Write("summary.py", SUMMARIZE_PY_WORLD)],
            Refresh,
            "Python variant of row 11.",
        ),
        settled(
            "12 safe overlapping edits",
            rust,
            &[("src/four.rs", FOUR)],
            &[("src/four.rs", FOUR_DELTA)],
            &[Op::Write("src/four.rs", FOUR_WORLD)],
            Continue,
            "Delta edits f1,f3; world edits f2,f4 in the same file.",
        ),
        settled(
            "12 safe overlapping edits",
            py,
            &[("four.py", FOUR_PY)],
            &[("four.py", FOUR_PY_DELTA)],
            &[Op::Write("four.py", FOUR_PY_WORLD)],
            Continue,
            "Python variant of row 12.",
        ),
        settled(
            "13 ignored build dir changed",
            rust,
            &[
                (".gitignore", IGNORE),
                ("src/stats.rs", STATS),
                ("target/debug/app.bin", "old build\n"),
            ],
            &[("src/stats.rs", STATS_DELTA)],
            &[
                Op::Write("target/debug/app.bin", "new build\n"),
                Op::Write("target/debug/extra.o", "object\n"),
                Op::Write("src/__pycache__/x.pyc", "bytecode\n"),
            ],
            Continue,
            "Ignored build output moves; strict today refuses. Directory sources cannot honor .gitignore, so `dir` sees changes but the patch still applies.",
        ),
        settled(
            "14 world file has syntax error",
            rust,
            &[("src/stats.rs", STATS)],
            &[("src/stats.rs", STATS_DELTA)],
            &[Op::Write("src/stats.rs", STATS_WORLD_BROKEN)],
            Refresh,
            "Mid-edit save breaks an unrelated function in the file the delta edits. Accept-time AnalysisUncertain: a false REFRESH accepted by design (an unparsable file cannot be checked). L0 alone still Continues, and the mid-run watcher must not flip on this reason.",
        ),
        settled(
            "15a file deleted by the world",
            rust,
            &[("src/stats.rs", STATS)],
            &[("src/stats.rs", STATS_DELTA)],
            &[Op::Delete("src/stats.rs")],
            Refresh,
            "The file the delta modifies no longer exists.",
        ),
        settled(
            "15b file renamed by the world",
            rust,
            &[("src/stats.rs", STATS)],
            &[("src/stats.rs", STATS_DELTA)],
            &[
                Op::Delete("src/stats.rs"),
                Op::Write("src/metrics.rs", STATS),
            ],
            Refresh,
            "The file the delta modifies moved to a new path.",
        ),
        settled(
            "16a unrelated binary changed",
            Lang::Text,
            &[("src/stats.rs", STATS), ("assets/logo.bin", LOGO_OLD)],
            &[("src/stats.rs", STATS_DELTA)],
            &[Op::Write("assets/logo.bin", LOGO_NEW)],
            Continue,
            "A binary asset unrelated to the delta changes.",
        ),
        settled(
            "16b unrelated mode-only change",
            Lang::Text,
            &[("src/stats.rs", STATS), ("scripts/run.sh", RUN_SH)],
            &[("src/stats.rs", STATS_DELTA)],
            &[Op::Chmod("scripts/run.sh")],
            Continue,
            "chmod +x on an unrelated script.",
        ),
        settled(
            "16c unrelated symlink added",
            Lang::Text,
            &[("README.md", README), ("src/stats.rs", STATS)],
            &[("src/stats.rs", STATS_DELTA)],
            &[Op::Symlink("docs/latest.md", "../README.md")],
            Continue,
            "A new symlink unrelated to the delta.",
        ),
        Scenario {
            nested_repos: &["vendor/dep"],
            ..settled(
                "17a nested git repo, edited",
                rust,
                &[("src/stats.rs", STATS), ("vendor/dep/lib.rs", VENDOR_LIB)],
                &[("src/stats.rs", STATS_DELTA)],
                &[Op::Write("vendor/dep/lib.rs", VENDOR_LIB_WORLD)],
                Continue,
                "A committed nested repo is a gitlink in the outer index; its files are not comparable, so an edit inside it is not world drift.",
            )
        },
        Scenario {
            nested_repos: &["vendor/dep"],
            ..settled(
                "17b nested git repo, untouched",
                rust,
                &[("src/stats.rs", STATS), ("vendor/dep/lib.rs", VENDOR_LIB)],
                &[("src/stats.rs", STATS_DELTA)],
                &[],
                Continue,
                "The world is unchanged: the nested repo's files are not reported Deleted, so the unchanged-world fast path fires.",
            )
        },
        settled(
            "18 new file calls changed callee",
            rust,
            &[("src/render.rs", RENDER_RS)],
            &[("src/report.rs", REPORT_RS_NEW)],
            &[Op::Write("src/render.rs", RENDER_RS_WORLD)],
            Refresh,
            "The delta only adds report.rs, which calls render_row; the world adds a required width parameter to render_row in another file.",
        ),
        settled(
            "19 name collision, other symbol changed",
            rust,
            &[
                ("src/app.rs", APP_RS),
                ("src/billing.rs", BILLING_RS),
                ("src/audit.rs", AUDIT_RS),
            ],
            &[("src/app.rs", APP_RS_DELTA)],
            &[Op::Write("src/audit.rs", AUDIT_RS_WORLD)],
            Continue,
            "Two unrelated functions are named process; the world changes audit's signature. The unique-name rule must not bind the delta's call to it.",
        ),
        settled(
            "20 new function, big rewrite elsewhere",
            rust,
            &[("src/table.rs", TABLE_RS)],
            &[("src/table.rs", TABLE_RS_DELTA)],
            &[Op::Write("src/table.rs", TABLE_RS_WORLD)],
            Continue,
            "The delta appends a function with no references; the world rewrites the top of the same file, signatures included.",
        ),
        settled(
            "21 class method signature changed",
            py,
            &[("store.py", STORE_PY), ("service.py", SERVICE_PY)],
            &[("service.py", SERVICE_PY_DELTA)],
            &[Op::Write("store.py", STORE_PY_WORLD)],
            Refresh,
            "The delta calls Store.save; the world adds a required ttl parameter to that method.",
        ),
        settled(
            "22 sibling method edited in a class",
            py,
            &[("store.py", STORE22_PY)],
            &[("store.py", STORE22_PY_DELTA)],
            &[Op::Write("store.py", STORE22_PY_WORLD)],
            Continue,
            "The delta edits Store.save; the world edits Store.drop in the same class. Only the class-level header is the class's own.",
        ),
        settled(
            "23 edit on the line above a neighbour",
            rust,
            &[("src/ab.rs", AB_RS)],
            &[("src/ab.rs", AB_RS_DELTA)],
            &[Op::Write("src/ab.rs", AB_RS_WORLD)],
            Continue,
            "The delta edits the last line of a; b starts right below and the world edits b's body further down. Context lines must not make b modified.",
        ),
        settled(
            "24 new local function shares a baseline name",
            rust,
            &[("src/app.rs", APP24_RS), ("src/billing.rs", BILLING24_RS)],
            &[("src/app.rs", APP24_RS_DELTA)],
            &[Op::Write("src/billing.rs", BILLING24_RS_WORLD)],
            Continue,
            "The delta declares its own process(); an unrelated baseline process() elsewhere changes signature. The work's own names are not bound to the baseline.",
        ),
        settled(
            "25 trait default method vs new trait method",
            rust,
            &[("src/shape.rs", SHAPE_RS)],
            &[("src/shape.rs", SHAPE_RS_DELTA)],
            &[Op::Write("src/shape.rs", SHAPE_RS_WORLD)],
            Continue,
            "The delta edits a default method body; the world adds a new method to the same trait. A trait is checked by its header, not its members.",
        ),
        settled(
            "26 impl sibling signature changed",
            rust,
            &[("src/store.rs", STORE26_RS)],
            &[("src/store.rs", STORE26_RS_DELTA)],
            &[Op::Write("src/store.rs", STORE26_RS_WORLD)],
            Continue,
            "The delta edits Store::save; the world changes the signature of Store::drop in the same impl.",
        ),
    ]
}

/// Run every scenario as a Git source and as a plain Directory source.
fn run_matrix() -> Vec<Row> {
    let matrix = scenarios();
    let mut rows = Vec::new();
    for scenario in &matrix {
        for git_source in [true, false] {
            rows.push(run_scenario(scenario, git_source));
        }
    }
    rows
}

fn assert_mention_case(
    s0: Files<'_>,
    delta: Files<'_>,
    changed_path: &str,
    expected: Decision,
    expected_origin: Option<&str>,
) -> Result<()> {
    for git_source in [true, false] {
        let ctx = build(
            git_source,
            s0,
            &[],
            delta,
            &[Op::Write(changed_path, "changed underneath\n")],
        )?;
        let verdict = verdict(&ctx)?;
        assert_eq!(
            verdict.decision,
            expected,
            "{} source: {}",
            if git_source { "Git" } else { "directory" },
            verdict.reason
        );
        if expected_origin == Some("mentioned") {
            assert!(
                !verdict.reason.is_empty(),
                "a changed weak mention must keep a visible reason"
            );
        }
        let patch = fs::read(&ctx.diff)?;
        let facts = coherence::facts::derive_facts(
            &WorkView {
                source: &ctx.source,
                delta_patch: &ctx.diff,
                baseline: &ctx.snapshot.baseline_path,
                baseline_commit: &ctx.snapshot.baseline_commit,
            },
            &coherence::facts::parse_patch(&patch),
        )?;
        let origin = facts
            .facts
            .iter()
            .find(|fact| fact.path == changed_path)
            .map(|fact| serde_json::to_value(fact.origin))
            .transpose()?;
        assert_eq!(
            origin,
            expected_origin.map(|origin| json!(origin)),
            "{} source, {changed_path}",
            if git_source { "Git" } else { "directory" }
        );
    }
    Ok(())
}

const WORKFLOW_S0: &str = "name: CI\njobs:\n  test:\n    runs-on: ubuntu-latest\n";
const WORKFLOW_DELTA: &str = "name: CI\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo \"${{ hashFiles('Cargo.lock') }}\"\n";

#[test]
fn rust_comment_naming_an_old_plan_is_not_a_dependency() -> Result<()> {
    assert_mention_case(
        &[
            ("docs/plan-0.4.14.md", "original plan\n"),
            ("src/lib.rs", "pub fn work() -> u8 {\n    1\n}\n"),
        ],
        &[(
            "src/lib.rs",
            "pub fn work() -> u8 {\n    // See docs/plan-0.4.14.md.\n    1\n}\n",
        )],
        "docs/plan-0.4.14.md",
        Continue,
        None,
    )
}

#[test]
fn rust_doc_comment_naming_an_old_plan_is_not_a_dependency() -> Result<()> {
    assert_mention_case(
        &[
            ("docs/plan-0.4.14.md", "original plan\n"),
            ("src/lib.rs", "pub fn work() -> u8 {\n    1\n}\n"),
        ],
        &[(
            "src/lib.rs",
            "/// See docs/plan-0.4.14.md.\npub fn work() -> u8 {\n    1\n}\n",
        )],
        "docs/plan-0.4.14.md",
        Continue,
        None,
    )
}

#[test]
fn workflow_hashfiles_mention_is_weak() -> Result<()> {
    assert_mention_case(
        &[
            ("Cargo.lock", "original lock\n"),
            (".github/workflows/ci.yml", WORKFLOW_S0),
        ],
        &[(".github/workflows/ci.yml", WORKFLOW_DELTA)],
        "Cargo.lock",
        Continue,
        Some("mentioned"),
    )
}

struct WorkflowRun {
    _root: TempDir,
    source: PathBuf,
    state: PathBuf,
    run_id: String,
}

impl WorkflowRun {
    fn dispatch(&self, args: &[&str]) -> Result<std::process::Output> {
        Ok(cargo_bin_cmd!("dispatch")
            .arg("--state-dir")
            .arg(&self.state)
            .current_dir(&self.source)
            .args(args)
            .output()?)
    }
}

fn workflow_run(checks: &str) -> Result<WorkflowRun> {
    let root = TempDir::new()?;
    let source = root.path().join("source");
    let state = root.path().join("state");
    let workflow = source.join(".github/workflows/ci.yml");
    write_file(&workflow, WORKFLOW_S0)?;
    write_file(&source.join("Cargo.lock"), "original lock\n")?;
    let agent = root.path().join("agent");
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'fixture 1.0\\n'; exit 0; fi\nmkdir -p .github/workflows\ncat > .github/workflows/ci.yml <<'YAML'\n{WORKFLOW_DELTA}YAML\nprintf '{{\"type\":\"result\",\"model\":\"fixture\"}}\\n'\n"
    );
    fs::write(&agent, script)?;
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755))?;
    write_file(
        &source.join("dispatch.yml"),
        &format!(
            "execution:\n  timeout_secs: 600\n{checks}harnesses:\n  cursor:\n    executable: \"{}\"\n",
            agent.display()
        ),
    )?;
    let run = cargo_bin_cmd!("dispatch")
        .arg("--state-dir")
        .arg(&state)
        .arg("run")
        .arg(&source)
        .args([
            "--task",
            "Update the workflow.",
            "--agent",
            "cursor",
            "--allow-unsafe-local",
            "--json",
        ])
        .output()?;
    ensure!(
        run.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let run: Value = serde_json::from_slice(&run.stdout)?;
    let run_id = run["run_id"].as_str().context("run has no ID")?;
    fs::write(source.join("Cargo.lock"), "changed underneath\n")?;
    Ok(WorkflowRun {
        _root: root,
        source,
        state,
        run_id: run_id.to_owned(),
    })
}

#[test]
fn workflow_mention_without_checks_requires_refresh() -> Result<()> {
    let fixture = workflow_run("")?;
    let check = fixture.dispatch(&["check", &fixture.run_id, "--json"])?;
    ensure!(
        check.status.success(),
        "check failed: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    let check: Value = serde_json::from_slice(&check.stdout)?;
    assert_eq!(check["validity"]["decision"], "refresh", "{check}");
    assert!(
        check["validity"]["reasons"]
            .as_array()
            .is_some_and(|reasons| reasons.iter().any(|reason| reason["path"] == "Cargo.lock")),
        "{check}"
    );
    Ok(())
}

#[test]
fn workflow_mention_continues_pending_merged_checks() -> Result<()> {
    let fixture = workflow_run("checks:\n  verify:\n    - 'test -f Cargo.lock'\n")?;
    let check = fixture.dispatch(&["check", &fixture.run_id, "--json"])?;
    ensure!(
        check.status.success(),
        "check failed: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    let check: Value = serde_json::from_slice(&check.stdout)?;
    let validity = &check["validity"];
    assert_eq!(validity["decision"], "continue", "{check}");
    assert!(
        validity["reasons"]
            .as_array()
            .is_some_and(|reasons| reasons.iter().any(|reason| {
                reason["code"] == "mention_changed"
                    && reason["path"] == "Cargo.lock"
                    && reason["detail"]
                        .as_str()
                        .is_some_and(|detail| detail.contains("merged checks"))
            })),
        "{check}"
    );
    let check_text = fixture.dispatch(&["check", &fixture.run_id])?;
    ensure!(check_text.status.success(), "text check failed");
    let check_text = String::from_utf8_lossy(&check_text.stdout);
    assert!(
        check_text.contains("mention_changed") && check_text.contains("merged checks"),
        "{check_text}"
    );
    let accept = fixture.dispatch(&["accept", &fixture.run_id])?;
    ensure!(
        accept.status.success(),
        "accept failed: {}",
        String::from_utf8_lossy(&accept.stderr)
    );
    let stdout = String::from_utf8_lossy(&accept.stdout);
    assert!(
        stdout.contains("Merged checks: passed on the current source with this patch"),
        "{stdout}"
    );
    assert_eq!(
        fs::read_to_string(fixture.source.join(".github/workflows/ci.yml"))?,
        WORKFLOW_DELTA
    );
    let metadata: Value = serde_json::from_slice(&fs::read(
        fixture
            .state
            .join("runs")
            .join(&fixture.run_id)
            .join("metadata.json"),
    )?)?;
    assert_eq!(metadata["coherence"]["validity"]["analysis"], "integration");
    let explain = fixture.dispatch(&["explain", &fixture.run_id])?;
    ensure!(explain.status.success(), "explain failed");
    let explain = String::from_utf8_lossy(&explain.stdout);
    assert!(
        explain.contains("mention_changed") && explain.contains("merged checks"),
        "{explain}"
    );
    Ok(())
}

#[test]
fn schema_file_used_by_code_still_requires_refresh() {
    let scenario = scenarios()
        .into_iter()
        .find(|scenario| scenario.name == "08 contract file (json schema)")
        .expect("scenario 08 exists");
    for git_source in [true, false] {
        let row = run_scenario(&scenario, git_source);
        assert_eq!(
            row.coherence,
            Some(Refresh),
            "{}: {:?}",
            row.source,
            row.error
        );
    }
}

#[test]
fn scenario_matrix_reports_and_enforces_gates() {
    let rows = run_matrix();
    report(&rows);

    let gated: Vec<&Row> = rows.iter().filter(|row| row.gated()).collect();
    let failures: Vec<String> = gated
        .iter()
        .filter(|row| !row.passes())
        .map(|row| {
            format!(
                "{} [{}]: coherence {} but L0 expects {}{}",
                row.name,
                row.source,
                short(row.coherence),
                short(Some(row.expected_l0)),
                row.error
                    .as_ref()
                    .map(|error| format!(" ({error})"))
                    .unwrap_or_default()
            )
        })
        .collect();
    assert!(
        failures.is_empty(),
        "rows disagree with expected_l0:\n{}",
        failures.join("\n")
    );

    let coherence = count(&gated, |row| row.coherence, |row| row.expected_l0);
    let overlap = count(&gated, |row| row.overlap, |row| row.expected_l0);
    // Every row but the known_gap ones is gated, so this covers all rows.
    assert_eq!(
        coherence.false_continue, 0,
        "coherence produced a false CONTINUE on a gated row"
    );
    // Row 14 (syntax error in the world) expects REFRESH by design, so it is
    // not a false refresh; every other row must not refresh needlessly.
    assert_eq!(
        coherence.false_refresh, 0,
        "coherence produced a false REFRESH on a gated row"
    );
    assert!(
        coherence.false_refresh < overlap.false_refresh,
        "coherence false-refresh ({}) must be strictly below file overlap ({})",
        coherence.false_refresh,
        overlap.false_refresh
    );
}

// ---------------------------------------------------------------------------
// Performance report (run with `cargo test --test coherence_matrix -- --ignored --nocapture`)

#[test]
#[ignore = "perf report; prints timings only"]
fn perf_report_on_a_synthetic_repo() {
    const FILES: usize = 300;
    const CHANGED: usize = 20;
    let body = |index: usize, version: u32| {
        format!(
            "pub fn item_{index}(x: i64) -> i64 {{\n    let base = {index};\n    x + base + {version}\n}}\n\n// padding\n// padding\n// padding\n// padding\n\npub fn twin_{index}(x: i64) -> i64 {{\n    item_{index}(x) * 2\n}}\n"
        )
    };
    let paths: Vec<String> = (0..FILES)
        .map(|index| format!("src/m{index:03}.rs"))
        .collect();
    let s0: Vec<(String, String)> = paths
        .iter()
        .enumerate()
        .map(|(index, path)| (path.clone(), body(index, 0)))
        .collect();
    let delta = [(paths[0].clone(), body(0, 1))];
    let world: Vec<(String, String)> = (100..100 + CHANGED)
        .map(|index| (paths[index].clone(), body(index, 2)))
        .collect();
    let s0_refs: Vec<(&str, &str)> = s0.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect();
    let delta_refs: Vec<(&str, &str)> = delta
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    let ops: Vec<Op> = world
        .iter()
        .map(|(path, contents)| Op::Write(path.as_str(), contents.as_str()))
        .collect();

    for git_source in [true, false] {
        let ctx = build(git_source, &s0_refs, &[], &delta_refs, &ops).unwrap();
        let verdict = verdict(&ctx).unwrap();
        println!(
            "perf {} source: {FILES} files, {CHANGED} changed in the world | snapshot {:.1} ms | observe {:.1} ms | evaluate {:.1} ms | decision {} | world changes {}",
            if git_source { "git" } else { "dir" },
            ctx.snapshot_ms,
            verdict.observe_ms,
            verdict.eval_ms,
            short(Some(verdict.decision)),
            verdict.changed.len(),
        );
        assert_eq!(verdict.decision, Continue);
        assert_eq!(verdict.changed.len(), CHANGED);
        assert!(matches!(
            (&ctx.snapshot.kind, git_source),
            (SourceKind::Git, true) | (SourceKind::Directory, false)
        ));
    }
}
