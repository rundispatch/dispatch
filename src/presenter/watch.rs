//! `dispatch watch` on a terminal: the project view, where the person can act
//! on a Work item without typing its ID. Every action is exactly the command
//! they would type, run as them under the same locks and authority; `watch`
//! still owns nothing and records nothing of its own.
//!
//! `build` is the view model: one `Item` per Work in view, from its run, its
//! `WorkLine` and the interaction projection, reading nothing else. `render`
//! draws it; `interactive` reads keys and acts on the selected Work by its ID.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout},
    widgets::{Cell, Row, Table},
};
use unicode_width::UnicodeWidthStr;

use super::*;
use crate::coherence::interactions::{
    Analysis, Change, Interaction, Projection, Rule, Side, Target,
};
use crate::orchestrator::{self, ReviewDecision, background, serve};

/// Wide layout (table and details) from this many terminal columns.
const WIDE: u16 = 90;
/// Below this, only a request to widen the terminal.
const NARROWEST: u16 = 40;
const NOTICE_LIFETIME: Duration = Duration::from_secs(10);
/// Labels in the details, padded to this many cells.
const LABEL: usize = 9;
const WIDE_DETAILS: usize = 8;

const ACCEPT: &str = "a accept";
const REJECT: &str = "r reject";
const REVIEW: &str = "d review";
const FINISH: &str = "f finish";

/// What a piece of text is colored with; the words always say the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Plain,
    Success,
    Warning,
    Error,
}

/// Work that needs a decision is listed first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Group {
    NeedsYou,
    InProgress,
    Done,
}

/// One labelled part of the details. Rank 0 is never dropped; under limited
/// height the highest rank is dropped first.
#[derive(Clone, Debug, PartialEq)]
struct Detail {
    label: &'static str,
    lines: Vec<String>,
    rank: u8,
}

impl Detail {
    fn new(label: &'static str, line: impl Into<String>, rank: u8) -> Self {
        Self {
            label,
            lines: vec![line.into()],
            rank,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Touches {
    /// Not known: nothing is claimed.
    Unknown,
    Names(Vec<String>),
}

/// One Work item in view.
#[derive(Clone, Debug, PartialEq)]
struct Item {
    id: String,
    /// Long enough to tell the Work in view apart.
    short: String,
    /// The name before duplicates are told apart.
    base: String,
    /// The name, told apart from every other item's at full width.
    name: String,
    agent: String,
    state: &'static str,
    verdict: &'static str,
    verdict_tone: Tone,
    checks: &'static str,
    touches: Touches,
    /// The details' first line after the name: the agent and how the Work
    /// came to be listed, then where it is.
    origin: String,
    place: Option<String>,
    details: Vec<Detail>,
    next: String,
    keys: Vec<&'static str>,
    group: Group,
}

/// What `build` reads besides the runs: nothing it loads itself.
struct Context<'a> {
    root: &'a Path,
    home: Option<&'a Path>,
    /// Whether an owner watches the project: without one, nothing writes
    /// the projection.
    watched: bool,
    projection: Option<&'a Projection>,
    /// Each run's configured checks, and whether accept runs them on the
    /// merged result (`coherence.integration_checks`).
    checks: &'a HashMap<String, (Vec<String>, bool)>,
}

/// The view model: one item per run in view, Work that needs you first, then
/// by when it began.
fn build(runs: &[RunRecord], context: &Context) -> Vec<Item> {
    let ids: Vec<&str> = runs.iter().map(|run| run.id.as_str()).collect();
    let short = crate::state::short_ids(&ids);
    let bases: Vec<(String, &str)> = runs
        .iter()
        .map(|run| (base_name(run, short[run.id.as_str()]), run.id.as_str()))
        .collect();
    let names = tell_apart(&bases, usize::MAX, false);
    let named: HashMap<&str, &str> = ids
        .iter()
        .copied()
        .zip(names.iter().map(String::as_str))
        .collect();
    let mut items: Vec<(Group, chrono::DateTime<chrono::Utc>, Item)> = runs
        .iter()
        .zip(bases)
        .map(|(run, (base, _))| {
            let item = item(run, short[run.id.as_str()], base, &named, context);
            (item.group, run.created_at, item)
        })
        .collect();
    items.sort_by(|a, b| (a.0, a.1, &a.2.id).cmp(&(b.0, b.1, &b.2.id)));
    items.into_iter().map(|(_, _, item)| item).collect()
}

/// The workspace folder's name, unless Dispatch made (and named) it; else
/// the task, unless it is the generic one; else the short ID.
fn base_name(run: &RunRecord, short: &str) -> String {
    if let Some(folder) = run
        .attachment
        .as_ref()
        .filter(|attachment| attachment.workspace_owner != crate::WorkspaceOwner::Dispatch)
        .and_then(|attachment| attachment.workspace.file_name())
    {
        return folder.to_string_lossy().into_owned();
    }
    let task = one_line(run.task.lines().next().unwrap_or_default());
    if task.is_empty() || task.starts_with("attached work in ") {
        short.to_owned()
    } else {
        task
    }
}

fn item(
    run: &RunRecord,
    short: &str,
    base: String,
    named: &HashMap<&str, &str>,
    context: &Context,
) -> Item {
    let validity = run.coherence.as_ref().and_then(|c| c.validity.as_ref());
    let line = orchestrator::work_line(run, validity);
    let name = named[run.id.as_str()].to_owned();
    let state = line.state;
    let group = match state {
        "working" | "idle" => Group::InProgress,
        "applied" | "rejected" | "finished" => Group::Done,
        _ => Group::NeedsYou,
    };
    let verdict = match line.verdict {
        _ if group == Group::Done => "–",
        "CONTINUE" | "unmoved" => "CONTINUE",
        "REFRESH" => "REFRESH",
        "STOP" => "STOP",
        "overridden" => "–",
        _ => "not checked",
    };
    let checks = match line.verification {
        "checks passed" => "passed",
        "checks failed" => "failed",
        "checks not run" => "not run",
        "no checks" => "none",
        _ => "inconclusive",
    };
    let verdict_tone = match verdict {
        "REFRESH" | "STOP" => Tone::Warning,
        "CONTINUE" if state == "ready" && checks != "failed" => Tone::Success,
        _ => Tone::Plain,
    };
    let reviewable =
        run.outcome.review == ReviewState::Pending && run.outcome.work_result == WorkResult::Ready;

    let mut details = Vec::new();
    match state {
        "applied" => details.push(Detail::new("Landed", landed(run, &line), 0)),
        "rejected" => details.push(Detail::new("Outcome", "Rejected · nothing was applied", 0)),
        "finished" => details.push(Detail::new("Outcome", "Ended without a result", 0)),
        _ => {
            let mut lines = match line.verdict {
                "unmoved" => vec!["CONTINUE · the source has not moved since it began".to_owned()],
                "CONTINUE" => vec![
                    "CONTINUE · the source moved; nothing this Work relies on changed".to_owned(),
                ],
                "REFRESH" => {
                    let reasons = validity.map(|v| v.reasons.as_slice()).unwrap_or_default();
                    let mut lines: Vec<String> = reasons
                        .iter()
                        .take(3)
                        .map(|reason| one_line(&reason.detail))
                        .collect();
                    match lines.first_mut() {
                        Some(first) => *first = format!("REFRESH · {first}"),
                        None => lines.push("REFRESH".into()),
                    }
                    if reasons.len() > 3 {
                        lines.push(format!(
                            "+{} more: dispatch check {short}",
                            reasons.len() - 3
                        ));
                    }
                    lines
                }
                "STOP" => vec!["STOP · its changes are already in the source".to_owned()],
                _ => vec!["not checked yet".to_owned()],
            };
            if matches!(state, "working" | "idle") && matches!(verdict, "REFRESH" | "STOP") {
                lines.push("Advisory while the agent works: nothing is stopped.".into());
            }
            let extra = lines.split_off(1);
            details.push(Detail {
                label: "Verdict",
                lines,
                rank: 0,
            });
            if !extra.is_empty() {
                details.push(Detail {
                    label: "",
                    lines: extra,
                    rank: 1,
                });
            }
        }
    }
    details.push(Detail::new(
        "Checks",
        match checks {
            "passed" => "passed",
            "failed" if reviewable => "failed · d review the output",
            "failed" => "failed",
            "not run" => "not run yet",
            "none" => "none configured",
            _ => "inconclusive",
        },
        2,
    ));
    // Done Work takes no part in interactions, watched or not.
    let (touches, lines) = if group == Group::Done {
        (Touches::Names(Vec::new()), Vec::new())
    } else {
        touches(run, &name, named, context)
    };
    if !lines.is_empty() {
        details.push(Detail {
            label: "Touches",
            lines,
            rank: 3,
        });
    }
    details.push(Detail::new("Began", began(run), 4));

    let agent = line.agent.clone();
    let origin = match line.origin {
        "native" => format!("{agent} · launched by Dispatch"),
        "isolated" => format!("{agent} · attached; Dispatch made its workspace"),
        "discovered" => format!("{agent} session in its own worktree"),
        _ => format!("{agent} · attached workspace"),
    };
    let place = run
        .attachment
        .as_ref()
        .map(|attachment| shown_path(&attachment.workspace, context.root, context.home));
    let (next, keys) = next(run, &line, short, verdict, checks, context);
    Item {
        id: run.id.clone(),
        short: short.to_owned(),
        base,
        name,
        agent,
        state,
        verdict,
        verdict_tone,
        checks,
        touches,
        origin,
        place,
        details,
        next,
        keys,
        group,
    }
}

/// How applied Work landed, in place of its verdict.
fn landed(run: &RunRecord, line: &orchestrator::WorkLine) -> String {
    let by = if line.applied_by == Some("auto_apply") {
        "applied by auto-apply"
    } else {
        "applied by you"
    };
    let how = match line.verdict {
        "unmoved" => " · it was CONTINUE (the source had not moved)".to_owned(),
        "CONTINUE" => " · it was CONTINUE".to_owned(),
        "overridden" => format!(
            " · applied over REFRESH by your override: {}",
            run.coherence
                .as_ref()
                .and_then(|c| c.overridden.as_ref())
                .and_then(|v| v.reasons.first())
                .map(|reason| one_line(&reason.detail))
                .unwrap_or_default()
        ),
        _ => String::new(),
    };
    format!("{by}{how}")
}

/// What the Work began against, from `WorkLine`'s inputs.
fn began(run: &RunRecord) -> String {
    let short = |commit: &str| commit.chars().take(8).collect::<String>();
    match run.attachment.as_ref().map(|a| &a.provenance) {
        Some(crate::BaselineProvenance::GitMergeBase { commit }) => format!(
            "the Git merge base {} of its worktree and the project",
            short(commit)
        ),
        Some(crate::BaselineProvenance::SnapshotAtAttach) => {
            "a Dispatch snapshot of its folder taken at attach; earlier edits are not in its changes"
                .into()
        }
        Some(crate::BaselineProvenance::WorkspaceAtStart { commit }) => format!(
            "a Dispatch snapshot ({}) of its worktree when its first session started, not a commit on any branch",
            short(commit)
        ),
        None => match &run.source_git_head {
            Some(head) => format!(
                "a Dispatch snapshot of the project's working tree at {}",
                short(head)
            ),
            None => "a Dispatch snapshot of the project folder".into(),
        },
    }
}

/// The next action in words, and the keys offered for it.
fn next(
    run: &RunRecord,
    line: &orchestrator::WorkLine,
    short: &str,
    verdict: &str,
    checks: &str,
    context: &Context,
) -> (String, Vec<&'static str>) {
    let root = folder_name(context.root);
    let (commands, integration) = context
        .checks
        .get(&run.id)
        .map(|(commands, integration)| (commands.as_slice(), *integration))
        .unwrap_or_default();
    let attached = run.mode == RunMode::Attached;
    match line.state {
        "question" => (
            format!("It is waiting for an answer: dispatch status {short} shows the question."),
            vec![],
        ),
        "working" if attached && commands.is_empty() => (
            "When the agent is done, f finish: freezes its changes.".into(),
            vec![FINISH],
        ),
        "working" if attached => (
            format!(
                "When the agent is done, f finish: freezes its changes and runs {}.",
                commands.join(", ")
            ),
            vec![FINISH],
        ),
        "working" => (
            "Dispatch is running it. It is listed as ready when it is done.".into(),
            vec![],
        ),
        "idle" => (
            "No session is open. f finish to freeze its changes, or resume the session in its worktree."
                .into(),
            vec![FINISH],
        ),
        "removed" => (
            "Its worktree was removed and its exact changes were kept. f finish to check them."
                .into(),
            vec![FINISH],
        ),
        "lost" => (
            "Its worktree is gone; only its last-seen changes were kept. f finish to freeze them, then review."
                .into(),
            vec![FINISH],
        ),
        "applied" => (format!("Nothing to do: it is in {root}."), vec![]),
        "rejected" | "finished" => ("Nothing to do.".into(), vec![]),
        // Ready, or blocked: what accepting it would do.
        _ if verdict == "REFRESH" => {
            let mut text = "It can't be accepted as is. r reject it, then run the agent again from the current source.".to_owned();
            if !attached {
                text.push_str(&format!(" Or: dispatch refresh {short}."));
            }
            (text, vec![REJECT, REVIEW])
        }
        _ if verdict == "STOP" => (
            "Its changes are already in the source. r reject it.".into(),
            vec![REJECT],
        ),
        _ if checks == "failed" => (
            "Its checks failed. d review the output, then a accept or r reject.".into(),
            vec![REVIEW, ACCEPT, REJECT],
        ),
        _ if line.verdict == "CONTINUE" && integration && !commands.is_empty() => (
            format!(
                "a accept: runs your checks on the merged result, then applies it to {root}."
            ),
            vec![ACCEPT, REVIEW, REJECT],
        ),
        _ if verdict == "CONTINUE" => (
            format!("a accept: applies it to {root}."),
            vec![ACCEPT, REVIEW, REJECT],
        ),
        _ => (
            "a accept: Dispatch checks it against the source first.".into(),
            vec![ACCEPT, REVIEW, REJECT],
        ),
    }
}

/// Whom this Work touches, for its column, and how, for its details. Both
/// pieces of Work are named in every line.
fn touches(
    run: &RunRecord,
    this: &str,
    named: &HashMap<&str, &str>,
    context: &Context,
) -> (Touches, Vec<String>) {
    let Some(projection) = context.projection else {
        let why = if context.watched {
            "not known yet"
        } else {
            "not known: the project is not watched"
        };
        return (Touches::Unknown, vec![why.into()]);
    };
    // Work that takes no part, such as lost Work, touches nothing.
    let Some(participant) = projection.participants.iter().find(|p| p.run_id == run.id) else {
        return (Touches::Names(Vec::new()), Vec::new());
    };
    if participant.analysis == Analysis::Pending {
        return (
            Touches::Unknown,
            vec!["not known yet: its files don't parse while it's being edited".into()],
        );
    }
    let name = |id: &str| {
        named
            .get(id)
            .map_or_else(|| id.chars().take(8).collect(), |name| (*name).to_owned())
    };
    let found = projection.of(&run.id);
    if found.is_empty() {
        return (Touches::Names(Vec::new()), Vec::new());
    }
    let mut others: Vec<String> = found.iter().map(|(other, _)| name(other)).collect();
    others.sort();
    others.dedup();
    let mut lines: Vec<String> = found
        .iter()
        .map(|(other, interaction)| touch(interaction, this, &name(other)))
        .collect();
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
    lines.push("Advisory: nothing is held back.".into());
    (Touches::Names(others), lines)
}

/// One interaction in words, seen from `this` (`Side::A`), with the verbs of
/// `interactions::explain`.
fn touch(interaction: &Interaction, this: &str, other: &str) -> String {
    let what = match &interaction.target {
        Target::Symbol { path, name } => format!("{name} ({path})"),
        Target::File { path } => path.clone(),
    };
    let verb = match interaction.change {
        Some(Change::Signature) => "changes the signature of",
        Some(Change::Removes) => "removes",
        Some(Change::Adds) => "adds",
        Some(Change::Deletes) => "deletes",
        Some(Change::Changes) | None => "changes",
    };
    let both = match interaction.change {
        Some(Change::Adds) => "both add",
        Some(Change::Deletes) => "both delete",
        _ => "both change",
    };
    let (writer, reader) = match interaction.writer {
        Some(Side::B) => (other, this),
        _ => (this, other),
    };
    match (interaction.rule, interaction.writer) {
        (Rule::SameDeclaration, _) => format!("{this} and {other} {both} {what}"),
        (Rule::Uses, _) => format!("{writer} {verb} {what}, which {reader} uses"),
        (Rule::File, None) => format!("{this} and {other} {both} {what} (whole file)"),
        (Rule::File, Some(_)) => {
            format!("{writer} {verb} {what}, which {reader} relies on (whole file)")
        }
        (Rule::TextualOverlap, _) => match interaction.lines {
            Some((start, end)) => format!(
                "The edits of {this} and {other} overlap as text at lines {start}–{end} of {what}"
            ),
            None => format!("The edits of {this} and {other} overlap as text in {what}"),
        },
    }
}

/// A path relative to the project root when inside it, else with `$HOME`
/// as `~`.
fn shown_path(path: &Path, root: &Path, home: Option<&Path>) -> String {
    if let Ok(inside) = path.strip_prefix(root)
        && !inside.as_os_str().is_empty()
    {
        return inside.display().to_string();
    }
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(inside) => format!("~/{}", inside.display()),
        None => path.display().to_string(),
    }
}

fn folder_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Names cut to `width` cells. Names left equal by the cut each get a short
/// ID, at least 4 characters and long enough to tell that group apart; the
/// name part is cut to make room, never the ID.
fn tell_apart(items: &[(String, &str)], width: usize, ascii: bool) -> Vec<String> {
    let cut: Vec<String> = items
        .iter()
        .map(|(name, _)| fit(name, width, ascii))
        .collect();
    items
        .iter()
        .enumerate()
        .map(|(i, (name, id))| {
            let shared = (0..items.len())
                .filter(|&j| j != i && cut[j] == cut[i])
                .map(|j| {
                    id.bytes()
                        .zip(items[j].1.bytes())
                        .take_while(|(a, b)| a == b)
                        .count()
                })
                .max();
            match shared {
                None => cut[i].clone(),
                Some(shared) => {
                    let tag = id.get(..(shared + 1).max(4).min(id.len())).unwrap_or(id);
                    format!(
                        "{} {tag}",
                        fit(name, width.saturating_sub(tag.len() + 1), ascii)
                    )
                }
            }
        })
        .collect()
}

/// Who watches the project, for the header.
#[derive(Clone, Debug, Default, PartialEq)]
struct Header {
    root: String,
    status: String,
    /// The status in a word or two, for narrow terminals.
    short: &'static str,
    unwatched: bool,
    /// The check consent, as `project_line` says it.
    extra: String,
}

impl Header {
    fn load(state: &State, root: &Path) -> Self {
        let watcher = background::watcher(state, root);
        let (short, unwatched) = match &watcher {
            Ok(background::Watcher::Watched(Some(record))) if !record.background => {
                ("watched by serve", false)
            }
            Ok(background::Watcher::Watched(_)) => ("watched", false),
            Ok(background::Watcher::NotWatched) => ("not watched", true),
            Err(_) => ("watching unknown", false),
        };
        let status = if unwatched {
            "not watched · start watching: dispatch start".into()
        } else {
            background::watcher_line(&watcher, false)
        };
        let mut extra = String::new();
        match crate::consent::project_root(root)
            .and_then(|project| crate::consent::consent(state, &project))
            .map(|consent| consent.describe())
        {
            Ok(Some(consent)) => extra.push_str(&format!(" · {consent}")),
            Ok(None) => {}
            Err(error) => extra.push_str(&format!(" · check consent unknown: {error:#}")),
        }
        Self {
            root: folder_name(root),
            status,
            short,
            unwatched,
            extra,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Done,
    Refused,
    Info,
}

/// The one notice shown: what happened to which Work.
#[derive(Clone, Debug)]
struct Notice {
    id: Option<String>,
    name: Option<String>,
    kind: Kind,
    text: String,
    at: Instant,
}

impl Notice {
    fn new(work: Option<&Item>, kind: Kind, text: impl Into<String>) -> Self {
        Self {
            id: work.map(|item| item.id.clone()),
            name: work.map(|item| item.name.clone()),
            kind,
            text: text.into(),
            at: Instant::now(),
        }
    }
    /// `<name>: <text>`, with the Work's name as it is listed now.
    fn shown(&self, items: &[Item]) -> String {
        let now = self
            .id
            .as_ref()
            .and_then(|id| items.iter().find(|item| &item.id == id))
            .map(|item| &item.name);
        match now.or(self.name.as_ref()) {
            Some(name) => format!("{name}: {}", self.text),
            None => self.text.clone(),
        }
    }
}

/// The selected Work, by ID. `moved` is set when the selected Work left the
/// list and the selection went to another, until the person moves or acts.
#[derive(Debug, Default)]
struct Selection {
    id: Option<String>,
    moved: bool,
}

impl Selection {
    /// Follow the selection from the list `before` to `after`; the notice
    /// when the selected Work left it.
    fn follow(&mut self, before: &[Item], after: &[Item]) -> Option<Notice> {
        let Some(last) = after.len().checked_sub(1) else {
            self.id = None;
            self.moved = false;
            return None;
        };
        let Some(id) = &self.id else {
            self.id = Some(after[0].id.clone());
            return None;
        };
        if after.iter().any(|item| &item.id == id) {
            return None;
        }
        let position = before.iter().position(|item| &item.id == id);
        let now = &after[position.unwrap_or(0).min(last)];
        let left = position.map_or_else(|| id.clone(), |i| before[i].name.clone());
        self.id = Some(now.id.clone());
        self.moved = true;
        Some(Notice::new(
            None,
            Kind::Info,
            format!("{left} left the list; now on {}.", now.name),
        ))
    }

    fn step(&mut self, items: &[Item], down: bool) {
        self.moved = false;
        let Some(i) = items
            .iter()
            .position(|item| Some(&item.id) == self.id.as_ref())
        else {
            self.id = items.first().map(|item| item.id.clone());
            return;
        };
        let i = if down {
            (i + 1).min(items.len() - 1)
        } else {
            i.saturating_sub(1)
        };
        self.id = Some(items[i].id.clone());
    }
}

/// Everything one frame shows.
#[derive(Debug)]
struct Screen<'a> {
    header: &'a Header,
    items: &'a [Item],
    selected: Option<&'a str>,
    notice: Option<(Kind, String)>,
    ascii: bool,
}

impl Screen<'_> {
    fn key(&self) -> String {
        format!("{self:?}")
    }
}

pub async fn interactive(state: &State, root: PathBuf, options: Options) -> Result<()> {
    let mut ui = Ui::new(options)?;
    ui.auto_apply_mode = false;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut header = Header::default();
    let mut items: Vec<Item> = Vec::new();
    let mut selection = Selection::default();
    let mut notice: Option<Notice> = None;
    let mut journal: Option<i64> = None;
    let mut loaded: Option<Instant> = None;
    loop {
        // The event journal is the doorbell; the clock ages finished Work out.
        let now = crate::db::Database::open_read_only(state.db_path())
            .and_then(|db| db.latest_event_id())
            .ok()
            .flatten();
        if loaded.is_none_or(|at| at.elapsed() >= Duration::from_secs(2)) || now != journal {
            header = Header::load(state, &root);
            let (runs, projection) = serve::watch_view(state, &root)?;
            let checks = runs
                .iter()
                .map(|run| (run.id.clone(), configured_checks(state, &run.id)))
                .collect();
            let fresh = build(
                &runs,
                &Context {
                    root: &root,
                    home: home.as_deref(),
                    watched: !header.unwatched,
                    projection: projection.as_ref(),
                    checks: &checks,
                },
            );
            if let Some(left) = selection.follow(&items, &fresh) {
                notice = Some(left);
            }
            items = fresh;
            journal = now;
            loaded = Some(Instant::now());
        }
        if notice
            .as_ref()
            .is_some_and(|notice| notice.at.elapsed() >= NOTICE_LIFETIME)
        {
            notice = None;
        }
        let screen = Screen {
            header: &header,
            items: &items,
            selected: selection.id.as_deref(),
            notice: notice
                .as_ref()
                .map(|notice| (notice.kind, notice.shown(&items))),
            ascii: options.ascii,
        };
        ui.draw_frame(&screen.key(), |frame, palette| {
            render(frame, &screen, palette)
        })?;
        let Some(event) = ui.next().await? else {
            return Ok(());
        };
        let Event::Key(key) = event else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        let action = match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                selection.step(&items, false);
                continue;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                selection.step(&items, true);
                continue;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char(c @ ('a' | 'r' | 'f' | 'd')) => c.to_string(),
            KeyCode::Enter => "Enter".to_owned(),
            // Shift+Tab included: watch has no auto-apply mode.
            _ => continue,
        };
        let item = match target(&mut selection, &items, &action) {
            Ok(Some(item)) => item.clone(),
            Ok(None) => continue,
            Err(moved) => {
                notice = Some(moved);
                continue;
            }
        };
        let run = match listed(serve::watch_view(state, &root)?.0, &item.id) {
            Ok(run) => run,
            Err(gone) => {
                notice = Some(gone);
                continue;
            }
        };
        notice = match action.as_str() {
            "a" => Some(accept(state, &root, &item)),
            "r" => Some(reject(&mut ui, state, &root, &item).await?),
            "f" => Some(finish(&mut ui, state, &run, &item).await?),
            _ if run.outcome.review == ReviewState::Pending
                && run.outcome.work_result == WorkResult::Ready =>
            {
                review_goal(&mut ui, state, run, options, "").await?;
                None
            }
            _ => Some(Notice::new(
                Some(&item),
                Kind::Info,
                "only a result waiting for review can be reviewed.",
            )),
        };
        loaded = None;
    }
}

/// The Work an action key acts on: the selection as last drawn. Nothing
/// right after the selection moved by itself; that press only clears it.
fn target<'a>(
    selection: &mut Selection,
    items: &'a [Item],
    key: &str,
) -> std::result::Result<Option<&'a Item>, Notice> {
    let Some(item) = selection
        .id
        .as_ref()
        .and_then(|id| items.iter().find(|item| &item.id == id))
    else {
        return Ok(None);
    };
    if selection.moved {
        selection.moved = false;
        return Err(Notice::new(
            None,
            Kind::Info,
            format!(
                "Selection moved to {}. Press {key} again to act on it.",
                item.name
            ),
        ));
    }
    Ok(Some(item))
}

/// The run reloaded by ID, while it is still in view.
fn listed(runs: Vec<RunRecord>, id: &str) -> std::result::Result<RunRecord, Notice> {
    runs.into_iter().find(|run| run.id == id).ok_or_else(|| {
        Notice::new(
            None,
            Kind::Info,
            "That Work is no longer listed; nothing changed.",
        )
    })
}

/// The project's checks as this run took them, and whether accept runs them
/// on the merged result.
fn configured_checks(state: &State, id: &str) -> (Vec<String>, bool) {
    crate::coherence::run_config(&state.run_dir(id))
        .map(|config| (config.checks.verify, config.coherence.integration_checks))
        .unwrap_or_default()
}

/// An error's first sentence, without its full stop.
fn first_sentence(error: &anyhow::Error) -> String {
    let text = one_line(&format!("{error:#}"));
    let first = text
        .split_once(". ")
        .map_or(text.as_str(), |(first, _)| first);
    first.trim_end_matches('.').to_owned()
}

/// `a`: `dispatch accept`.
fn accept(state: &State, root: &Path, item: &Item) -> Notice {
    let result = orchestrator::decide(
        state,
        Some(&item.id),
        root,
        ReviewDecision::Accept {
            despite_refresh: false,
        },
        Vec::new(),
        None,
        true,
    );
    match result {
        Ok(_) => Notice::new(Some(item), Kind::Done, "accepted and applied."),
        Err(error) => Notice::new(
            Some(item),
            Kind::Refused,
            refusal(state.load_run(&item.id).ok().as_ref(), &error),
        ),
    }
}

/// Why accept was refused. When the coherence gate blocked it, the run,
/// reloaded after the refusal, says why in its stored validity; anything
/// else is the error's first sentence. Never parsed from the error's words.
fn refusal(run: Option<&RunRecord>, error: &anyhow::Error) -> String {
    let blocked = run
        .filter(|run| run.outcome.application == ApplicationState::BlockedBySourceDrift)
        .and_then(|run| run.coherence.as_ref()?.validity.as_ref());
    // The reason is on the Verdict line; the notice says what to do.
    match blocked.map(|validity| validity.decision) {
        Some(Decision::Refresh) => {
            "not applied: stale (REFRESH). The source is unchanged. Next: r reject it.".into()
        }
        Some(Decision::Stop) => {
            "not applied: STOP, its changes are already in the source. Next: r reject it.".into()
        }
        _ => format!("not applied: {}.", first_sentence(error)),
    }
}

/// `r`: `dispatch reject`, once the person says so; Enter alone cancels.
async fn reject(ui: &mut Ui, state: &State, root: &Path, item: &Item) -> Result<Notice> {
    let question = format!(
        "Reject {} (Work {})? Nothing is applied, and a workspace Dispatch made is kept.",
        item.name, item.short
    );
    let rows = [Choice::new("Reject"), Choice::new("Cancel")];
    if ui.select(&question, &rows, 1).await? != Some(0) {
        return Ok(Notice::new(Some(item), Kind::Info, "nothing changed."));
    }
    Ok(
        match orchestrator::decide(
            state,
            Some(&item.id),
            root,
            ReviewDecision::Reject,
            Vec::new(),
            None,
            true,
        ) {
            Ok(_) => Notice::new(Some(item), Kind::Done, "rejected; nothing was applied."),
            Err(error) => Notice::new(
                Some(item),
                Kind::Refused,
                format!("not rejected: {}.", first_sentence(&error)),
            ),
        },
    )
}

/// `f`: `dispatch finish` for active attached Work. Running the project's
/// checks needs the person's word, unless the Work already carries it.
async fn finish(ui: &mut Ui, state: &State, run: &RunRecord, item: &Item) -> Result<Notice> {
    if run.mode != crate::RunMode::Attached || run.outcome.lifecycle == LifecycleState::Finished {
        return Ok(Notice::new(
            Some(item),
            Kind::Info,
            "only attached Work still in progress can be finished.",
        ));
    }
    let checks = configured_checks(state, &run.id).0;
    let mut allow = false;
    if !checks.is_empty() && !run.environment.unsafe_local {
        let question = format!(
            "Finish {} (Work {})? Finishing runs this project's checks on your machine, with your permissions:\n{}",
            item.name,
            item.short,
            checks
                .iter()
                .map(|command| format!("  {command}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let rows = [
            Choice::new("Run the checks and finish"),
            Choice::new("Cancel"),
        ];
        if ui.select(&question, &rows, 1).await? != Some(0) {
            return Ok(Notice::new(Some(item), Kind::Info, "nothing changed."));
        }
        allow = true;
    }
    Ok(
        match crate::orchestrator::attach::finish_quietly(state, &run.id, allow).await {
            Ok(run) => Notice::new(
                Some(item),
                Kind::Done,
                match run.outcome.verification {
                    crate::VerificationState::Passed => {
                        "finished; checks passed. It now waits for your review."
                    }
                    crate::VerificationState::Failed => "finished; checks failed.",
                    _ => "finished. It now waits for your review.",
                },
            ),
            Err(error) => Notice::new(
                Some(item),
                Kind::Refused,
                format!("not finished: {}.", first_sentence(&error)),
            ),
        },
    )
}

// Rendering. Everything below reads only the `Screen`.

/// Text as drawn: terminal controls removed, on one line, and in `--ascii`
/// nothing outside ASCII.
fn fold(text: &str, ascii: bool) -> String {
    let text = sanitize(text).replace(['\n', '\t'], " ");
    if !ascii {
        return text;
    }
    let mut folded = String::new();
    for c in text.replace("↑↓", "up/down").chars() {
        match c {
            '›' => folded.push('>'),
            '–' | '—' | '─' | '·' => folded.push('-'),
            '…' => folded.push_str("..."),
            '→' => folded.push_str("->"),
            c if c.is_ascii() => folded.push(c),
            _ => folded.push('?'),
        }
    }
    folded
}

/// `text` cut to `width` cells, ending in a single ellipsis when cut.
fn fit(text: &str, width: usize, ascii: bool) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let ellipsis = if ascii { "..." } else { "…" };
    let room = width.checked_sub(ellipsis.width()).unwrap_or(width);
    let mut cut = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let cells = grapheme.width();
        if used + cells > room {
            break;
        }
        used += cells;
        cut.push_str(grapheme);
    }
    if width >= ellipsis.width() {
        cut.push_str(ellipsis);
    }
    cut
}

/// Words wrapped to `width` cells; a word longer than a line is split.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_owned();
        while !word.is_empty() {
            let gap = usize::from(!line.is_empty());
            if line.width() + gap + word.width() <= width {
                if gap == 1 {
                    line.push(' ');
                }
                line.push_str(&word);
                break;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                continue;
            }
            let mut used = 0;
            let split = word
                .grapheme_indices(true)
                .find(|(_, grapheme)| {
                    used += grapheme.width();
                    used > width
                })
                .map_or(word.len(), |(at, _)| at.max(1));
            lines.push(word[..split].to_owned());
            word = word[split..].to_owned();
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

fn pad(text: &str, width: usize) -> String {
    format!("{text}{}", " ".repeat(width.saturating_sub(text.width())))
}

fn style(tone: Tone, palette: &Theme) -> Style {
    match tone {
        Tone::Plain => palette.foreground,
        Tone::Success => palette.success,
        Tone::Warning => palette.warning,
        Tone::Error => palette.error,
    }
}

/// A line whose first word carries `tone`.
fn led(text: String, tone: Tone, palette: &Theme) -> Vec<Span<'static>> {
    if tone == Tone::Plain {
        return vec![Span::styled(text, palette.foreground)];
    }
    let (word, rest) = text.split_at(text.find(' ').unwrap_or(text.len()));
    vec![
        Span::styled(word.to_owned(), style(tone, palette)),
        Span::styled(rest.to_owned(), palette.foreground),
    ]
}

fn render(frame: &mut Frame, screen: &Screen, palette: &Theme) {
    let full = frame.area();
    let area = if full.width > 4 {
        Rect::new(full.x + 1, full.y, full.width - 2, full.height)
    } else {
        full
    };
    let ascii = screen.ascii;
    let width = usize::from(area.width);
    if full.width < NARROWEST {
        let lines = vec![
            Line::styled(
                fold("Widen the terminal to at least 40 columns.", ascii),
                palette.foreground,
            ),
            Line::styled("q leave", palette.secondary),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        return;
    }
    let wide = full.width >= WIDE;
    let rule = Line::styled(fold(&"─".repeat(width), ascii), palette.secondary);
    let selected = screen
        .items
        .iter()
        .position(|item| Some(item.id.as_str()) == screen.selected);
    let item = selected.map(|i| &screen.items[i]);

    let notice: Vec<Line> = screen
        .notice
        .as_ref()
        .map(|(kind, text)| {
            let mut lines = wrap(&fold(text, ascii), width);
            if lines.len() > 2 {
                let rest = lines[1..].join(" ");
                lines.truncate(1);
                lines.push(fit(&rest, width, ascii));
            }
            let tone = if *kind == Kind::Refused {
                palette.error
            } else {
                palette.foreground
            };
            lines
                .into_iter()
                .map(|line| Line::styled(line, tone))
                .collect()
        })
        .unwrap_or_default();
    // Too long, the offered keys go from the end, then `↑↓`; `q leave` stays.
    let mut keys: Vec<&str> = item.map(|item| item.keys.clone()).unwrap_or_default();
    let mut tail = vec![if wide { "↑↓ select" } else { "↑↓" }, "q leave"];
    let hint = |keys: &[&str], tail: &[&str]| fold(&[keys, tail].concat().join(" · "), ascii);
    while hint(&keys, &tail).width() > width && tail.len() > 1 {
        if keys.pop().is_none() {
            tail.remove(0);
        }
    }
    let hint = Line::styled(fit(&hint(&keys, &tail), width, ascii), palette.secondary);

    // The list: a table on wide terminals, two-line cards on narrow ones.
    let empty: Vec<String> = if screen.items.is_empty() {
        let second = if screen.header.unwatched {
            "Nothing is watching this project: dispatch start."
        } else {
            "A Claude Code session in its own worktree of this project, or dispatch attach -- <agent>, appears here."
        };
        ["No Work in the last hour.", second]
            .iter()
            .flat_map(|line| wrap(&fold(line, ascii), width))
            .collect()
    } else {
        Vec::new()
    };
    let list_full = if !empty.is_empty() {
        empty.len()
    } else if wide {
        1 + screen.items.len()
    } else {
        2 * screen.items.len()
    };
    let groups = item.map(|item| details(item, width, ascii, palette));
    let line_one = usize::from(wide && item.is_some());
    let detail_lines = |needed: bool| {
        groups.as_ref().map_or(0, |groups| {
            groups
                .iter()
                .filter(|(rank, _)| (*rank == 0) == needed)
                .map(|(_, lines)| lines.len())
                .sum::<usize>()
        })
    };
    let mut detail_full = line_one + detail_lines(true) + detail_lines(false);
    if wide {
        detail_full = detail_full.min(WIDE_DETAILS);
    }
    let detail_needed = (line_one + detail_lines(true)).min(detail_full);

    // The height goes, in this order, to the column titles and the selected
    // Work's row (or its card), its details' first line, Verdict and Next,
    // the other rows, then the rest of the details. The header, notice and
    // hint always show.
    let mut room = usize::from(area.height).saturating_sub(2 + notice.len() + 1);
    // Lines in steps of `step`: cards are two lines each.
    let mut take = |want: usize, step: usize| {
        let got = want.min(room) / step * step;
        room -= got;
        got
    };
    let selected_row = take(if empty.is_empty() { 2 } else { empty.len() }, 1);
    let middle_rule = take(usize::from(item.is_some()), 1);
    let needed = take(detail_needed, 1);
    let card = if wide || !empty.is_empty() { 1 } else { 2 };
    let other_rows = take(list_full.saturating_sub(selected_row), card);
    let closing_rule = take(usize::from(wide || item.is_none()), 1);
    let detail_height = needed + take(detail_full - detail_needed, 1);
    let list_height = selected_row + other_rows;

    let mut heights = vec![1, 1, list_height];
    if item.is_some() {
        heights.extend([middle_rule, detail_height]);
    }
    if wide || item.is_none() {
        heights.push(closing_rule);
    }
    heights.extend([notice.len(), 1]);
    let parts = Layout::vertical(
        heights
            .iter()
            .map(|height| Constraint::Length(*height as u16)),
    )
    .flex(Flex::Start)
    .split(area);
    let mut part = parts.iter().copied();
    let mut take = || part.next().unwrap_or_default();

    frame.render_widget(Paragraph::new(header(screen, wide, width, palette)), take());
    frame.render_widget(Paragraph::new(rule.clone()), take());
    let list = take();
    if !empty.is_empty() {
        frame.render_widget(
            Paragraph::new(
                empty
                    .into_iter()
                    .map(|line| Line::styled(line, palette.foreground))
                    .collect::<Vec<_>>(),
            ),
            list,
        );
    } else if wide {
        frame.render_widget(table(screen, selected, list, palette), list);
    } else {
        frame.render_widget(Paragraph::new(cards(screen, selected, list, palette)), list);
    }
    if let (Some(item), Some(groups)) = (item, groups) {
        frame.render_widget(Paragraph::new(rule.clone()), take());
        let mut lines = Vec::new();
        if wide {
            lines.push(first_detail(item, width, ascii, palette));
        }
        lines.extend(fitted(groups, detail_height.saturating_sub(lines.len())));
        frame.render_widget(Paragraph::new(lines), take());
    }
    if wide || item.is_none() {
        frame.render_widget(Paragraph::new(rule), take());
    }
    frame.render_widget(Paragraph::new(notice), take());
    frame.render_widget(Paragraph::new(hint), take());
}

/// The project, who watches it and, when there is room, how many items need
/// you, are in progress and are done.
fn header(screen: &Screen, wide: bool, width: usize, palette: &Theme) -> Line<'static> {
    let ascii = screen.ascii;
    let header = screen.header;
    let root = fold(&header.root, ascii);
    let status = if wide {
        fold(&format!("{}{}", header.status, header.extra), ascii)
    } else {
        header.short.to_owned()
    };
    let count = |group| {
        screen
            .items
            .iter()
            .filter(|item| item.group == group)
            .count()
    };
    let counts = [
        (count(Group::NeedsYou), "needs you", "need you"),
        (count(Group::InProgress), "in progress", "in progress"),
        (count(Group::Done), "done", "done"),
    ]
    .into_iter()
    .filter(|(n, _, _)| *n > 0)
    .map(|(n, one, many)| format!("{n} {}", if n == 1 { one } else { many }))
    .collect::<Vec<_>>()
    .join(" · ");
    let counts = fold(&counts, ascii);
    let separator = fold(" · ", ascii);
    let left = root.width() + separator.width() + status.width();
    let status_style = if header.unwatched {
        palette.warning
    } else {
        palette.foreground
    };
    if !counts.is_empty() && left + 2 + counts.width() <= width {
        return Line::from(vec![
            Span::styled(root, palette.foreground.bold()),
            Span::styled(separator, palette.foreground),
            Span::styled(status, status_style),
            Span::raw(" ".repeat(width - left - counts.width())),
            Span::styled(counts, palette.foreground),
        ]);
    }
    let status = fit(
        &status,
        width.saturating_sub(root.width() + separator.width()),
        ascii,
    );
    Line::from(vec![
        Span::styled(root, palette.foreground.bold()),
        Span::styled(separator, palette.foreground),
        Span::styled(status, status_style),
    ])
}

/// Where the list starts so the selected row stays in view.
fn scrolled(selected: Option<usize>, visible: usize) -> usize {
    selected.map_or(0, |i| (i + 1).saturating_sub(visible.max(1)))
}

const AGENT: usize = 8;
const STATE: usize = 9;
const VERDICT: usize = 11;
const CHECKS: usize = 12;

fn table<'a>(
    screen: &'a Screen,
    selected: Option<usize>,
    area: Rect,
    palette: &Theme,
) -> Table<'a> {
    let ascii = screen.ascii;
    let width = usize::from(area.width);
    // Marker, then the fixed columns, each with one cell of spacing.
    let fixed = 1 + AGENT + STATE + VERDICT + CHECKS + 6;
    // The Work column: as wide as the longest name, 12 to 28 cells.
    let bases: Vec<(String, &str)> = screen
        .items
        .iter()
        .map(|item| (fold(&item.base, ascii), item.id.as_str()))
        .collect();
    let longest = tell_apart(&bases, 28, ascii)
        .iter()
        .map(|name| name.width())
        .max()
        .unwrap_or(0);
    let work = longest
        .clamp(12, 28)
        .min(width.saturating_sub(fixed + 10))
        .max(1);
    let touches = width.saturating_sub(fixed + work);
    let names = tell_apart(&bases, work, ascii);
    let visible = usize::from(area.height).saturating_sub(1);
    let skip = scrolled(selected, visible);
    let rows = screen
        .items
        .iter()
        .zip(names)
        .enumerate()
        .skip(skip)
        .map(|(i, (item, name))| {
            let on = Some(i) == selected;
            let done = item.group == Group::Done;
            let tone = |tone: Tone| {
                if done {
                    palette.inactive
                } else {
                    style(tone, palette)
                }
            };
            let name_style = if on {
                tone(Tone::Plain).bold()
            } else {
                tone(Tone::Plain)
            };
            let cell = |text: String, style: Style| Cell::from(Span::styled(text, style));
            Row::new(vec![
                cell(
                    if on { fold("›", ascii) } else { " ".into() },
                    palette.focus,
                ),
                cell(name, name_style),
                cell(
                    fit(&fold(&item.agent, ascii), AGENT, ascii),
                    tone(Tone::Plain),
                ),
                cell(item.state.to_owned(), tone(Tone::Plain)),
                cell(fold(item.verdict, ascii), tone(item.verdict_tone)),
                cell(
                    item.checks.to_owned(),
                    tone(if item.checks == "failed" {
                        Tone::Error
                    } else {
                        Tone::Plain
                    }),
                ),
                cell(
                    touched(&item.touches, touches, ascii, false),
                    tone(Tone::Plain),
                ),
            ])
        })
        .collect::<Vec<_>>();
    let titles = ["", "WORK", "AGENT", "STATE", "VERDICT", "CHECKS", "TOUCHES"];
    Table::new(
        rows,
        [1, work, AGENT, STATE, VERDICT, CHECKS, touches]
            .map(|width| Constraint::Length(width as u16)),
    )
    .column_spacing(1)
    .header(Row::new(titles).style(palette.secondary))
}

/// Other Work's names in `width` cells: as many as fit, then `… +N`; `?`
/// while unknown, `–` when none. `count` gives `N` alone when the names
/// don't fit.
fn touched(touches: &Touches, width: usize, ascii: bool, count: bool) -> String {
    let names = match touches {
        Touches::Unknown => return "?".into(),
        Touches::Names(names) if names.is_empty() => return fold("–", ascii),
        Touches::Names(names) => names,
    };
    let all = fold(&names.join(", "), ascii);
    if all.width() <= width || count {
        return if all.width() <= width {
            all
        } else {
            names.len().to_string()
        };
    }
    for shown in (1..names.len()).rev() {
        let text = fold(
            &format!("{}… +{}", names[..shown].join(", "), names.len() - shown),
            ascii,
        );
        if text.width() <= width {
            return text;
        }
    }
    let more = if names.len() > 1 {
        format!(" +{}", names.len() - 1)
    } else {
        String::new()
    };
    format!(
        "{}{more}",
        fit(
            &fold(&names[0], ascii),
            width.saturating_sub(more.len()),
            ascii
        )
    )
}

fn cards(
    screen: &Screen,
    selected: Option<usize>,
    area: Rect,
    palette: &Theme,
) -> Vec<Line<'static>> {
    let ascii = screen.ascii;
    let width = usize::from(area.width);
    let verdict_width = screen
        .items
        .iter()
        .map(|item| fold(item.verdict, ascii).width())
        .max()
        .unwrap_or(1);
    let name_width = width.saturating_sub(3 + verdict_width);
    let bases: Vec<(String, &str)> = screen
        .items
        .iter()
        .map(|item| (fold(&item.base, ascii), item.id.as_str()))
        .collect();
    let names = tell_apart(&bases, name_width, ascii);
    let skip = scrolled(selected, usize::from(area.height) / 2);
    let mut lines = Vec::new();
    for (i, (item, name)) in screen.items.iter().zip(names).enumerate().skip(skip) {
        let on = Some(i) == selected;
        let done = item.group == Group::Done;
        let tone = |tone: Tone| {
            if done {
                palette.inactive
            } else {
                style(tone, palette)
            }
        };
        let verdict = fold(item.verdict, ascii);
        let marker = if on { fold("› ", ascii) } else { "  ".into() };
        let gap = width.saturating_sub(2 + name.width() + verdict.width());
        lines.push(Line::from(vec![
            Span::styled(marker, palette.focus),
            Span::styled(
                name,
                if on {
                    tone(Tone::Plain).bold()
                } else {
                    tone(Tone::Plain)
                },
            ),
            Span::raw(" ".repeat(gap)),
            Span::styled(verdict, tone(item.verdict_tone)),
        ]));
        let separator = fold(" · ", ascii);
        let mut second = vec![
            Span::styled(
                format!("    {}{separator}checks ", item.state),
                tone(Tone::Plain),
            ),
            Span::styled(
                item.checks.to_owned(),
                tone(if item.checks == "failed" {
                    Tone::Error
                } else {
                    Tone::Plain
                }),
            ),
        ];
        let used: usize = second.iter().map(|span| span.width()).sum();
        if item.touches != Touches::Names(Vec::new()) {
            let lead = format!("{separator}touches ");
            let room = width.saturating_sub(used + lead.width());
            let names = touched(&item.touches, room, ascii, true);
            if names.width() <= room {
                second.push(Span::styled(format!("{lead}{names}"), tone(Tone::Plain)));
            }
        }
        lines.push(Line::from(second));
    }
    lines
}

/// The selected Work's labelled details and Next, each wrapped under its
/// label, with the rank each is dropped by.
fn details(
    item: &Item,
    width: usize,
    ascii: bool,
    palette: &Theme,
) -> Vec<(u8, Vec<Line<'static>>)> {
    let next = Detail::new("Next", item.next.clone(), 0);
    // Color reinforces the verdict and failed checks, as in the list.
    let lead = |label| match label {
        "Verdict" => item.verdict_tone,
        "Checks" if item.checks == "failed" => Tone::Error,
        _ => Tone::Plain,
    };
    item.details
        .iter()
        .chain([&next])
        .map(|detail| {
            let mut lines = Vec::new();
            for (n, line) in detail.lines.iter().enumerate() {
                for (m, part) in wrap(&fold(line, ascii), width.saturating_sub(LABEL))
                    .into_iter()
                    .enumerate()
                {
                    let label = if n == 0 && m == 0 { detail.label } else { "" };
                    let tone = lead(label);
                    let mut spans = vec![Span::styled(pad(label, LABEL), palette.secondary)];
                    spans.extend(led(part, tone, palette));
                    lines.push(Line::from(spans));
                }
            }
            (detail.rank, lines)
        })
        .collect()
}

/// The details within `height` lines: Began goes first, then Touches,
/// Checks, then the verdict's extra lines. Should the rest still not fit,
/// wrapped lines are cut from the bottom, Next's before the verdict's.
fn fitted(mut groups: Vec<(u8, Vec<Line<'static>>)>, height: usize) -> Vec<Line<'static>> {
    let total = |groups: &[(u8, Vec<Line>)]| groups.iter().map(|(_, l)| l.len()).sum::<usize>();
    for rank in (1..=4).rev() {
        if total(&groups) <= height {
            break;
        }
        groups.retain(|(r, _)| *r != rank);
    }
    let mut excess = total(&groups).saturating_sub(height);
    for (_, lines) in groups.iter_mut().rev() {
        let cut = excess.min(lines.len() - 1);
        lines.truncate(lines.len() - cut);
        excess -= cut;
    }
    groups
        .into_iter()
        .flat_map(|(_, lines)| lines)
        .take(height)
        .collect()
}

/// The details' first line: the name, how the Work came to be listed, where
/// it is when that fits, and always its ID: the name is cut to make room.
fn first_detail(item: &Item, width: usize, ascii: bool, palette: &Theme) -> Line<'static> {
    let mut name = fold(&item.name, ascii);
    let separator = fold(" · ", ascii);
    let id = format!("{separator}Work {}", item.short);
    let origin = fold(&format!(" · {}", item.origin), ascii);
    let place = item
        .place
        .as_ref()
        .map(|place| fold(&format!(" · {place}"), ascii))
        .unwrap_or_default();
    let mut rest = format!("{origin}{place}{id}");
    if name.width() + rest.width() > width {
        rest = format!("{origin}{id}");
    }
    if name.width() + rest.width() > width {
        rest = format!(
            "{}{id}",
            fit(
                &origin,
                width.saturating_sub(name.width() + id.width()),
                ascii
            )
        );
    }
    name = fit(&name, width.saturating_sub(rest.width()), ascii);
    Line::from(vec![
        Span::styled(name, palette.foreground.bold()),
        Span::styled(rest, palette.foreground),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AttachCapabilities, AttachConfidence, AttachmentRecord, BaselineProvenance, OwnerState,
        Reason, ReasonCode, RuntimeSession, Validity, WorkspaceOwner, WorkspaceRemoval,
        coherence::interactions::{Edge, Participant},
    };
    use ratatui::{backend::TestBackend, buffer::Buffer, style::Color};

    const ROOT: &str = "/home/me/tinyauth";
    const REASON: &str = "def validate(token): => def validate(ctx, token):";

    fn run(id: &str, task: &str, minute: u32) -> RunRecord {
        serde_json::from_value(serde_json::json!({
            "id": id, "task": task, "exact_prompt": task,
            "source_path": ROOT, "source_kind": "git_worktree", "source_git_head": "5e3cb514aaaa",
            "source_fingerprint": "f", "baseline_path": "/baseline", "baseline_commit": "abc",
            "status": "running", "created_at": format!("2026-10-08T06:{minute:02}:00Z"),
            "completed_at": null,
            "environment": {"dispatch_version":"test","os":"test","architecture":"test","execution_backend":"local","timeout_secs":30,"cpus":1.0,"memory":"1g","max_parallel":1},
            "evaluation": null, "applied_candidate": null
        }))
        .unwrap()
    }

    fn ready(mut run: RunRecord, decision: Option<(Decision, bool)>) -> RunRecord {
        run.outcome.lifecycle = LifecycleState::Finished;
        run.outcome.work_result = WorkResult::Ready;
        run.outcome.verification = crate::VerificationState::Passed;
        if let Some((decision, moved)) = decision {
            judge(&mut run, decision, moved, &[REASON]);
        }
        run
    }

    fn judge(run: &mut RunRecord, decision: Decision, moved: bool, reasons: &[&str]) {
        run.coherence = Some(crate::CoherenceRecord {
            version: 1,
            refreshed_from: None,
            validity: Some(Validity {
                decision,
                evaluated_at: chrono::Utc::now(),
                world_digest: String::new(),
                world_changed: moved || decision != Decision::Continue,
                changed_files: 1,
                reasons: if decision == Decision::Continue {
                    Vec::new()
                } else {
                    reasons
                        .iter()
                        .map(|detail| Reason {
                            code: ReasonCode::FactBroken,
                            fact_id: None,
                            path: None,
                            detail: (*detail).to_owned(),
                        })
                        .collect()
                },
                analysis: crate::AnalysisLevel::Symbols,
            }),
            first_invalid_at: None,
            overridden: None,
        });
    }

    fn attached(mut run: RunRecord, workspace: &str, owner: WorkspaceOwner) -> RunRecord {
        run.mode = RunMode::Attached;
        run.attachment = Some(AttachmentRecord {
            version: 1,
            workspace: PathBuf::from(workspace),
            integration_root: PathBuf::from(ROOT),
            repo_key: None,
            provenance: BaselineProvenance::WorkspaceAtStart {
                commit: "e4a15cd9ffffffff".into(),
            },
            confidence: AttachConfidence::Full,
            agent: Some("claude".into()),
            command: None,
            owner: None,
            agent_process: None,
            owner_state: OwnerState::Unknown,
            capabilities: AttachCapabilities {
                observe: true,
                signal: false,
                control: false,
                integrate: true,
            },
            attached_at: chrono::Utc::now(),
            finished_at: None,
            finish_reason: None,
            workspace_owner: owner,
            managed: None,
            sessions: vec![RuntimeSession {
                provider: "claude".into(),
                session_id: "s1".into(),
                source: "startup".into(),
                started_at: chrono::Utc::now(),
                ended_at: None,
                end_reason: None,
                model: None,
            }],
            workspace_removed: None,
        });
        run
    }

    fn working(mut run: RunRecord) -> RunRecord {
        run.outcome.lifecycle = LifecycleState::Working;
        run
    }

    fn worktree(id: &str, folder: &str, minute: u32) -> RunRecord {
        attached(
            run(id, &format!("attached work in {folder}"), minute),
            &format!("{ROOT}/.claude/worktrees/{folder}"),
            WorkspaceOwner::Runtime,
        )
    }

    fn items_with(runs: &[RunRecord], projection: Option<&Projection>) -> Vec<Item> {
        let checks: HashMap<String, (Vec<String>, bool)> = runs
            .iter()
            .map(|run| (run.id.clone(), (vec!["pytest".to_owned()], true)))
            .collect();
        build(
            runs,
            &Context {
                root: Path::new(ROOT),
                home: Some(Path::new("/home/me")),
                watched: true,
                projection,
                checks: &checks,
            },
        )
    }

    fn items(runs: &[RunRecord]) -> Vec<Item> {
        items_with(runs, None)
    }

    fn header(unwatched: bool) -> Header {
        Header {
            root: "tinyauth".into(),
            status: if unwatched {
                "not watched · start watching: dispatch start".into()
            } else {
                "watched in the background since 06:48".into()
            },
            short: if unwatched { "not watched" } else { "watched" },
            unwatched,
            extra: String::new(),
        }
    }

    fn no_color() -> Theme {
        Theme::from_hints(true, None, None, None, None)
    }

    fn truecolor() -> Theme {
        Theme::from_hints(false, Some("truecolor"), Some("dark"), None, None)
    }

    fn draw(
        items: &[Item],
        selected: Option<&str>,
        size: (u16, u16),
        ascii: bool,
        palette: &Theme,
    ) -> Buffer {
        let header = header(false);
        let screen = Screen {
            header: &header,
            items,
            selected,
            notice: None,
            ascii,
        };
        let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
        terminal.draw(|f| render(f, &screen, palette)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn rows(buffer: &Buffer) -> Vec<String> {
        buffer
            .content
            .chunks(usize::from(buffer.area.width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect()
    }

    /// The cell and row where `text` is drawn.
    fn find(buffer: &Buffer, text: &str) -> Option<(u16, u16)> {
        rows(buffer).iter().enumerate().find_map(|(y, row)| {
            row.find(text)
                .map(|at| (row[..at].chars().count() as u16, y as u16))
        })
    }

    /// A project as in the demo: Work in every group.
    fn demo() -> Vec<RunRecord> {
        let me = ready(
            worktree("01M4DRXH00000000000000000B", "me-endpoint", 3),
            Some((Decision::Refresh, true)),
        );
        let mut auth = ready(worktree("01M4DRX900000000000000000A", "auth-ctx", 1), None);
        auth.outcome.application = ApplicationState::Applied;
        auth.outcome.applied_by = Some(crate::AppliedBy::Human);
        let slug = ready(
            worktree("01M4DRXS00000000000000000C", "slugify", 2),
            Some((Decision::Continue, true)),
        );
        let fix = working(attached(
            run("01M4DRXZ00000000000000000D", "Fix the cache", 4),
            "/home/me/elsewhere/cache",
            WorkspaceOwner::User,
        ));
        vec![me, auth, slug, fix]
    }

    #[test]
    fn work_that_needs_you_comes_first_then_by_when_it_began() {
        let listed: Vec<(String, &str, &str)> = items(&demo())
            .into_iter()
            .map(|item| (item.name, item.state, item.verdict))
            .collect();
        assert_eq!(
            listed,
            [
                ("slugify".to_owned(), "ready", "CONTINUE"),
                ("me-endpoint".to_owned(), "blocked", "REFRESH"),
                ("cache".to_owned(), "working", "not checked"),
                ("auth-ctx".to_owned(), "applied", "–"),
            ]
        );
    }

    /// One item per state, with its Next text and keys (§3.8).
    #[test]
    fn every_state_says_what_comes_next_and_offers_its_keys() {
        let id = "01M4DRXH00000000000000000B";
        let mut failed = ready(run(id, "t", 1), Some((Decision::Continue, false)));
        failed.outcome.verification = crate::VerificationState::Failed;
        let mut applied = ready(run(id, "t", 1), None);
        applied.outcome.application = ApplicationState::Applied;
        applied.outcome.applied_by = Some(crate::AppliedBy::Human);
        let mut rejected = ready(run(id, "t", 1), None);
        rejected.outcome.review = ReviewState::Rejected;
        let mut advisory = working(worktree(id, "wt", 1));
        judge(&mut advisory, Decision::Refresh, true, &[REASON]);
        let mut question = working(run(id, "t", 1));
        question.outcome.waiting_on = WaitingOn::Human;
        let mut idle = working(worktree(id, "wt", 1));
        idle.attachment.as_mut().unwrap().sessions[0].ended_at = Some(chrono::Utc::now());
        let removed = |exact| {
            let mut run = working(worktree(id, "wt", 1));
            run.attachment.as_mut().unwrap().workspace_removed = Some(WorkspaceRemoval {
                at: chrono::Utc::now(),
                exact,
            });
            run
        };
        let cases: Vec<(RunRecord, &str, &str, &str, &[&str])> = vec![
            (
                ready(run(id, "t", 1), Some((Decision::Continue, false))),
                "ready",
                "CONTINUE",
                "a accept: applies it to tinyauth.",
                &[ACCEPT, REVIEW, REJECT],
            ),
            (
                ready(run(id, "t", 1), Some((Decision::Continue, true))),
                "ready",
                "CONTINUE",
                "a accept: runs your checks on the merged result, then applies it to tinyauth.",
                &[ACCEPT, REVIEW, REJECT],
            ),
            (
                ready(run(id, "t", 1), None),
                "ready",
                "not checked",
                "a accept: Dispatch checks it against the source first.",
                &[ACCEPT, REVIEW, REJECT],
            ),
            (
                ready(run(id, "t", 1), Some((Decision::Refresh, true))),
                "blocked",
                "REFRESH",
                "It can't be accepted as is. r reject it, then run the agent again from the current source. Or: dispatch refresh 01M4DRXH.",
                &[REJECT, REVIEW],
            ),
            (
                ready(worktree(id, "wt", 1), Some((Decision::Refresh, true))),
                "blocked",
                "REFRESH",
                "It can't be accepted as is. r reject it, then run the agent again from the current source.",
                &[REJECT, REVIEW],
            ),
            (
                ready(run(id, "t", 1), Some((Decision::Stop, true))),
                "blocked",
                "STOP",
                "Its changes are already in the source. r reject it.",
                &[REJECT],
            ),
            (
                failed,
                "ready",
                "CONTINUE",
                "Its checks failed. d review the output, then a accept or r reject.",
                &[REVIEW, ACCEPT, REJECT],
            ),
            (
                applied,
                "applied",
                "–",
                "Nothing to do: it is in tinyauth.",
                &[],
            ),
            (rejected, "rejected", "–", "Nothing to do.", &[]),
            (
                advisory,
                "working",
                "REFRESH",
                "When the agent is done, f finish: freezes its changes and runs pytest.",
                &[FINISH],
            ),
            (
                working(run(id, "t", 1)),
                "working",
                "not checked",
                "Dispatch is running it. It is listed as ready when it is done.",
                &[],
            ),
            (
                question,
                "question",
                "not checked",
                "It is waiting for an answer: dispatch status 01M4DRXH shows the question.",
                &[],
            ),
            (
                idle,
                "idle",
                "not checked",
                "No session is open. f finish to freeze its changes, or resume the session in its worktree.",
                &[FINISH],
            ),
            (
                removed(true),
                "removed",
                "not checked",
                "Its worktree was removed and its exact changes were kept. f finish to check them.",
                &[FINISH],
            ),
            (
                removed(false),
                "lost",
                "not checked",
                "Its worktree is gone; only its last-seen changes were kept. f finish to freeze them, then review.",
                &[FINISH],
            ),
        ];
        for (run, state, verdict, next, keys) in cases {
            let item = items(&[run]).remove(0);
            assert_eq!((item.state, item.verdict), (state, verdict), "{next}");
            assert_eq!(item.next, next);
            assert_eq!(item.keys, keys, "{next}");
            let buffer = draw(
                std::slice::from_ref(&item),
                Some(&item.id),
                (120, 30),
                false,
                &no_color(),
            );
            let hint = rows(&buffer)
                .into_iter()
                .rfind(|row| row.contains("q leave"))
                .unwrap();
            assert!(hint.trim().starts_with(&keys.join(" · ")), "{hint}");
        }
        // Moved, but nothing to run on the merged result.
        let moved = ready(run(id, "t", 1), Some((Decision::Continue, true)));
        let checks = HashMap::from([(id.to_owned(), (Vec::new(), true))]);
        let item = build(
            &[moved],
            &Context {
                root: Path::new(ROOT),
                home: None,
                watched: true,
                projection: None,
                checks: &checks,
            },
        )
        .remove(0);
        assert_eq!(item.next, "a accept: applies it to tinyauth.");
    }

    fn lines(item: &Item, label: &str) -> Vec<String> {
        let at = item
            .details
            .iter()
            .position(|d| d.label == label)
            .unwrap_or_else(|| panic!("no {label} in {:?}", item.details));
        let mut lines = item.details[at].lines.clone();
        lines.extend(
            item.details[at + 1..]
                .iter()
                .take_while(|d| d.label.is_empty())
                .flat_map(|d| d.lines.clone()),
        );
        lines
    }

    #[test]
    fn details_say_the_verdict_how_it_landed_and_what_it_began_against() {
        let id = "01M4DRXH00000000000000000B";
        let mut advisory = working(worktree(id, "me-endpoint", 1));
        judge(
            &mut advisory,
            Decision::Refresh,
            true,
            &[REASON, "two", "three", "four", "five"],
        );
        let item = items(&[advisory]).remove(0);
        assert_eq!(
            lines(&item, "Verdict"),
            [
                format!("REFRESH · {REASON}"),
                "two".into(),
                "three".into(),
                "+2 more: dispatch check 01M4DRXH".into(),
                "Advisory while the agent works: nothing is stopped.".into(),
            ]
        );
        assert_eq!(
            lines(&item, "Began"),
            [
                "a Dispatch snapshot (e4a15cd9) of its worktree when its first session started, not a commit on any branch"
            ]
        );
        assert_eq!(item.origin, "claude session in its own worktree");
        assert_eq!(item.place.as_deref(), Some(".claude/worktrees/me-endpoint"));
        let elsewhere = items(&[working(attached(
            run(id, "t", 1),
            "/home/me/elsewhere/cache",
            WorkspaceOwner::User,
        ))]);
        assert_eq!(elsewhere[0].place.as_deref(), Some("~/elsewhere/cache"));

        let unmoved = items(&[ready(run(id, "t", 1), Some((Decision::Continue, false)))]);
        assert_eq!(
            lines(&unmoved[0], "Verdict"),
            ["CONTINUE · the source has not moved since it began"]
        );
        assert_eq!(
            lines(&unmoved[0], "Began"),
            ["a Dispatch snapshot of the project's working tree at 5e3cb514"]
        );
        assert_eq!(unmoved[0].origin, "unknown · launched by Dispatch");
        let moved = items(&[ready(run(id, "t", 1), Some((Decision::Continue, true)))]);
        assert_eq!(
            lines(&moved[0], "Verdict"),
            ["CONTINUE · the source moved; nothing this Work relies on changed"]
        );

        let mut applied = ready(run(id, "t", 1), None);
        applied.outcome.application = ApplicationState::Applied;
        applied.outcome.applied_by = Some(crate::AppliedBy::AutoApply);
        let applied = items(&[applied]);
        assert_eq!(
            lines(&applied[0], "Landed"),
            ["applied by auto-apply · it was CONTINUE (the source had not moved)"]
        );
        assert!(applied[0].details.iter().all(|d| d.label != "Verdict"));

        let mut rejected = ready(run(id, "t", 1), None);
        rejected.outcome.review = ReviewState::Rejected;
        assert_eq!(
            lines(&items(&[rejected])[0], "Outcome"),
            ["Rejected · nothing was applied"]
        );

        let mut failed = ready(run(id, "t", 1), None);
        failed.outcome.verification = crate::VerificationState::Failed;
        assert_eq!(
            lines(&items(&[failed])[0], "Checks"),
            ["failed · d review the output"]
        );
    }

    #[test]
    fn names_are_folders_then_tasks_then_ids_and_duplicates_are_told_apart() {
        let made = attached(
            run("01M4DRX900000000000000000A", "Add a /me endpoint", 1),
            "/home/me/.dispatch/workspaces/01M4DRX900000000000000000A",
            WorkspaceOwner::Dispatch,
        );
        let generic = attached(
            run("01M4DRXH00000000000000000B", "attached work in tinyauth", 2),
            "/home/me/.dispatch/workspaces/01M4DRXH00000000000000000B",
            WorkspaceOwner::Dispatch,
        );
        let a = worktree("01M4DRXS00000000000000000C", "slugify", 3);
        let mut b = worktree("01M4DRXZ00000000000000000D", "slugify", 4);
        b.attachment.as_mut().unwrap().workspace = PathBuf::from("/elsewhere/slugify");
        let names: Vec<String> = items(&[made, generic, a, b])
            .into_iter()
            .map(|item| item.name)
            .collect();
        assert_eq!(
            names,
            [
                "Add a /me endpoint",
                "01M4DRXH",
                "slugify 01M4DRXS",
                "slugify 01M4DRXZ"
            ]
        );
        // At least 4 characters of the ID.
        let short = tell_apart(
            &[("x".into(), "ABCDEF"), ("x".into(), "ZBCDEF")],
            usize::MAX,
            false,
        );
        assert_eq!(short, ["x ABCD", "x ZBCD"]);

        // Names cut to the same text are told apart too; the ID is never cut.
        let long = |id, minute| {
            worktree(
                id,
                &format!("a-very-long-worktree-name-for-the-feature-{minute}"),
                minute,
            )
        };
        let listed = items(&[
            long("01M4DRX900000000000000000A", 1),
            long("01M4DRXH00000000000000000B", 2),
        ]);
        assert_eq!(
            listed[0].name,
            "a-very-long-worktree-name-for-the-feature-1"
        );
        let text = rows(&draw(
            &listed,
            Some(&listed[0].id),
            (120, 30),
            false,
            &no_color(),
        ))
        .join("\n");
        assert!(text.contains(" a-very-long-worktr… 01M4DRX9 "), "{text}");
        assert!(text.contains(" a-very-long-worktr… 01M4DRXH "), "{text}");
        let narrow = rows(&draw(
            &listed,
            Some(&listed[0].id),
            (60, 20),
            false,
            &no_color(),
        ))
        .join("\n");
        assert!(
            narrow.contains("a-very-long-worktree-name-for-the-feature-1"),
            "{narrow}"
        );
        let narrower = rows(&draw(
            &listed,
            Some(&listed[0].id),
            (44, 20),
            false,
            &no_color(),
        ))
        .join("\n");
        assert!(
            narrower.contains(" a-very-long-worktree-name-fo… 01M4DRX9 "),
            "{narrower}"
        );
        assert!(
            narrower.contains(" a-very-long-worktree-name-fo… 01M4DRXH "),
            "{narrower}"
        );
    }

    #[test]
    fn every_name_state_and_verdict_is_visible_on_one_line_at_every_width() {
        let listed = items(&demo());
        for (width, height) in [(60, 20), (80, 24), (120, 30), (200, 30)] {
            for ascii in [false, true] {
                let buffer = draw(
                    &listed,
                    Some(&listed[1].id),
                    (width, height),
                    ascii,
                    &no_color(),
                );
                let text = rows(&buffer);
                let wide = width >= WIDE;
                assert_eq!(
                    text.iter()
                        .any(|row| row.contains("WORK") && row.contains("TOUCHES")),
                    wide,
                    "{width}: {text:#?}"
                );
                for item in &listed {
                    let verdict = fold(item.verdict, ascii);
                    let (_, y) = find(&buffer, &format!(" {} ", item.name))
                        .unwrap_or_else(|| panic!("{} at {width}: {text:#?}", item.name));
                    let row = &text[usize::from(y)];
                    assert!(row.contains(&format!(" {verdict}")), "{width}: {row}");
                    if wide {
                        assert!(row.contains(&format!(" {} ", item.state)), "{width}: {row}");
                    } else {
                        let next = &text[usize::from(y) + 1];
                        assert!(
                            next.starts_with(&format!("     {} ", item.state)),
                            "{width}: {next}"
                        );
                    }
                }
                if ascii {
                    assert!(text.iter().all(|row| row.is_ascii()), "{text:#?}");
                }
            }
        }
    }

    #[test]
    fn at_sixty_columns_the_reason_and_next_of_the_selection_are_visible() {
        let listed = items(&demo());
        let me = listed
            .iter()
            .find(|item| item.name == "me-endpoint")
            .unwrap();
        let buffer = draw(&listed, Some(&me.id), (60, 20), false, &no_color());
        let text = rows(&buffer).join(" ");
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            text.contains(&format!("Verdict REFRESH · {REASON}")),
            "{text}"
        );
        assert!(
            text.contains("Next It can't be accepted as is. r reject it, then run the agent again from the current source."),
            "{text}"
        );
        assert!(
            text.contains("r reject · d review · ↑↓ · q leave"),
            "{text}"
        );
        assert!(!text.contains("Shift+Tab"), "{text}");
    }

    #[test]
    fn the_wide_view_is_a_table_with_the_selected_work_detailed() {
        let listed = items(&demo());
        let me = listed
            .iter()
            .find(|item| item.name == "me-endpoint")
            .unwrap();
        let buffer = draw(&listed, Some(&me.id), (120, 30), false, &no_color());
        let text: Vec<String> = rows(&buffer)
            .iter()
            .map(|row| row.trim_end().to_owned())
            .collect();
        assert!(
            text[0].starts_with(" tinyauth · watched in the background since 06:48"),
            "{}",
            text[0]
        );
        assert!(
            text[0].ends_with("2 need you · 1 in progress · 1 done"),
            "{}",
            text[0]
        );
        assert!(
            text.iter().any(|row| row.starts_with(" › me-endpoint ")),
            "{text:#?}"
        );
        assert!(
            text.iter().any(|row| row.trim()
                == "me-endpoint · claude session in its own worktree · .claude/worktrees/me-endpoint · Work 01M4DRXH"),
            "{text:#?}"
        );
        assert!(
            text.iter().any(|row| row.trim()
                == "Began    a Dispatch snapshot (e4a15cd9) of its worktree when its first session started, not a commit on any branch"),
            "{text:#?}"
        );
        assert!(
            text.iter()
                .any(|row| row.trim() == "r reject · d review · ↑↓ select · q leave"),
            "{text:#?}"
        );
        assert!(text.iter().all(|row| !row.contains("S0")), "{text:#?}");
    }

    #[test]
    fn under_limited_height_began_goes_first_and_next_stays() {
        let listed = items(&demo());
        let me = listed
            .iter()
            .find(|item| item.name == "me-endpoint")
            .unwrap();
        let text = rows(&draw(&listed, Some(&me.id), (120, 12), false, &no_color())).join("\n");
        assert!(!text.contains("Began"), "{text}");
        assert!(!text.contains("Checks   passed"), "{text}");
        assert!(text.contains("Next"), "{text}");
        assert!(text.contains("Verdict  REFRESH"), "{text}");
        assert!(text.contains("me-endpoint · claude"), "{text}");
    }

    #[test]
    fn below_forty_columns_only_asks_for_a_wider_terminal() {
        let listed = items(&demo());
        let text = rows(&draw(&listed, None, (39, 10), false, &no_color()));
        assert_eq!(text[0].trim(), "Widen the terminal to at least 40");
        assert_eq!(text[1].trim(), "columns.");
        assert_eq!(text[2].trim(), "q leave");
    }

    #[test]
    fn an_empty_view_says_what_would_appear_or_that_nothing_watches() {
        for (unwatched, line) in [
            (false, "A Claude Code session in its own worktree"),
            (true, "Nothing is watching this project: dispatch start."),
        ] {
            let header = header(unwatched);
            let screen = Screen {
                header: &header,
                items: &[],
                selected: None,
                notice: None,
                ascii: false,
            };
            let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
            terminal.draw(|f| render(f, &screen, &no_color())).unwrap();
            let text = rows(terminal.backend().buffer()).join("\n");
            assert!(text.contains("No Work in the last hour."), "{text}");
            assert!(text.contains(line), "{text}");
            assert!(text.contains("↑↓ select · q leave"), "{text}");
        }
    }

    /// Draw `items` with `notice` about the selected Work.
    fn draw_notice(
        items: &[Item],
        selected: &Item,
        notice: (Kind, &str),
        size: (u16, u16),
        palette: &Theme,
    ) -> Buffer {
        let header = header(false);
        let notice = Notice::new(Some(selected), notice.0, notice.1);
        let screen = Screen {
            header: &header,
            items,
            selected: Some(&selected.id),
            notice: Some((notice.kind, notice.shown(items))),
            ascii: false,
        };
        let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
        terminal.draw(|f| render(f, &screen, palette)).unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn a_notice_names_its_work_and_a_refusal_is_an_error() {
        let listed = items(&demo());
        let palette = truecolor();
        let mut blocked = demo().remove(0);
        blocked.outcome.application = ApplicationState::BlockedBySourceDrift;
        let refused = refusal(Some(&blocked), &anyhow::anyhow!("stale"));
        // At 60 columns the whole refusal shows, down to what to do next.
        let buffer = draw_notice(
            &listed,
            &listed[1],
            (Kind::Refused, &refused),
            (60, 20),
            &palette,
        );
        let (x, y) = find(&buffer, "me-endpoint: not applied: stale (REFRESH).").unwrap();
        assert_eq!(Some(buffer[(x, y)].fg), palette.error.fg);
        let text = rows(&buffer);
        let at = usize::from(y);
        assert_eq!(
            format!("{} {}", text[at].trim(), text[at + 1].trim()),
            "me-endpoint: not applied: stale (REFRESH). The source is unchanged. Next: r reject it."
        );
        assert!(text[at + 2].contains("q leave"), "{text:#?}");
        let mut stop = blocked.clone();
        judge(&mut stop, Decision::Stop, true, &[REASON]);
        let refused = refusal(Some(&stop), &anyhow::anyhow!("stop"));
        let text = rows(&draw_notice(
            &listed,
            &listed[1],
            (Kind::Refused, &refused),
            (60, 20),
            &palette,
        ))
        .join(" ");
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            text.contains("me-endpoint: not applied: STOP, its changes are already in the source. Next: r reject it."),
            "{text}"
        );

        // A longer notice is cut to two lines.
        let long = format!("not applied: {}.", "a long reason ".repeat(10));
        let buffer = draw_notice(
            &listed,
            &listed[1],
            (Kind::Refused, &long),
            (60, 20),
            &palette,
        );
        let (_, y) = find(&buffer, "me-endpoint: not applied: a long").unwrap();
        let text = rows(&buffer);
        let at = usize::from(y);
        assert!(text[at + 1].trim_end().ends_with('…'), "{text:#?}");
        assert!(text[at + 2].contains("q leave"), "{text:#?}");
    }

    #[test]
    fn color_reinforces_the_words_and_no_color_means_none() {
        let listed = items(&demo());
        let palette = truecolor();
        let buffer = draw(&listed, Some(&listed[0].id), (120, 30), false, &palette);
        let at = |text: &str| {
            let (x, y) = find(&buffer, text).unwrap();
            buffer[(x, y)].fg
        };
        assert_eq!(Some(at("REFRESH")), palette.warning.fg);
        assert_eq!(Some(at("CONTINUE")), palette.success.fg);
        assert_eq!(Some(at("›")), palette.focus.fg);
        assert_eq!(Some(at("WORK")), palette.secondary.fg);
        let (_, y) = find(&buffer, " auth-ctx ").unwrap();
        let row = &rows(&buffer)[usize::from(y)];
        for (x, c) in row.chars().enumerate().skip(3) {
            if c != ' ' {
                assert_eq!(Some(buffer[(x as u16, y)].fg), palette.inactive.fg, "{row}");
            }
        }
        // A CONTINUE that cannot be accepted as is is not success.
        let mut failed = ready(
            run("01M4DRXH00000000000000000B", "t", 1),
            Some((Decision::Continue, false)),
        );
        failed.outcome.verification = crate::VerificationState::Failed;
        let failed = items(&[failed]);
        assert_eq!(failed[0].verdict_tone, Tone::Plain);
        let buffer = draw(&failed, Some(&failed[0].id), (120, 30), false, &palette);
        let (x, y) = find(&buffer, "failed ").unwrap();
        assert_eq!(Some(buffer[(x, y)].fg), palette.error.fg);
        assert!(buffer.content.iter().all(|cell| cell.bg == Color::Reset));

        for (width, height) in [(60, 20), (120, 30)] {
            let buffer = draw(
                &listed,
                Some(&listed[1].id),
                (width, height),
                false,
                &no_color(),
            );
            assert!(buffer.content.iter().all(|cell| cell.fg == Color::Reset));
            assert!(buffer.content.iter().all(|cell| cell.bg == Color::Reset));
        }
    }

    #[test]
    fn the_selection_is_work_and_follows_it_through_any_change() {
        let mut runs = demo();
        let mut selection = Selection::default();
        let first = items(&runs);
        assert!(selection.follow(&[], &first).is_none());
        assert_eq!(selection.id.as_deref(), Some(first[0].id.as_str()));
        selection.step(&first, true);
        let me = first[1].id.clone();
        assert_eq!(selection.id.as_deref(), Some(me.as_str()));

        // Work added before and after it.
        let mut grown = runs.clone();
        grown.push(ready(
            worktree("01M4DRX000000000000000000E", "early", 0),
            Some((Decision::Continue, false)),
        ));
        grown.push(working(worktree("01M4DRY000000000000000000F", "late", 9)));
        let after = items(&grown);
        assert!(selection.follow(&first, &after).is_none());
        assert_eq!(selection.id.as_deref(), Some(me.as_str()));
        assert_ne!(after[1].id, me, "the selected row moved");

        // Other Work removed.
        runs.retain(|run| run.id != first[0].id);
        let fewer = items(&runs);
        assert!(selection.follow(&after, &fewer).is_none());
        assert_eq!(selection.id.as_deref(), Some(me.as_str()));

        // A group change: from blocked to applied, last in the list.
        let at = runs.iter().position(|run| run.id == me).unwrap();
        runs[at].outcome.application = ApplicationState::Applied;
        let applied = items(&runs);
        assert!(selection.follow(&fewer, &applied).is_none());
        assert_eq!(selection.id.as_deref(), Some(me.as_str()));
        assert_eq!(applied.last().unwrap().id, me);
        assert!(!selection.moved);

        // Resizing between 200 and 60 columns keeps the same Work selected.
        for width in [200, 60, 120, 200] {
            let buffer = draw(
                &applied,
                selection.id.as_deref(),
                (width, 30),
                false,
                &no_color(),
            );
            assert!(find(&buffer, "› me-endpoint").is_some(), "{width}");
        }
    }

    #[test]
    fn when_the_selection_leaves_the_neighbor_is_selected_and_the_next_key_only_says_so() {
        let runs = demo();
        let before = items(&runs);
        let gone = before[1].id.clone();
        let mut selection = Selection {
            id: Some(gone.clone()),
            moved: false,
        };
        let after = items(
            &runs
                .iter()
                .filter(|run| run.id != gone)
                .cloned()
                .collect::<Vec<_>>(),
        );
        let notice = selection.follow(&before, &after).unwrap();
        assert_eq!(selection.id.as_deref(), Some(after[1].id.as_str()));
        assert!(selection.moved);
        assert_eq!(
            notice.shown(&after),
            format!("me-endpoint left the list; now on {}.", after[1].name)
        );

        // The first press does nothing but say so; the second acts.
        let first = target(&mut selection, &after, "a").unwrap_err();
        assert_eq!(
            first.shown(&after),
            format!(
                "Selection moved to {}. Press a again to act on it.",
                after[1].name
            )
        );
        assert!(!selection.moved);
        let second = target(&mut selection, &after, "a").unwrap().unwrap();
        assert_eq!(second.id, after[1].id);

        // Moving clears it too.
        selection.moved = true;
        selection.step(&after, false);
        assert!(!selection.moved);

        // Work that left the list since it was drawn is never acted on.
        let gone = listed(runs[1..].to_vec(), &runs[0].id).unwrap_err();
        assert_eq!(
            gone.shown(&after),
            "That Work is no longer listed; nothing changed."
        );
        assert_eq!(listed(runs.clone(), &runs[0].id).unwrap().id, runs[0].id);

        // The last item leaving selects the new last; an empty list nothing.
        let mut selection = Selection {
            id: Some(after.last().unwrap().id.clone()),
            moved: false,
        };
        assert!(selection.follow(&after, &after[..2]).is_some());
        assert_eq!(selection.id.as_deref(), Some(after[1].id.as_str()));
        assert!(selection.follow(&after, &[]).is_none());
        assert_eq!(selection.id, None);
        assert!(target(&mut selection, &after, "a").unwrap().is_none());
    }

    fn projection(a: &RunRecord, b: &RunRecord, interactions: Vec<Interaction>) -> Projection {
        let participant = |run: &RunRecord| Participant {
            run_id: run.id.clone(),
            delta_sha256: String::new(),
            analysis: Analysis::Analyzed,
            unresolved: 0,
        };
        Projection {
            version: 1,
            computed_at: chrono::Utc::now(),
            participants: vec![participant(a), participant(b)],
            edges: vec![Edge {
                a: a.id.clone(),
                b: b.id.clone(),
                interactions,
            }],
        }
    }

    #[test]
    fn interactions_name_both_pieces_of_work_from_either_side() {
        let auth = working(worktree("01M4DRX900000000000000000A", "auth-ctx", 1));
        let me = working(worktree("01M4DRXH00000000000000000B", "me-endpoint", 2));
        let symbol = Target::Symbol {
            path: "tinyauth/auth.py".into(),
            name: "validate".into(),
        };
        let file = Target::File {
            path: "tinyauth/app.py".into(),
        };
        let interaction = |rule, writer, change, target: &Target, lines| Interaction {
            rule,
            writer,
            target: target.clone(),
            change,
            lines,
        };
        let found = vec![
            interaction(Rule::SameDeclaration, None, None, &symbol, None),
            interaction(
                Rule::Uses,
                Some(Side::A),
                Some(Change::Signature),
                &symbol,
                None,
            ),
            interaction(
                Rule::File,
                Some(Side::B),
                Some(Change::Deletes),
                &file,
                None,
            ),
            interaction(Rule::File, None, Some(Change::Adds), &file, None),
            interaction(Rule::TextualOverlap, None, None, &file, Some((4, 9))),
        ];
        let both = projection(&auth, &me, found);
        let listed = items_with(&[auth, me], Some(&both));
        let touches = |name: &str| {
            let item = listed.iter().find(|item| item.name == name).unwrap();
            (item.touches.clone(), lines(item, "Touches"))
        };
        let (column, from_auth) = touches("auth-ctx");
        assert_eq!(column, Touches::Names(vec!["me-endpoint".into()]));
        assert_eq!(
            from_auth,
            [
                "auth-ctx and me-endpoint both change validate (tinyauth/auth.py)",
                "auth-ctx changes the signature of validate (tinyauth/auth.py), which me-endpoint uses",
                "me-endpoint deletes tinyauth/app.py, which auth-ctx relies on (whole file)",
                "auth-ctx and me-endpoint both add tinyauth/app.py (whole file)",
                "The edits of auth-ctx and me-endpoint overlap as text at lines 4–9 of tinyauth/app.py",
                "Advisory: nothing is held back.",
            ]
        );
        let (column, from_me) = touches("me-endpoint");
        assert_eq!(column, Touches::Names(vec!["auth-ctx".into()]));
        assert_eq!(from_me[1], from_auth[1]);
        assert_eq!(from_me[2], from_auth[2]);
        for line in from_auth.iter().chain(&from_me) {
            let words: Vec<&str> = line.split_whitespace().collect();
            assert!(!words.contains(&"it"), "{line}");
            assert!(!line.contains("this Work"), "{line}");
            if line != "Advisory: nothing is held back." {
                assert!(
                    line.contains("auth-ctx") && line.contains("me-endpoint"),
                    "{line}"
                );
            }
        }
        // Shown in the details while they fit: one interaction does.
        let pair = demo_pair();
        let one = projection(
            &pair[0],
            &pair[1],
            vec![interaction(
                Rule::Uses,
                Some(Side::B),
                Some(Change::Signature),
                &symbol,
                None,
            )],
        );
        let listed = items_with(&pair, Some(&one));
        let text = rows(&draw(
            &listed,
            Some(&listed[0].id),
            (200, 30),
            false,
            &no_color(),
        ))
        .join("\n");
        assert!(
            text.contains("Touches  me-endpoint changes the signature of validate (tinyauth/auth.py), which auth-ctx uses"),
            "{text}"
        );
        assert!(
            text.contains("         Advisory: nothing is held back."),
            "{text}"
        );
    }

    fn demo_pair() -> [RunRecord; 2] {
        [
            working(worktree("01M4DRX900000000000000000A", "auth-ctx", 1)),
            working(worktree("01M4DRXH00000000000000000B", "me-endpoint", 2)),
        ]
    }

    #[test]
    fn interactions_not_known_yet_show_a_question_mark() {
        let auth = working(worktree("01M4DRX900000000000000000A", "auth-ctx", 1));
        let me = working(worktree("01M4DRXH00000000000000000B", "me-endpoint", 2));
        let mut projection = projection(&auth, &me, Vec::new());
        projection.participants[0].analysis = Analysis::Pending;
        let listed = items_with(&[auth, me], Some(&projection));
        assert_eq!(listed[0].touches, Touches::Unknown);
        assert_eq!(listed[1].touches, Touches::Names(Vec::new()));
        assert!(listed[1].details.iter().all(|d| d.label != "Touches"));
        let text = rows(&draw(
            &listed,
            Some(&listed[0].id),
            (120, 30),
            false,
            &no_color(),
        ))
        .join("\n");
        assert!(
            text.contains("not known yet: its files don't parse while it's being edited"),
            "{text}"
        );
    }

    /// Without a projection nothing says what Work touches: `?`, never `–`.
    #[test]
    fn without_a_projection_touches_are_not_known() {
        let pair = demo_pair();
        let checks = HashMap::new();
        for (watched, why) in [
            (false, "not known: the project is not watched"),
            (true, "not known yet"),
        ] {
            let listed = build(
                &pair,
                &Context {
                    root: Path::new(ROOT),
                    home: None,
                    watched,
                    projection: None,
                    checks: &checks,
                },
            );
            assert!(listed.iter().all(|item| item.touches == Touches::Unknown));
            assert_eq!(lines(&listed[0], "Touches"), [why]);
            let buffer = draw(&listed, Some(&listed[0].id), (120, 30), false, &no_color());
            let text = rows(&buffer);
            let (touches, _) = find(&buffer, "TOUCHES").unwrap();
            for item in &listed {
                let (_, y) = find(&buffer, &format!(" {} ", item.name)).unwrap();
                let row: String = text[usize::from(y)]
                    .chars()
                    .skip(usize::from(touches))
                    .collect();
                assert_eq!(row.trim(), "?", "{text:#?}");
            }
            assert!(
                text.iter()
                    .any(|row| row.trim() == format!("Touches  {why}")),
                "{text:#?}"
            );
            let narrow = rows(&draw(
                &listed,
                Some(&listed[0].id),
                (60, 20),
                false,
                &no_color(),
            ));
            assert!(
                narrow
                    .iter()
                    .any(|row| row.trim_end().ends_with("· touches ?")),
                "{narrow:#?}"
            );
        }
        // Done Work takes no part, watched or not.
        let applied = items(&demo()).pop().unwrap();
        assert_eq!(applied.state, "applied");
        assert_eq!(applied.touches, Touches::Names(Vec::new()));
        assert!(applied.details.iter().all(|d| d.label != "Touches"));
    }

    /// However short the terminal, the selected Work's row shows, with the
    /// header, the notice and the hint; Next's wrapped lines go before the
    /// verdict's.
    #[test]
    fn a_short_terminal_still_shows_the_selected_work() {
        let mut runs = demo();
        runs.extend((5..9).map(|minute| {
            working(worktree(
                &format!("01M4DRY{minute}00000000000000000"),
                &format!("more-{minute}"),
                minute,
            ))
        }));
        let listed = items(&runs);
        let palette = no_color();
        let refused = "not applied: stale (REFRESH). The source is unchanged. Next: r reject it.";
        for item in &listed {
            for size in [(100, 10), (60, 12), (100, 8), (60, 9), (200, 7)] {
                let buffer = draw_notice(&listed, item, (Kind::Refused, refused), size, &palette);
                let text = rows(&buffer);
                assert!(
                    find(&buffer, &format!("› {} ", item.name)).is_some(),
                    "{size:?}: {text:#?}"
                );
                assert!(text[0].contains("tinyauth"), "{size:?}: {text:#?}");
                assert!(
                    text.iter()
                        .any(|row| row.contains(&format!("{}: not applied", item.name))),
                    "{size:?}: {text:#?}"
                );
                assert!(
                    text.iter().any(|row| row.trim_end().ends_with("q leave")),
                    "{size:?}: {text:#?}"
                );
            }
        }
        let me = listed
            .iter()
            .find(|item| item.name == "me-endpoint")
            .unwrap();
        let trimmed = |buffer: &Buffer| {
            rows(buffer)
                .iter()
                .map(|row| row.trim_end())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let buffer = draw_notice(&listed, me, (Kind::Refused, refused), (100, 10), &palette);
        let text = trimmed(&buffer);
        assert!(text.contains("› me-endpoint "), "{text}");
        assert!(text.contains("Verdict  REFRESH · "), "{text}");
        assert!(
            text.contains("Next     It can't be accepted as is."),
            "{text}"
        );
        assert!(!text.contains("current source."), "{text}");
        // Narrow: the verdict wraps too; Next's second line goes first.
        let text = trimmed(&draw(&listed, Some(&me.id), (60, 9), false, &palette));
        assert!(text.contains("› me-endpoint "), "{text}");
        assert!(
            text.contains(
                "Verdict  REFRESH · def validate(token): => def\n          validate(ctx, token):\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("Next     It can't be accepted as is."),
            "{text}"
        );
        assert!(!text.contains("current source."), "{text}");
    }

    /// Seeing every Work matters more than the optional details.
    #[test]
    fn every_work_is_listed_before_optional_details() {
        let runs: Vec<RunRecord> = (1..7)
            .map(|minute| {
                working(worktree(
                    &format!("01M4DRX{minute}00000000000000000"),
                    &format!("work-{minute}"),
                    minute,
                ))
            })
            .collect();
        let listed = items(&runs);
        // A 60x30 terminal: the inline view is at most 20 lines tall.
        for height in [20, 30] {
            let buffer = draw(
                &listed,
                Some(&listed[0].id),
                (60, height),
                false,
                &no_color(),
            );
            for item in &listed {
                assert!(
                    find(&buffer, &format!(" {} ", item.name)).is_some(),
                    "{height}: {:#?}",
                    rows(&buffer)
                );
            }
        }
    }

    #[test]
    fn details_line_one_ends_with_the_work_id_however_long_the_name() {
        let name = format!("feature-{}", "x".repeat(72));
        let listed = items(&[working(worktree("01M4DRX900000000000000000A", &name, 1))]);
        for width in [90, 120, 200] {
            let text = rows(&draw(
                &listed,
                Some(&listed[0].id),
                (width, 30),
                false,
                &no_color(),
            ));
            let line = text
                .iter()
                .find(|row| row.trim_start().starts_with("feature-x") && row.contains(" · "))
                .unwrap_or_else(|| panic!("{width}: {text:#?}"));
            assert!(
                line.trim_end().ends_with(" · Work 01M4DRX9"),
                "{width}: {line}"
            );
            assert_eq!(line.contains(&name), width >= 100, "{width}: {line}");
        }
    }

    #[test]
    fn the_hint_keeps_q_leave_and_drops_offered_keys_from_the_end() {
        let listed = items(&[ready(
            run("01M4DRXH00000000000000000B", "t", 1),
            Some((Decision::Continue, false)),
        )]);
        let hint = |width, ascii| {
            rows(&draw(
                &listed,
                Some(&listed[0].id),
                (width, 20),
                ascii,
                &no_color(),
            ))
            .into_iter()
            .rfind(|row| row.contains("leave"))
            .unwrap()
            .trim()
            .to_owned()
        };
        assert_eq!(
            hint(60, false),
            "a accept · d review · r reject · ↑↓ · q leave"
        );
        assert_eq!(hint(40, false), "a accept · d review · ↑↓ · q leave");
        assert_eq!(hint(40, true), "a accept - up/down - q leave");
    }

    #[test]
    fn text_is_cut_and_wrapped_by_cells_never_bytes() {
        assert_eq!(fit("界界界界", 5, false), "界界…");
        assert_eq!(fit("abcdef", 5, true), "ab...");
        assert_eq!(fit("abc", 3, false), "abc");
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(fold("› a – b … ↑↓ 界", true), "> a - b ... up/down ?");
        assert_eq!(
            touched(
                &Touches::Names(vec!["auth-ctx".into(), "slugify".into(), "me".into()]),
                16,
                false,
                false
            ),
            "auth-ctx… +2"
        );
    }

    #[test]
    fn a_refusal_is_explained_by_its_verdict_only_when_the_gate_blocked_it() {
        let error = anyhow::anyhow!("run x is still working. Finish it first.");
        // Attached Work still working, with an advisory REFRESH: accept is
        // refused because it is not finished, and says so.
        let mut working = working(worktree("01M4DRXH00000000000000000B", "wt", 1));
        judge(&mut working, Decision::Refresh, true, &[REASON]);
        assert_eq!(
            refusal(Some(&working), &error),
            "not applied: run x is still working."
        );
        // A ready result the gate blocked: its stored verdict says why.
        let mut blocked = ready(
            run("01M4DRXH00000000000000000B", "t", 1),
            Some((Decision::Refresh, true)),
        );
        blocked.outcome.application = ApplicationState::BlockedBySourceDrift;
        assert_eq!(
            refusal(Some(&blocked), &error),
            "not applied: stale (REFRESH). The source is unchanged. Next: r reject it."
        );
        judge(&mut blocked, Decision::Stop, true, &[REASON]);
        assert_eq!(
            refusal(Some(&blocked), &error),
            "not applied: STOP, its changes are already in the source. Next: r reject it."
        );
        // A stored REFRESH that the gate did not block on says nothing.
        blocked.outcome.application = ApplicationState::NotApplied;
        assert_eq!(
            refusal(Some(&blocked), &error),
            "not applied: run x is still working."
        );
        assert_eq!(
            refusal(None, &error),
            "not applied: run x is still working."
        );
    }

    #[test]
    fn errors_are_cut_at_their_first_sentence() {
        assert_eq!(
            first_sentence(&anyhow::anyhow!("run x is not ready. Finish it first.")),
            "run x is not ready"
        );
        assert_eq!(first_sentence(&anyhow::anyhow!("no result.")), "no result");
    }
}
