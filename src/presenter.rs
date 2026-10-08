//! Foreground presentation only. Committed outcomes remain execution authority.
use crate::{
    executor::CancellationToken,
    orchestrator::{
        self, ApplyOutcome, Presentation, QuestionCommand, ReviewCommand, RunOutputMode, RunRequest,
    },
    state::State,
    *,
};
use anyhow::{Context, Result};
mod clean;
mod handoff;
mod inspection;
mod setup;
mod theme;
mod watch;
pub use clean::clean;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, terminal,
};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::Rect,
    style::Style,
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
};
use ratatui_textarea::TextArea;
pub use setup::standalone as resource_setup;
use std::{
    future::Future,
    io::{self, IsTerminal, Write},
    time::{Duration, Instant},
};
use theme::Theme;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
pub use watch::interactive as watch;

#[derive(Clone, Copy, Default)]
pub struct Options {
    pub plain: bool,
    pub ascii: bool,
    pub no_color: bool,
}

pub fn suitable() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Strip terminal control strings as units, including OSC clipboard/title and
/// DCS. Also remove C1 and bidi controls; preserve only useful text whitespace.
pub fn sanitize(text: &str) -> String {
    let mut result = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' || matches!(c, '\u{009b}' | '\u{009d}' | '\u{0090}') {
            let kind = if c == '\x1b' {
                chars.next()
            } else {
                Some(match c {
                    '\u{009b}' => '[',
                    '\u{009d}' => ']',
                    _ => 'P',
                })
            };
            match kind {
                Some('[') => {
                    for x in chars.by_ref() {
                        if ('@'..='~').contains(&x) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    while let Some(x) = chars.next() {
                        if x == '\x07' || x == '\u{009c}' {
                            break;
                        }
                        if x == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if c == '\n'
            || c == '\t'
            || (!c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        {
            result.push(c);
        }
    }
    result
}

pub fn label(run: &RunRecord, event: Option<&EventRecord>) -> &'static str {
    let o = &run.outcome;
    if run.mode == RunMode::Attached {
        if run
            .attachment
            .as_ref()
            .is_some_and(|attachment| attachment.owner_state == OwnerState::Gone)
            && o.lifecycle != LifecycleState::Finished
        {
            return "Attached agent's owner is gone; finish or reject";
        }
        if o.lifecycle == LifecycleState::Working {
            return "Attached agent working";
        }
    }
    if o.application == ApplicationState::BlockedBySourceDrift {
        return "Application blocked: source changed";
    }
    if o.application == ApplicationState::Failed {
        return "Application failed";
    }
    if o.application == ApplicationState::Applied && o.applied_by == Some(AppliedBy::AutoApply) {
        return match o.review {
            ReviewState::Accepted => "Auto-applied; reviewed: accepted",
            ReviewState::Rejected => "Auto-applied; reviewed: rejected",
            _ => "Auto-applied; review pending",
        };
    }
    if o.application == ApplicationState::Applied {
        return "Accepted and applied";
    }
    if o.review == ReviewState::Rejected {
        return "Rejected";
    }
    if o.review == ReviewState::Accepted {
        return "Accepted; not applied";
    }
    match o.waiting_on {
        WaitingOn::Human => return "Waiting for you",
        WaitingOn::Capacity => return "Waiting for subscription capacity",
        WaitingOn::Admission => return "Waiting for shared admission",
        WaitingOn::Reconciliation => return "Waiting for process reconciliation",
        WaitingOn::Authorization => return "Waiting for authorization",
        _ => {}
    }
    match o.work_result {
        WorkResult::Interrupted => return "Interrupted",
        WorkResult::Cancelled => return "Cancelled",
        WorkResult::Failed => return "Failed",
        WorkResult::Deferred => return "Deferred",
        WorkResult::Ready => return "Ready for review",
        _ => {}
    }
    if event.is_some_and(|e| e.event_type == "recovery.selected") {
        return "Recovering once after failed verification";
    }
    match o.phase {
        RunPhase::Verifying => "Verifying",
        RunPhase::Executing => "Working",
        RunPhase::Preparing => "Preparing",
        RunPhase::Planning => "Planning",
        RunPhase::Integrating => "Integrating",
        RunPhase::Reviewing => "Ready for review",
        RunPhase::Applying => "Applying",
        RunPhase::Finished => "Finished",
    }
}

fn model_name(model: &str) -> &str {
    // Display aliases only; exact identities remain in Details and machine output.
    match model {
        "gpt-5.6-luna" => "Luna",
        "gpt-5.6-terra" => "Terra",
        "gpt-5.6-sol" => "Sol",
        "gpt-6-astra" => "Astra",
        _ => model,
    }
}

fn verification_text(state: VerificationState) -> &'static str {
    match state {
        VerificationState::NotConfigured => "Unverified — no checks configured",
        VerificationState::NotRun => "Verification has not run",
        VerificationState::Passed => "Verification passed",
        VerificationState::Failed => "Verification failed",
        VerificationState::Inconclusive => "Verification inconclusive",
    }
}

/// The one-line coherence verdict shown under the verification line.
pub fn coherence_text(validity: &Validity) -> String {
    match validity.decision {
        Decision::Continue if !validity.world_changed => {
            "Coherence: CONTINUE — world unchanged".into()
        }
        Decision::Continue => format!(
            "Coherence: CONTINUE — {} file{} changed underneath, none affect this work",
            validity.changed_files,
            if validity.changed_files == 1 { "" } else { "s" }
        ),
        Decision::Refresh => format!(
            "Coherence: REFRESH — {}",
            validity
                .reasons
                .first()
                .map_or("files changed underneath the work", |reason| reason
                    .detail
                    .as_str())
        ),
        Decision::Stop => "Coherence: STOP — this patch is already in the source".into(),
    }
}

pub fn projection(run: &RunRecord, event: Option<&EventRecord>, width: u16, ascii: bool) -> String {
    let (done, pending, failed, skip, active, inactive) = if ascii {
        ("*", "o", "x", "-", " == ", " -- ")
    } else {
        ("●", "○", "×", "–", " ━━ ", " ── ")
    };
    let model = if run.mode == RunMode::Attached {
        run.attachment
            .as_ref()
            .and_then(|attachment| attachment.agent.as_deref())
            .unwrap_or("external")
    } else {
        run.attempts
            .last()
            .and_then(|a| a.resolved_model.as_deref())
            .or_else(|| {
                run.allocation
                    .as_ref()
                    .map(|d| d.selected.resolved_model.as_str())
            })
            .map(model_name)
            .unwrap_or("work")
    };
    let work_mark = if run.attempts.is_empty() {
        pending
    } else {
        done
    };
    let verify_mark = match run.outcome.verification {
        VerificationState::Passed => done,
        VerificationState::Failed | VerificationState::Inconclusive => failed,
        VerificationState::NotConfigured => skip,
        VerificationState::NotRun => pending,
    };
    let review_mark = match run.outcome.review {
        ReviewState::Accepted | ReviewState::Pending
            if run.outcome.work_result == WorkResult::Ready =>
        {
            done
        }
        ReviewState::Rejected => failed,
        _ => pending,
    };
    let work_edge = if run.attempts.is_empty() {
        inactive
    } else {
        active
    };
    let verify_edge = if run.outcome.phase == RunPhase::Verifying
        || matches!(
            run.outcome.verification,
            VerificationState::Passed | VerificationState::Failed | VerificationState::Inconclusive
        ) {
        active
    } else {
        inactive
    };
    let review_edge = if run.outcome.work_result == WorkResult::Ready {
        active
    } else {
        inactive
    };
    let graph = if width >= 68 {
        format!(
            "{done} goal{work_edge}{work_mark} {model}{verify_edge}{verify_mark} verify{review_edge}{review_mark} review"
        )
    } else {
        format!(
            "{done} goal{work_edge}{work_mark} {model}\n{verify_mark} verify{review_edge}{review_mark} review"
        )
    };
    let mut lines = vec![
        goal_heading(&run.task, width),
        String::new(),
        label(run, event).to_owned(),
    ];
    {
        lines.push(if width < 40 {
            format!("{work_mark} {model}\n{verify_mark} checks  {review_mark} review")
        } else {
            graph
        });
    }
    if run.outcome.phase == RunPhase::Executing
        && run.outcome.work_result == WorkResult::Pending
        && run.outcome.waiting_on == WaitingOn::None
    {
        lines.push("Agent running · tool activity is in private logs".into());
        if let Some(decision) = &run.allocation {
            lines.push(format!(
                "Choice: {}",
                clip(&decision.reason, width.saturating_sub(8))
            ));
        }
    }
    // One attempt is already represented by the work node. Show branches only
    // when the committed history contains recovery or clarification continuation.
    if run.attempts.len() > 1 {
        for attempt in &run.attempts {
            let prefix = match attempt.detail.reason.as_deref() {
                Some("target_verification_failure") => "Recovery · ",
                Some(_) => "Continuation · ",
                None => "",
            };
            let status = if attempt.detail.failure == Some(FailureKind::TargetVerification) {
                "verification failed"
            } else {
                match attempt.outcome.as_str() {
                    "preparing" => "preparing",
                    "running" => "working",
                    "completed" => "completed",
                    "failed" => "failed",
                    "cancelled" => "cancelled",
                    "interrupted" => "interrupted",
                    "timed_out" => "timed out",
                    "not_launched" => "not started",
                    _ => "state unavailable",
                }
            };
            let branch = if ascii {
                "+-"
            } else if attempt.ordinal == 1 {
                "├─"
            } else {
                "╰━"
            };
            lines.push(format!(
                "{branch} {prefix}Attempt {} · {}: {status}",
                attempt.ordinal,
                attempt
                    .resolved_model
                    .as_deref()
                    .map(model_name)
                    .unwrap_or("model unknown")
            ));
        }
    }
    if run.outcome.phase == RunPhase::Verifying || run.outcome.work_result != WorkResult::Pending {
        lines.push(verification_text(run.outcome.verification).to_owned());
    }
    if run.outcome.work_result == WorkResult::Ready
        && let Some(validity) = run.coherence.as_ref().and_then(|c| c.validity.as_ref())
    {
        lines.push(clip(&coherence_text(validity), width));
    }
    if run.outcome.work_result == WorkResult::Ready
        && let [candidate] = run.candidates.as_slice()
    {
        let diff = &candidate.diff_stats;
        lines.push(format!(
            "{} file{} changed · +{} / -{} lines",
            diff.files_changed,
            if diff.files_changed == 1 { "" } else { "s" },
            diff.lines_added,
            diff.lines_removed
        ));
    }
    if let Some(q) = pending_question(run) {
        lines.push(
            "Decision needed · answering continues this goal within its remaining limit.".into(),
        );
        lines.push(q.report.question.clone());
        if !q.report.choices.is_empty() {
            lines.push(q.report.choices.join(" / "));
        }
    }
    if let Some(failure) = run.execution.as_ref().and_then(|p| p.failure) {
        let reason = match failure {
            FailureKind::InvocationLimit => {
                "The two-invocation limit leaves no continuation budget."
            }
            FailureKind::Deadline => "The shared deadline expired; no continuation time remains.",
            FailureKind::Authorization => {
                "Current subscription authorization does not permit execution."
            }
            FailureKind::TargetVerification => "Configured verification failed.",
            FailureKind::VerificationInfrastructure => {
                "Verification infrastructure failed; automatic recovery is not permitted."
            }
            FailureKind::VerificationUnknown => {
                "Verification failed for an unknown reason; automatic recovery is not permitted."
            }
            FailureKind::Cancelled => "Execution was cancelled.",
            _ => "Work stopped; inspect Details for the recorded failure.",
        };
        lines.push(reason.into());
    }
    if let Some(reason) = event
        .and_then(|e| e.payload.get("reason").or_else(|| e.payload.get("error")))
        .and_then(serde_json::Value::as_str)
    {
        lines.push(reason.to_owned());
    }
    let text = sanitize(&lines.join("\n"));
    if ascii {
        text.replace('→', "->").replace(['—', '·'], "-")
    } else {
        text
    }
}

pub fn pending_question(run: &RunRecord) -> Option<&Clarification> {
    run.execution
        .as_ref()?
        .questions
        .last()
        .filter(|q| q.state == QuestionState::Pending)
}
fn question_command(run: &RunRecord) -> Option<QuestionCommand> {
    pending_question(run).map(|q| QuestionCommand {
        run_id: run.id.clone(),
        question_id: q.id.clone(),
        revision: q.revision,
        generation: q.generation,
    })
}
pub fn review_command(run: &RunRecord) -> Result<ReviewCommand> {
    let [candidate] = run.candidates.as_slice() else {
        anyhow::bail!("delivery has no single candidate");
    };
    Ok(ReviewCommand {
        run_id: run.id.clone(),
        candidate_id: candidate.id.clone(),
        revision: run.state_revision,
    })
}

fn launch_accounting(state: &State, run: &RunRecord) -> String {
    let maximum = run.execution.as_ref().map_or(2, |p| p.max_invocations);
    let counts = (|| -> Result<(u32, u32)> {
        let db = crate::db::Database::open_read_only(state.db_path())?;
        db.connection().busy_timeout(Duration::from_millis(10))?;
        let launches = crate::launch::launches_for_run(&db, &run.id)?;
        let known = launches.iter().filter(|l| l.child.is_some()).count();
        let uncertain = launches
            .iter()
            .filter(|l| {
                matches!(
                    l.state,
                    crate::launch::LaunchState::Intent | crate::launch::LaunchState::Uncertain
                )
            })
            .count();
        Ok((u32::try_from(known)?, u32::try_from(uncertain)?))
    })();
    match counts {
        Ok((known, uncertain)) => {
            format!("Launches {known} recorded · {uncertain} uncertain · limit {maximum}")
        }
        Err(_) => format!("Launch count unavailable · limit {maximum}"),
    }
}

fn details(run: &RunRecord) -> String {
    let mut text = format!(
        "Goal: {}\nRun: {}\nState revision: {}",
        run.task, run.id, run.state_revision
    );
    if let Some(decision) = &run.allocation {
        text.push_str(&format!("\nProfile choice: {}", decision.reason));
    }
    for attempt in &run.attempts {
        text.push_str(&format!(
            "\nAttempt {}: {} · {}",
            attempt.ordinal,
            attempt.resolved_model.as_deref().unwrap_or("unknown model"),
            attempt.outcome
        ));
    }
    for c in &run.candidates {
        text.push_str(&format!(
            "\nCandidate: {}\nPatch: {}\nOutput: {}\nErrors: {}",
            c.id,
            c.diff_path.display(),
            c.stdout_path.display(),
            c.stderr_path.display()
        ));
    }
    text
}

const INPUT_LIMIT: usize = 16 * 1024;

/// How to use Dispatch on work it does not launch: no resource setup needed.
const OBSERVE_HELP: &str = "Protect work you run yourself · no setup needed\n  dispatch attach -- <agent command>         run your agent in its own worktree under Dispatch\n  dispatch attach --workspace <dir>           observe an agent already working there\n  dispatch finish <run>                       freeze its result, verify it, make it reviewable\n  dispatch serve                              watch this project in the foreground\n  dispatch check / accept / reject            judge and apply a result against the source now";
/// One row of a selection menu. A disabled row is shown with its reason and
/// cannot be chosen.
struct Choice {
    label: String,
    disabled: Option<String>,
}
impl Choice {
    fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            disabled: None,
        }
    }
    fn disabled(label: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            disabled: Some(reason.into()),
        }
    }
    fn row(&self) -> String {
        match &self.disabled {
            None => self.label.clone(),
            Some(reason) => format!("{} — {reason}", self.label),
        }
    }
}

#[derive(Debug, PartialEq)]
enum Input {
    Submit(String),
    Cancel,
    Eof,
    Changed,
    /// Shift+Tab. Never submits or edits the draft; `input_prompt` flips the
    /// mode itself and never returns this to a caller.
    ToggleAutoApply,
}
struct Editor {
    text: TextArea<'static>,
}
impl Editor {
    fn new() -> Self {
        let mut text = TextArea::default();
        text.set_cursor_line_style(Style::default());
        text.set_max_histories(32);
        Self { text }
    }
    fn event(&mut self, event: Event) -> Input {
        match event {
            Event::Paste(value) => {
                let value = sanitize(&value.replace("\r\n", "\n").replace('\r', "\n"));
                if self.text.lines().iter().map(String::len).sum::<usize>() + value.len()
                    <= INPUT_LIMIT
                {
                    self.text.insert_str(value);
                }
            }
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                // Intercepted before the widget sees it: ratatui-textarea turns
                // BackTab into a Tab insert otherwise.
                if k.code == KeyCode::BackTab {
                    return Input::ToggleAutoApply;
                }
                if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
                    return Input::Cancel;
                }
                if k.modifiers.contains(KeyModifiers::CONTROL)
                    && k.code == KeyCode::Char('d')
                    && self.text.lines().iter().all(String::is_empty)
                {
                    return Input::Eof;
                }
                if k.code == KeyCode::Enter
                    && !k
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
                {
                    let value = self.text.lines().join("\n");
                    self.text = Self::new().text;
                    return Input::Submit(value);
                }
                if let KeyCode::Char(c) = k.code
                    && sanitize(&c.to_string()) != c.to_string()
                {
                    return Input::Changed;
                }
                if k.code == KeyCode::Enter {
                    self.text.insert_newline();
                } else if self.text.lines().iter().map(String::len).sum::<usize>() < INPUT_LIMIT
                    || !matches!(k.code, KeyCode::Char(_))
                {
                    self.text.input(k);
                }
            }
            _ => {}
        }
        Input::Changed
    }
}

type NativeTerminal = Terminal<CrosstermBackend<io::Stdout>>;
fn activity_hint(elapsed: Duration, ascii: bool) -> String {
    let frames = if ascii {
        ["|", "/", "-", "\\"]
    } else {
        ["◐", "◓", "◑", "◒"]
    };
    let frame = if std::env::var_os("DISPATCH_REDUCED_MOTION").is_some() {
        "."
    } else {
        frames[(elapsed.as_millis() / 200 % 4) as usize]
    };
    format!("{frame} {}s elapsed · Ctrl+C cancel", elapsed.as_secs())
}

fn content_area(area: Rect) -> Rect {
    let margin = if area.width > 4 { 2 } else { 0 };
    Rect::new(
        area.x + margin,
        area.y,
        area.width
            .saturating_sub(if margin > 0 { margin + 1 } else { 0 }),
        area.height,
    )
}

fn clip(text: &str, width: u16) -> String {
    let text = sanitize(text).replace(['\n', '\t'], " ");
    if UnicodeWidthStr::width(text.as_str()) <= usize::from(width) {
        return text;
    }
    let mut result = String::new();
    let mut cells = 0;
    for cluster in text.graphemes(true) {
        let n = UnicodeWidthStr::width(cluster);
        if cells + n + 3 > usize::from(width) {
            break;
        }
        cells += n;
        result.push_str(cluster);
    }
    result.push_str(&"..."[..usize::from(width).min(3)]);
    result
}
fn goal_heading(task: &str, width: u16) -> String {
    format!("Goal  {}", clip(task, width.saturating_sub(6)))
}
/// The auto-apply mode indicator, always the hint row's first segment. Pure
/// so both charsets and modes are unit-testable without a `Ui`.
fn mode_hint_text(ascii: bool, auto_apply: bool) -> String {
    if ascii {
        if auto_apply {
            ">> AUTO-APPLY ON - Shift+Tab to pause".into()
        } else {
            "[review before apply] Shift+Tab".into()
        }
    } else if auto_apply {
        "⏵⏵ auto-apply on · Shift+Tab to pause".into()
    } else {
        "⏸ review before apply · Shift+Tab".into()
    }
}
/// The goal heading with the auto-apply marker prepended when the mode is on,
/// so a scrollback entry for a submitted goal shows the mode it ran under.
fn goal_heading_marked(task: &str, width: u16, ascii: bool, auto_apply: bool) -> String {
    let marker = if !auto_apply {
        ""
    } else if ascii {
        ">> "
    } else {
        "⏵⏵ "
    };
    format!("{marker}{}", goal_heading(task, width))
}

fn styled_body<'a>(body: &'a str, palette: &Theme) -> Text<'a> {
    Text::from(
        body.lines()
            .map(|line| {
                if line.ends_with("  DISPATCH")
                    && (line.starts_with("  ┌") || line.starts_with("  +"))
                {
                    let (mark, wordmark) = line.rsplit_once("  ").unwrap();
                    return Line::from(vec![
                        Span::styled(mark, palette.foreground),
                        Span::styled(format!("  {wordmark}"), palette.foreground.bold()),
                    ]);
                }
                if line.starts_with("━━") || line.starts_with("==") {
                    let (cells, style) = if line.starts_with("━━━") || line.starts_with("===")
                    {
                        (3, palette.success)
                    } else if line.starts_with("━━└") || line.starts_with("==+") {
                        (2, palette.warning)
                    } else {
                        (2, palette.accent)
                    };
                    let bar = line
                        .char_indices()
                        .nth(cells)
                        .map_or(line.len(), |(i, _)| i);
                    let split = line.char_indices().nth(6).map_or(line.len(), |(i, _)| i);
                    return Line::from(vec![
                        Span::styled(&line[..bar], style),
                        Span::styled(&line[bar..split], palette.foreground),
                        Span::styled(&line[split..], palette.secondary),
                    ]);
                }
                if line.starts_with("● goal") || line.starts_with("* goal") {
                    return Line::from(
                        line.chars()
                            .map(|c| {
                                Span::styled(
                                    c.to_string(),
                                    if matches!(c, '●' | '━' | '*' | '=') {
                                        palette.accent
                                    } else {
                                        palette.inactive
                                    },
                                )
                            })
                            .collect::<Vec<_>>(),
                    );
                }
                let style = if line.starts_with("Goal  ") {
                    palette.foreground.bold()
                } else if line.starts_with("Unverified")
                    || line.starts_with("Waiting")
                    || line.starts_with("Application blocked")
                {
                    palette.warning
                } else if line == "Verification passed" {
                    palette.success
                } else if line.contains("failed") || line == "Failed" || line == "Interrupted" {
                    palette.error
                } else if line == "Ready for review"
                    || line == "Working"
                    || line == "Preparing"
                    || line == "Verifying"
                {
                    palette.accent
                } else if line.contains("○") || line.contains(" == ") || line.contains(" ━━ ")
                {
                    palette.inactive
                } else {
                    palette.foreground
                };
                Line::styled(line, style)
            })
            .collect::<Vec<_>>(),
    )
}

fn draw_view(
    frame: &mut ratatui::Frame,
    body: &str,
    editor: Option<&Editor>,
    hint: &str,
    palette: &Theme,
    ascii: bool,
    auto_apply: bool,
) {
    let area = content_area(frame.area());
    let input_height = editor.map_or(0, |e| {
        (e.text.lines().len().clamp(1, 3) as u16).min(area.height.saturating_sub(1))
    });
    let hints = Paragraph::new(hint)
        .style(if auto_apply {
            palette.warning
        } else {
            palette.secondary
        })
        .wrap(Wrap { trim: false });
    let hint_height = (hints.line_count(area.width) as u16)
        .min(3)
        .min(area.height.saturating_sub(input_height));
    let paragraph = Paragraph::new(styled_body(body, palette)).wrap(Wrap { trim: false });
    let body_height = paragraph
        .line_count(area.width)
        .min(area.height.saturating_sub(input_height + hint_height) as usize)
        as u16;
    frame.render_widget(
        paragraph,
        Rect::new(area.x, area.y, area.width, body_height),
    );
    if let Some(editor) = editor {
        frame.render_widget(
            Paragraph::new(if ascii { ">" } else { "›" }).style(palette.focus),
            Rect::new(
                area.x,
                area.y + body_height,
                area.width.min(2),
                input_height,
            ),
        );
        frame.render_widget(
            &editor.text,
            Rect::new(
                area.x + 2,
                area.y + body_height,
                area.width.saturating_sub(2),
                input_height,
            ),
        );
    }
    frame.render_widget(
        hints,
        Rect::new(
            area.x,
            area.y + body_height + input_height,
            area.width,
            area.height
                .saturating_sub(body_height + input_height)
                .min(hint_height),
        ),
    );
}

/// Text emphasis only, not a semantic diff engine. Keep +/- markers even without color.
fn diff_lines<'a>(text: &'a str, palette: &Theme) -> Vec<Line<'a>> {
    let lines: Vec<_> = text.lines().collect();
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let style = if line.starts_with('+') {
                palette.addition
            } else if line.starts_with('-') {
                palette.deletion
            } else if line.starts_with("@@") {
                palette.secondary
            } else {
                palette.foreground
            };
            let other = if line.starts_with('-')
                && !line.starts_with("--- ")
                && !i
                    .checked_sub(1)
                    .and_then(|j| lines.get(j))
                    .is_some_and(|l| l.starts_with('-'))
                && !lines.get(i + 2).is_some_and(|l| l.starts_with('+'))
            {
                lines.get(i + 1).filter(|l| l.starts_with('+'))
            } else if line.starts_with('+')
                && !line.starts_with("+++ ")
                && !lines.get(i + 1).is_some_and(|l| l.starts_with('+'))
                && !i
                    .checked_sub(2)
                    .and_then(|j| lines.get(j))
                    .is_some_and(|l| l.starts_with('-'))
            {
                i.checked_sub(1)
                    .and_then(|j| lines.get(j))
                    .filter(|l| l.starts_with('-'))
            } else {
                None
            };
            if let Some(other) = other {
                let a: Vec<_> = line[1..].graphemes(true).collect();
                let b: Vec<_> = other[1..].graphemes(true).collect();
                let prefix = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
                let suffix = a[prefix..]
                    .iter()
                    .rev()
                    .zip(b[prefix..].iter().rev())
                    .take_while(|(a, b)| a == b)
                    .count();
                let start = 1 + a[..prefix].iter().map(|g| g.len()).sum::<usize>();
                let end = line.len() - a[a.len() - suffix..].iter().map(|g| g.len()).sum::<usize>();
                Line::from(vec![
                    Span::styled(&line[..start], style),
                    Span::styled(&line[start..end], style.bold().underlined()),
                    Span::styled(&line[end..], style),
                ])
            } else {
                Line::styled(*line, style)
            }
        })
        .collect()
}

struct Screen {
    terminal: NativeTerminal,
    inspection: bool,
}
static ALTERNATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
fn restore() {
    if ALTERNATE.swap(false, std::sync::atomic::Ordering::SeqCst) {
        let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen);
    }
    let _ = execute!(
        io::stdout(),
        event::DisableBracketedPaste,
        crossterm::cursor::Show,
        crossterm::style::ResetColor
    );
    let _ = terminal::disable_raw_mode();
}
impl Screen {
    fn new() -> Result<Self> {
        Self::open(false)
    }
    fn open(inspection: bool) -> Result<Self> {
        // One hook across repeated reviewer handoffs; never accumulate closures.
        static HOOK: std::sync::Once = std::sync::Once::new();
        HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                restore();
                previous(info);
            }));
        });
        terminal::enable_raw_mode()?;
        let result = (|| {
            execute!(io::stdout(), event::EnableBracketedPaste)?;
            let (_, height) = terminal::size()?;
            if inspection {
                ALTERNATE.store(true, std::sync::atomic::Ordering::SeqCst);
                execute!(io::stdout(), terminal::EnterAlternateScreen)?;
            }
            let terminal = Terminal::with_options(
                CrosstermBackend::new(io::stdout()),
                TerminalOptions {
                    viewport: if inspection {
                        Viewport::Fullscreen
                    } else {
                        Viewport::Inline(height.clamp(1, 20))
                    },
                },
            )?;
            Ok(Self {
                terminal,
                inspection,
            })
        })();
        if result.is_err() {
            restore();
        }
        result
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        // The alternate viewport is discarded on return. In particular, the
        // panic hook may already have returned to the primary screen: clearing
        // this Fullscreen viewport then would erase the user's terminal display.
        if !self.inspection {
            let _ = self.terminal.clear();
        }
        restore();
    }
}

struct Ui {
    screen: Option<Screen>,
    #[cfg(not(unix))]
    plain_input: Option<tokio::sync::mpsc::Receiver<io::Result<String>>>,
    options: Options,
    palette: Theme,
    closed: bool,
    draft: String,
    launch_line: String,
    render_key: String,
    /// Session-scoped only: off at every start, never persisted. See
    /// `docs/plan-0.3-auto-apply-and-attach.md` part 5.5.
    auto_apply: bool,
    /// Whether this session has an auto-apply mode at all: the goal session
    /// does; `watch` does not, so it shows no mode and ignores Shift+Tab.
    auto_apply_mode: bool,
    reviewed: Option<RunRecord>,
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hup: tokio::signal::unix::Signal,
}
impl Ui {
    fn new(options: Options) -> Result<Self> {
        #[cfg(unix)]
        let screen = if options.plain {
            None
        } else {
            Some(Screen::new()?)
        };
        #[cfg(not(unix))]
        let (screen, plain_input) = if options.plain {
            use std::io::{BufRead, Read};
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            std::thread::spawn(move || {
                loop {
                    let mut line = String::new();
                    let result = io::stdin()
                        .lock()
                        .take((INPUT_LIMIT + 1) as u64)
                        .read_line(&mut line);
                    if line.len() > INPUT_LIMIT {
                        let _ = tx.blocking_send(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "input exceeds 16 KiB",
                        )));
                        break;
                    }
                    if matches!(result, Ok(0)) {
                        break;
                    }
                    let value = result.map(|_| line.trim_end_matches(['\r', '\n']).to_owned());
                    if tx.blocking_send(value).is_err() {
                        break;
                    }
                }
            });
            (None, Some(rx))
        } else {
            (Some(Screen::new()?), None)
        };
        Ok(Self {
            screen,
            #[cfg(not(unix))]
            plain_input,
            options,
            palette: Theme::from_env(options.no_color),
            closed: false,
            draft: String::new(),
            launch_line: String::new(),
            render_key: String::new(),
            auto_apply: false,
            auto_apply_mode: true,
            reviewed: None,
            #[cfg(unix)]
            term: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            #[cfg(unix)]
            hup: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?,
        })
    }
    fn width(&self) -> u16 {
        terminal::size().map(|s| s.0).unwrap_or(80)
    }
    /// Session-scoped auto-apply indicator, always the hint row's first
    /// segment. See `docs/plan-0.3-auto-apply-and-attach.md` part 5.5.
    fn mode_hint(&self) -> String {
        if !self.auto_apply_mode {
            return String::new();
        }
        mode_hint_text(self.options.ascii, self.auto_apply)
    }
    fn hint_with_mode(&self, hint: &str) -> String {
        let mode = self.mode_hint();
        if mode.is_empty() {
            hint.to_owned()
        } else if hint.is_empty() {
            mode
        } else if self.options.ascii {
            format!("{mode} - {hint}")
        } else {
            format!("{mode} · {hint}")
        }
    }
    fn draw(&mut self, body: &str, editor: Option<&Editor>, hint: &str) -> Result<()> {
        let body = self.display_text(body);
        let hint = self.display_text(&self.hint_with_mode(hint));
        let key = format!(
            "{body}\0{hint}\0{:?}\0{:?}",
            terminal::size(),
            editor.map(|e| (e.text.lines(), e.text.cursor()))
        );
        if key == self.render_key {
            return Ok(());
        }
        self.render_key = key;
        let Some(screen) = &mut self.screen else {
            return Ok(());
        };
        screen.terminal.draw(|frame| {
            draw_view(
                frame,
                &body,
                editor,
                &hint,
                &self.palette,
                self.options.ascii,
                self.auto_apply && self.auto_apply_mode,
            );
        })?;
        Ok(())
    }
    /// Draw a frame the caller builds, coalesced like `draw`: nothing is
    /// drawn while `key` (the caller's content) and the terminal size stay
    /// the same.
    fn draw_frame(
        &mut self,
        key: &str,
        render: impl FnOnce(&mut ratatui::Frame, &Theme),
    ) -> Result<()> {
        let key = format!("{key}\0{:?}", terminal::size());
        if key == self.render_key {
            return Ok(());
        }
        self.render_key = key;
        let Some(screen) = &mut self.screen else {
            return Ok(());
        };
        screen.terminal.draw(|frame| render(frame, &self.palette))?;
        Ok(())
    }
    fn display_text(&self, text: &str) -> String {
        let text = sanitize(text);
        if self.options.ascii {
            text.replace('…', "...")
                .replace('·', "-")
                .replace('→', "->")
                .replace('—', "-")
        } else {
            text
        }
    }
    fn commit(&mut self, text: &str) -> Result<()> {
        self.commit_text(text, false)
    }
    fn commit_text(&mut self, text: &str, diff: bool) -> Result<()> {
        self.render_key.clear();
        let text = self.display_text(text);
        if let Some(screen) = &mut self.screen {
            // Bounded blocks enter ordinary terminal scrollback, never alternate screen.
            let lines = if diff {
                diff_lines(&text, &self.palette)
            } else {
                styled_body(&text, &self.palette).lines
            };
            for chunk in lines.chunks(32) {
                let paragraph = Paragraph::new(Text::from(
                    chunk
                        .iter()
                        .map(|line| {
                            let mut padded = Line::from("  ");
                            padded.spans.extend(line.spans.clone());
                            padded.style = line.style;
                            padded
                        })
                        .collect::<Vec<_>>(),
                ))
                .wrap(Wrap { trim: false });
                let height = paragraph
                    .line_count(screen.terminal.size()?.width.max(1))
                    .min(u16::MAX as usize) as u16;
                screen.terminal.insert_before(height, |buffer| {
                    use ratatui::widgets::Widget;
                    paragraph.render(
                        Rect::new(
                            buffer.area.x,
                            buffer.area.y,
                            buffer.area.width,
                            buffer.area.height,
                        ),
                        buffer,
                    );
                })?;
            }
        } else {
            // Plain mode preserves the patch verbatim (apart from terminal sanitization).
            println!("{text}");
        }
        Ok(())
    }
    async fn next(&mut self) -> Result<Option<Event>> {
        #[cfg(unix)]
        tokio::select! {
            _ = self.term.recv() => return Ok(None),
            _ = self.hup.recv() => return Ok(None),
            _ = tokio::signal::ctrl_c() => return Ok(Some(Event::Key(event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)))),
            _ = tokio::time::sleep(Duration::from_millis(30)) => {}
        }
        #[cfg(not(unix))]
        tokio::time::sleep(Duration::from_millis(30)).await;
        #[cfg(unix)]
        if self.options.plain {
            // Cooked terminal reads are line-ready; no background input reader
            // can prefetch a review action or compete with an external tool.
            let mut fd = libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe {
                libc::poll(&mut fd, 1, 0);
            }
            if fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Ok(None);
            }
            if fd.revents & libc::POLLIN == 0 {
                return Ok(Some(Event::FocusGained));
            }
            let mut bytes = vec![0u8; INPUT_LIMIT + 1];
            let size = unsafe { libc::read(0, bytes.as_mut_ptr().cast(), bytes.len()) };
            if size == 0 {
                return Ok(None);
            }
            if size < 0 {
                return Err(io::Error::last_os_error().into());
            }
            anyhow::ensure!(size as usize <= INPUT_LIMIT, "input exceeds 16 KiB");
            return Ok(Some(Event::Paste(
                String::from_utf8_lossy(&bytes[..size as usize])
                    .trim_end_matches(['\r', '\n'])
                    .to_owned(),
            )));
        }
        #[cfg(not(unix))]
        if let Some(rx) = &mut self.plain_input {
            return match rx.try_recv() {
                Ok(line) => Ok(Some(Event::Paste(line?))),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => Ok(None),
                Err(_) => Ok(Some(Event::FocusGained)),
            };
        }
        #[cfg(unix)]
        {
            let mut fd = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: 0,
                revents: 0,
            };
            // Only inspect hangup; Crossterm remains the single input reader.
            unsafe {
                libc::poll(&mut fd, 1, 0);
            }
            if fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Ok(None);
            }
        }
        if event::poll(Duration::ZERO)? {
            Ok(Some(event::read()?))
        } else {
            Ok(Some(Event::FocusGained))
        }
    }
    async fn prompt(&mut self, body: &str) -> Result<Input> {
        self.input_prompt(body, true).await
    }
    async fn command_prompt(&mut self, body: &str) -> Result<Input> {
        self.input_prompt(body, false).await
    }
    /// Choose one of `choices`: ↑/↓ or j/k move, a digit moves to that row,
    /// Enter chooses the focused row, Esc or Ctrl+C returns `None`. Focus
    /// starts on `initial`, so Enter alone always chooses it. Plain mode
    /// prints a numbered list and reads a number; an empty line chooses
    /// `initial`. A disabled row shows its reason and cannot be chosen.
    async fn select(
        &mut self,
        body: &str,
        choices: &[Choice],
        initial: usize,
    ) -> Result<Option<usize>> {
        if self.closed || choices.is_empty() {
            return Ok(None);
        }
        let mut focus = initial.min(choices.len() - 1);
        if self.screen.is_none() {
            let rows = choices
                .iter()
                .enumerate()
                .map(|(i, choice)| format!("{}) {}", i + 1, choice.row()))
                .collect::<Vec<_>>()
                .join("\n");
            self.commit(&format!("{body}\n{rows}"))?;
            loop {
                print!("[{}] > ", focus + 1);
                io::stdout().flush()?;
                let line = loop {
                    match self.next().await? {
                        None => {
                            self.closed = true;
                            return Ok(None);
                        }
                        Some(Event::Paste(line)) => break line,
                        Some(_) => {}
                    }
                };
                let chosen = match line.trim() {
                    "" => Some(focus),
                    value => value
                        .parse::<usize>()
                        .ok()
                        .and_then(|n| n.checked_sub(1))
                        .filter(|n| *n < choices.len()),
                };
                match chosen.map(|i| (i, &choices[i].disabled)) {
                    Some((i, None)) => return Ok(Some(i)),
                    Some((i, Some(reason))) => {
                        focus = i;
                        println!("Not available: {reason}");
                    }
                    None => println!("Choose a listed number."),
                }
            }
        }
        loop {
            let marker = if self.options.ascii { ">" } else { "›" };
            let rows = choices
                .iter()
                .enumerate()
                .map(|(i, choice)| {
                    let mark = if i == focus { marker } else { " " };
                    format!("{mark} {}", choice.row())
                })
                .collect::<Vec<_>>()
                .join("\n");
            self.draw(
                &format!("{body}\n{rows}"),
                None,
                "↑↓ move · Enter choose · Esc back",
            )?;
            let Some(event) = self.next().await? else {
                self.closed = true;
                return Ok(None);
            };
            let Event::Key(key) = event else {
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => focus = focus.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => focus = (focus + 1).min(choices.len() - 1),
                KeyCode::Char(c)
                    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(c, 'c' | 'd') =>
                {
                    self.closed |= c == 'd';
                    return Ok(None);
                }
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    if let Some(i) = c
                        .to_digit(10)
                        .and_then(|n| (n as usize).checked_sub(1))
                        .filter(|i| *i < choices.len())
                    {
                        focus = i;
                    }
                }
                KeyCode::Esc => return Ok(None),
                KeyCode::Enter if choices[focus].disabled.is_none() => {
                    self.render_key.clear();
                    return Ok(Some(focus));
                }
                _ => {}
            }
        }
    }
    async fn input_prompt(&mut self, body: &str, compose: bool) -> Result<Input> {
        if self.closed {
            return Ok(Input::Eof);
        }
        let mut editor = Editor::new();
        if body.contains("accomplish?") && !self.draft.is_empty() {
            let draft = std::mem::take(&mut self.draft);
            if self.screen.is_none() {
                self.commit(&format!(
                    "Preserved goal: {draft}\nEnter it again to submit explicitly."
                ))?;
            } else {
                editor.text.insert_str(draft);
            }
        }
        if self.screen.is_none() {
            self.commit(body)?;
            print!("> ");
            io::stdout().flush()?;
        }
        loop {
            self.draw(
                body,
                Some(&editor),
                if compose {
                    "Enter send · Alt+Enter newline · Ctrl+C cancel · Ctrl+D exit"
                } else {
                    "Enter choose · Ctrl+C back · Ctrl+D exit"
                },
            )?;
            let Some(event) = self.next().await? else {
                self.closed = true;
                return Ok(Input::Eof);
            };
            if self.screen.is_none()
                && let Event::Paste(value) = event
            {
                return Ok(Input::Submit(sanitize(&value)));
            }
            match editor.event(event) {
                Input::Changed => {}
                Input::Eof => {
                    self.closed = true;
                    return Ok(Input::Eof);
                }
                // Never submits or edits the draft; redraw shows the new hint.
                Input::ToggleAutoApply if self.auto_apply_mode => {
                    self.auto_apply = !self.auto_apply
                }
                Input::ToggleAutoApply => {}
                input => return Ok(input),
            }
        }
    }
    fn discard_work_input(&mut self) -> Result<()> {
        // Bytes entered while working are never a queued answer or next goal.
        // Keep draining bounded; a flooded input stream closes the session.
        if self.options.plain {
            #[cfg(unix)]
            unsafe {
                libc::tcflush(0, libc::TCIFLUSH);
            }
            #[cfg(not(unix))]
            if let Some(rx) = &mut self.plain_input {
                while rx.try_recv().is_ok() {}
            }
            return Ok(());
        }
        for _ in 0..256 {
            if !self.closed && event::poll(Duration::ZERO)? {
                let _ = event::read()?;
            } else {
                return Ok(());
            }
        }
        self.closed = true;
        anyhow::bail!("input overflow while working; session closed")
    }
    async fn work<F: Future<Output = Result<RunRecord>>>(
        &mut self,
        state: &State,
        work: F,
    ) -> Result<RunRecord> {
        let cancellation = CancellationToken::new();
        let (updates, mut rx) = tokio::sync::watch::channel(None);
        let work = orchestrator::present(
            Presentation {
                updates,
                cancellation: cancellation.clone(),
            },
            work,
        );
        tokio::pin!(work);
        let started = Instant::now();
        let mut body = "Preparing…".to_string();
        let mut latest = None;
        if self.screen.is_none() {
            self.commit(&body)?;
        }
        let mut last_label = String::new();
        let mut input_open = true;
        let mut accounting_at = Instant::now();
        loop {
            if !cancellation.is_cancelled()
                && let Some((event, run)) = &latest
            {
                if accounting_at.elapsed() >= Duration::from_secs(1) {
                    self.launch_line = launch_accounting(state, run);
                    accounting_at = Instant::now();
                }
                body = format!(
                    "{}\n{}",
                    projection(
                        run,
                        Some(event),
                        self.width().saturating_sub(3),
                        self.options.ascii
                    ),
                    self.launch_line
                );
            }
            if let Err(error) = self.draw(
                &body,
                None,
                &activity_hint(started.elapsed(), self.options.ascii),
            ) {
                cancellation.cancel();
                let _ = (&mut work).await;
                return Err(error);
            }
            tokio::select! {
                result = &mut work => {
                    self.discard_work_input()?;
                    let run = result?;
                    self.launch_line = launch_accounting(state,&run);
                    let update = rx.borrow_and_update().clone();
                    let event = update.as_ref()
                        .filter(|(_, committed)| committed.id == run.id && committed.state_revision == run.state_revision)
                        .map(|(event, _)| event);
                    // The review menu owns the ready-delivery summary in both modes.
                    if pending_question(&run).is_some() || run.outcome.work_result != WorkResult::Ready {
                        self.commit(&projection(&run, event, self.width(), self.options.ascii))?;
                    }
                    return Ok(run);
                },
                changed = rx.changed() => {
                    if changed.is_ok() {
                        let update = rx.borrow_and_update().clone();
                        if let Some((event, run)) = update {
                            let current = label(&run, Some(&event));
                            // A committed ready event may arrive before the future returns.
                            // Preserve that event, but let the review menu present it once.
                            if self.screen.is_none() && current != last_label && current != "Ready for review" {
                                if let Err(error) = self.commit(current) {
                                    cancellation.cancel();
                                    let _ = (&mut work).await;
                                    return Err(error);
                                }
                                last_label = current.into();
                            }
                            self.launch_line = launch_accounting(state,&run);
                            latest = Some((event, run));
                        }
                    }
                }
                input = self.next(), if input_open => {
                    match input {
                        Ok(Some(Event::Key(k))) if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) => {
                            cancellation.cancel();
                            body = "Cancelling; waiting for process cleanup…".into();
                        }
                        Ok(Some(Event::Key(k))) if k.code == KeyCode::Char('d') && k.modifiers.contains(KeyModifiers::CONTROL) => {
                            cancellation.cancel();
                            input_open = false;
                            self.closed = true;
                            body = "Closing; waiting for process cleanup…".into();
                        }
                        Ok(None) | Err(_) => {
                            cancellation.cancel();
                            input_open = false;
                            self.closed = true;
                            body = "Terminal closed; waiting for process cleanup…".into();
                        }
                        // Never submits or edits anything; the next redraw
                        // shows the flipped hint while the agent keeps running.
                        Ok(Some(Event::Key(k))) if k.code == KeyCode::BackTab && self.auto_apply_mode => {
                            self.auto_apply = !self.auto_apply;
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    /// A synchronous, not-cancellable operation (auto-apply's integration
    /// checks) that shows a fixed body instead of a live projection.
    /// `block_in_place` does not yield, so there is nothing for Ctrl+C to
    /// interrupt during the call; input queued during it is discarded after.
    async fn work_static<F: Future<Output = Result<RunRecord>>>(
        &mut self,
        body: &str,
        work: F,
    ) -> Result<RunRecord> {
        self.draw(body, None, "")?;
        if self.screen.is_none() {
            self.commit(body)?;
        }
        let result = work.await;
        self.discard_work_input()?;
        result
    }
}

/// What `/auto-apply` asked for. A pure parse, testable without a `Ui`; the
/// caller resolves `Toggle` against the current mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoApplyCommand {
    On,
    Off,
    Toggle,
}
fn parse_auto_apply_command(text: &str) -> Option<AutoApplyCommand> {
    match text.trim().strip_prefix("/auto-apply")?.trim() {
        "" => Some(AutoApplyCommand::Toggle),
        "on" => Some(AutoApplyCommand::On),
        "off" => Some(AutoApplyCommand::Off),
        _ => None,
    }
}
const AUTO_APPLY_ON_LINE: &str = "Auto-apply on: eligible results will be applied without review.";
const AUTO_APPLY_OFF_LINE: &str = "Auto-apply off: results wait for your review.";

pub async fn session(state: &State, mut options: Options) -> Result<()> {
    anyhow::ensure!(
        suitable(),
        "interactive input requires a terminal; use dispatch run \"<task>\""
    );
    options.plain |= std::env::var("TERM").is_ok_and(|v| v == "dumb");
    options.no_color |= std::env::var_os("NO_COLOR").is_some();
    let mut ui = Ui::new(options)?;
    let source = std::env::current_dir()?;
    let project = source.file_name().unwrap_or_default().to_string_lossy();
    if !options.plain {
        ui.commit(&theme::signature(
            &project,
            ui.width().saturating_sub(3),
            options.ascii,
        ))?;
    }
    let context = tokio::time::timeout(Duration::from_millis(200), async {
        let inside = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["rev-parse", "--is-inside-work-tree"])
            .kill_on_drop(true)
            .output()
            .await?;
        if !inside.status.success() {
            return Ok::<_, io::Error>("Plain directory · local source".to_owned());
        }
        let branch = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
            .kill_on_drop(true)
            .output()
            .await?;
        Ok(if branch.status.success() {
            format!(
                "Git checkout · branch {}",
                sanitize(&String::from_utf8_lossy(&branch.stdout)).trim()
            )
        } else {
            "Git checkout · detached HEAD".into()
        })
    })
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or_else(|| "Project context unavailable · local source".into());
    ui.commit(&context)?;
    loop {
        let prompt = if ui.reviewed.is_some() {
            "Next goal: What do you want to accomplish?\n[i] Details"
        } else {
            "What do you want to accomplish?\n/resources accounts · /checks project checks"
        };
        let prompt = if options.plain && ui.auto_apply {
            prompt.replacen("accomplish?", "accomplish? (auto-apply on)", 1)
        } else {
            prompt.to_owned()
        };
        let task = match ui.prompt(&prompt).await? {
            Input::Submit(task) if task.trim() == "/resources" => {
                if let Err(e) = setup::accounts(&mut ui, state, None).await {
                    ui.commit(&format!("Resources: {e}"))?;
                }
                continue;
            }
            Input::Submit(task) if task.trim() == "/checks" => {
                if let Err(e) = setup::checks(&mut ui, &source).await {
                    ui.commit(&format!("Checks unchanged: {e}"))?;
                }
                continue;
            }
            Input::Submit(task) if parse_auto_apply_command(&task).is_some() => {
                let new_state = match parse_auto_apply_command(&task).unwrap() {
                    AutoApplyCommand::On => true,
                    AutoApplyCommand::Off => false,
                    AutoApplyCommand::Toggle => !ui.auto_apply,
                };
                if new_state != ui.auto_apply {
                    ui.auto_apply = new_state;
                    ui.commit(if ui.auto_apply {
                        AUTO_APPLY_ON_LINE
                    } else {
                        AUTO_APPLY_OFF_LINE
                    })?;
                }
                continue;
            }
            Input::Submit(task) if ui.reviewed.is_some() && task.trim() == "i" => {
                let run = ui.reviewed.as_ref().unwrap().clone();
                ui.diagnostics(&run).await?;
                continue;
            }
            Input::Submit(task) if !task.trim().is_empty() => task,
            Input::Eof | Input::Cancel => return Ok(()),
            _ => continue,
        };
        ui.reviewed = None;
        ui.draw(
            &format!(
                "{}\n\nGoal received. Preparing…",
                goal_heading_marked(
                    &task,
                    ui.width().saturating_sub(3),
                    options.ascii,
                    ui.auto_apply
                )
            ),
            None,
            "",
        )?;
        if options.plain {
            ui.commit(&format!("Goal received: {task}"))?;
        }
        let result = run_goal(&mut ui, state, &source, task, options).await;
        if let Err(error) = result {
            ui.commit(&format!(
                "Stopped: {}\n{}",
                clip(&format!("{error:#}"), 400),
                if ui.draft.is_empty() {
                    "Inspect the retained result with dispatch status or history."
                } else {
                    "Goal preserved below; edit or explicitly submit again."
                }
            ))?;
        }
    }
}

async fn run_goal(
    ui: &mut Ui,
    state: &State,
    source: &std::path::Path,
    task: String,
    options: Options,
) -> Result<()> {
    ui.draft = task.clone();
    let resources = crate::config::ResourceConfig::load(&state.root)?;
    if !resources.allocation_enabled || !resources.profiles.iter().any(|p| p.eligibility().is_ok())
    {
        ui.commit(&format!(
            "{}\nNo agent is set up for Dispatch to launch · your goal is preserved.",
            goal_heading(&task, ui.width().saturating_sub(3))
        ))?;
        let choices = [
            Choice::new("Set up an agent for Dispatch to launch"),
            Choice::new("Protect work I run myself"),
            Choice::new("Back to my goal"),
        ];
        match ui
            .select(
                "Dispatch launches an agent only with a set-up resource.",
                &choices,
                0,
            )
            .await?
        {
            Some(0) => setup::accounts(ui, state, None).await?,
            Some(1) => ui.commit(OBSERVE_HELP)?,
            _ => {}
        }
        ui.draft = task;
        ui.commit("Return to your goal. Submit explicitly when ready.")?;
        return Ok(());
    }
    let (config, _) = Config::discover(source, None)?;
    // This is an explicit per-goal host-execution permission, never trust from YAML.
    let local = config.execution.backend == "local";
    if local {
        match ui.prompt(&format!("{}\n\nLocal execution is not sandboxed.\nAgent and checks use your permissions in a separate workspace.\nAllow this goal? [y/N]",goal_heading(&task,ui.width().saturating_sub(3)))).await? {
            Input::Submit(answer) if matches!(answer.to_lowercase().as_str(), "y" | "yes") => {},
            _ => { ui.commit("Local execution was not authorized.")?; return Ok(()); }
        }
    }
    if config.checks.verify.is_empty() {
        if crate::setup::check_choices(source).is_empty() {
            ui.commit("Unverified — no checks configured. Add project checks with /checks or dispatch.yml.")?;
        } else {
            setup::checks(ui, source).await?;
        }
    }
    let request = RunRequest {
        source: source.to_path_buf(),
        task,
        agent: None,
        model: None,
        effort: None,
        config_path: None,
        backend: None,
        timeout_secs: None,
        allow_unsafe_local: local,
        allow_forwarded_env: false,
        output: RunOutputMode::Silent,
        refreshed_from: None,
    };
    ui.draft.clear();
    let mut run = ui
        .work(state, orchestrator::run_dispatch(state, request))
        .await?;
    loop {
        if let Some(command) = question_command(&run) {
            let deadline = run
                .execution
                .as_ref()
                .map(|p| {
                    format!(
                        "Deadline {} · budget does not reset.\n",
                        p.deadline_at.format("%Y-%m-%d %H:%M:%S UTC")
                    )
                })
                .unwrap_or_default();
            let prompt = format!(
                "{}\n{deadline}Your answer (Ctrl+C cancels this goal)",
                ui.launch_line
            );
            let answer = ui.prompt(&prompt).await?;
            match answer {
                Input::Submit(answer) if !answer.trim().is_empty() => {
                    run = ui
                        .work(
                            state,
                            orchestrator::answer_question(
                                state,
                                command,
                                answer,
                                RunOutputMode::Silent,
                            ),
                        )
                        .await?;
                    continue;
                }
                Input::Submit(_) => continue,
                _ => {
                    run = orchestrator::cancel_question(state, command, RunOutputMode::Silent)?;
                    ui.commit(&projection(&run, None, ui.width(), options.ascii))?;
                    break;
                }
            }
        }
        if run.outcome.work_result != WorkResult::Ready
            || run.outcome.review != ReviewState::Pending
        {
            if let Input::Submit(action) = ui
                .command_prompt("Work stopped. [i] details  [n] next goal")
                .await?
                && matches!(action.trim(), "i" | "details")
            {
                ui.diagnostics(&run).await?;
                continue;
            }
            break;
        }
        // The one point every Ready, review-pending result reaches, whether
        // it came straight from the work call or after a clarification was
        // answered: the mode in force at this moment decides.
        if ui.auto_apply {
            return auto_apply_goal(ui, state, run, options).await;
        }
        return review_goal(ui, state, run, options, "").await;
    }
    Ok(())
}

/// The moment a goal finishes Ready under auto-apply: run `orchestrator::auto_apply`
/// and present its outcome. Never calls a review-recording function itself;
/// `Applied` records nothing further, and every other outcome hands off to
/// the ordinary human review menu with a notice explaining why.
async fn auto_apply_goal(
    ui: &mut Ui,
    state: &State,
    run: RunRecord,
    options: Options,
) -> Result<()> {
    let outcome_slot: std::rc::Rc<std::cell::Cell<Option<ApplyOutcome>>> = Default::default();
    let slot = outcome_slot.clone();
    let id = run.id.clone();
    let future = async {
        let outcome = tokio::task::block_in_place(|| orchestrator::auto_apply(state, &id));
        match outcome {
            Ok(outcome) => slot.set(Some(outcome)),
            Err(error) => return Err(error),
        }
        state.load_run(&id)
    };
    let run = ui
        .work_static(
            "Auto-apply: validating against the current source… (checks on the merged tree can take as long as your verification; not interruptible)",
            future,
        )
        .await?;
    match outcome_slot
        .take()
        .expect("auto_apply records its outcome before the reloaded run returns")
    {
        ApplyOutcome::Applied { validity, .. } => {
            let mut text = projection(&run, None, ui.width().saturating_sub(3), options.ascii);
            text.push_str("\nAuto-applied · review not performed");
            if let Some(validity) = &validity {
                text.push_str(&format!("\n{}", coherence_text(validity)));
            }
            ui.commit(&text)?;
            ui.reviewed = None;
            Ok(())
        }
        ApplyOutcome::Skipped { reason } => {
            let human = match reason.as_str() {
                "verification_not_configured" => "no checks configured",
                "verification_failed" => "verification failed",
                "integration_checks_unavailable" => "checks cannot run on the merged tree",
                other => other,
            };
            review_goal(
                ui,
                state,
                run,
                options,
                format!("Auto-apply skipped: {human}"),
            )
            .await
        }
        ApplyOutcome::Blocked { reason, validity } => {
            let notice = match &validity {
                Some(validity) => format!("Auto-apply blocked: {}", coherence_text(validity)),
                None => format!("Auto-apply blocked: {reason}"),
            };
            review_goal(ui, state, run, options, notice).await
        }
        ApplyOutcome::Failed { error } => {
            review_goal(
                ui,
                state,
                run,
                options,
                format!("Auto-apply failed: {}", clip(&error, 240)),
            )
            .await
        }
    }
}

async fn review_goal(
    ui: &mut Ui,
    state: &State,
    mut run: RunRecord,
    options: Options,
    notice: impl Into<String>,
) -> Result<()> {
    let mut target = review_command(&run)?;
    ui.draw(
        &projection(&run, None, ui.width().saturating_sub(3), options.ascii),
        None,
        "Preparing change index…",
    )?;
    let state_copy = state.clone();
    let target_copy = target.clone();
    let mut bundle = tokio::task::spawn_blocking(move || {
        crate::reviewer::ReviewBundle::prepare(&state_copy, &target_copy)
    })
    .await??;
    let preview = inspection::tiny_preview(&bundle)?;
    let mut view = inspection::ReviewView::default();
    let mut notice = notice.into();
    ui.input_boundary().await?;
    loop {
        let Input::Submit(action) = ui.review_action(&run, &preview, &notice).await? else {
            ui.commit("Left pending. No acceptance or application recorded.")?;
            return Ok(());
        };
        let action = action.trim().to_ascii_lowercase();
        match action.as_str() {
            "" | "d" | "diff" | "e" | "editor" => {
                let external = matches!(action.as_str(), "e" | "editor");
                if options.plain && external {
                    notice = "External reviewers require the interactive terminal. Use Review changes here.".into();
                    continue;
                }
                notice = ui.inspect(&mut bundle, &mut view, state, &target, external).await?;
                bundle.verify()?;
                match orchestrator::refresh_review_target(state, &target) {
                    Ok(current) => {
                        run = current;
                        target.revision = run.state_revision;
                    }
                    Err(error) => {
                        ui.commit(&format!("Review changed elsewhere: {error}. No action was applied."))?;
                        return Ok(());
                    }
                }
            }
            "i" | "details" => ui.diagnostics(&run).await?,
            "aa" if !ui.auto_apply_mode => {
                notice = "watch has no auto-apply mode; nothing changed.".into();
            }
            "a" | "accept" | "aa" | "r" | "reject" => {
                bundle.verify()?;
                // "aa" is a real human accept, recorded exactly like "a"; the
                // mode flips only after that acceptance is actually recorded.
                let accept = matches!(action.as_str(), "a" | "accept" | "aa");
                let result = orchestrator::review_delivery(state, &target, accept);
                run = state.load_run(&target.run_id)?;
                ui.commit(&projection(&run, None, ui.width().saturating_sub(3), options.ascii))?;
                if let Err(error) = &result {
                    ui.commit(&format!("{error:#}"))?;
                }
                if matches!(run.outcome.review, ReviewState::Accepted | ReviewState::Rejected) {
                    ui.reviewed = Some(run);
                }
                if action == "aa" && result.is_ok() {
                    ui.auto_apply = true;
                    ui.commit(AUTO_APPLY_ON_LINE)?;
                }
                return Ok(());
            }
            "n" | "next" => {
                ui.commit("Left pending. You can review this result later.")?;
                return Ok(());
            }
            _ => notice = "Choose Review changes, Open in editor, Accept & apply, Reject, Leave pending, or Details.".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn run() -> RunRecord {
        serde_json::from_value(serde_json::json!({
            "id":"01TEST", "task":"Fix cache", "exact_prompt":"Fix cache",
            "source_path":"/source", "source_kind":"directory", "source_git_head":null,
            "source_fingerprint":"baseline", "baseline_path":"/baseline", "baseline_commit":"abc",
            "status":"running", "created_at":"2026-09-17T00:00:00Z", "completed_at":null,
            "environment":{"dispatch_version":"test","os":"test","architecture":"test","execution_backend":"local","timeout_secs":30,"cpus":1.0,"memory":"1g","max_parallel":1},
            "evaluation":null,"applied_candidate":null
        })).unwrap()
    }
    fn render(text: &str, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
        terminal
            .draw(|f| f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), f.area()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(width as usize)
            .map(|row| {
                row.iter()
                    .map(|c| c.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_owned()
    }
    #[test]
    fn state_render_wide_narrow_ascii_snapshots() {
        let mut r = run();
        r.outcome.work_result = WorkResult::Ready;
        r.outcome.verification = VerificationState::Passed;
        r.outcome.phase = RunPhase::Reviewing;
        r.outcome.review = ReviewState::Pending;
        assert_eq!(
            render(&projection(&r, None, 90, false), 90),
            "Goal  Fix cache\n\nReady for review\n● goal ── ○ work ━━ ● verify ━━ ● review\nVerification passed"
        );
        assert_eq!(
            render(&projection(&r, None, 24, true), 24),
            "Goal  Fix cache\n\nReady for review\no work\n* checks  * review\nVerification passed"
        );
        for (waiting, expected) in [
            (WaitingOn::Human, "Waiting for you"),
            (WaitingOn::Admission, "Waiting for shared admission"),
            (WaitingOn::Capacity, "Waiting for subscription capacity"),
        ] {
            r.outcome.waiting_on = waiting;
            assert_eq!(label(&r, None), expected);
        }
        r.outcome.application = ApplicationState::BlockedBySourceDrift;
        assert_eq!(label(&r, None), "Application blocked: source changed");
    }

    fn attachment() -> AttachmentRecord {
        AttachmentRecord {
            version: 1,
            workspace: std::path::PathBuf::from("/repo-worktree"),
            integration_root: std::path::PathBuf::from("/repo"),
            repo_key: Some("sha256:deadbeef".into()),
            provenance: BaselineProvenance::GitMergeBase {
                commit: "0123456789abcdef".into(),
            },
            confidence: AttachConfidence::Full,
            agent: Some("claude".into()),
            command: Some(vec!["claude".into()]),
            owner: None,
            agent_process: None,
            owner_state: OwnerState::Live,
            capabilities: AttachCapabilities {
                observe: true,
                signal: true,
                control: true,
                integrate: false,
            },
            attached_at: chrono::Utc::now(),
            finished_at: None,
            finish_reason: None,
            workspace_owner: Default::default(),
            managed: None,
            sessions: Vec::new(),
            workspace_removed: None,
        }
    }

    /// S1 (attach part 14): the label and graph-line text an attached run
    /// gets while its agent is working, and when its owner has gone away.
    /// No other run's label or graph-line model name changes (see the
    /// unaffected snapshots above and below, which stay on `mode: Legacy`).
    #[test]
    fn attached_run_working_and_owner_gone_labels_and_graph_model_name() {
        let mut r = run();
        r.mode = RunMode::Attached;
        r.attachment = Some(attachment());
        r.outcome.lifecycle = LifecycleState::Working;
        r.outcome.phase = RunPhase::Executing;
        r.outcome.work_result = WorkResult::Pending;
        assert_eq!(label(&r, None), "Attached agent working");
        let text = projection(&r, None, 90, false);
        assert!(text.contains("Attached agent working"), "{text}");
        assert!(text.contains("claude"), "{text}");

        // No attachment agent name: the graph line falls back to "external".
        r.attachment.as_mut().unwrap().agent = None;
        assert!(projection(&r, None, 90, false).contains("external"));

        // The owner is gone and the run has not finished: a distinct label,
        // higher priority than "working".
        r.attachment.as_mut().unwrap().owner_state = OwnerState::Gone;
        assert_eq!(
            label(&r, None),
            "Attached agent's owner is gone; finish or reject"
        );

        // Once finished, the owner-gone label no longer applies.
        r.outcome.lifecycle = LifecycleState::Finished;
        assert_ne!(
            label(&r, None),
            "Attached agent's owner is gone; finish or reject"
        );
    }

    #[test]
    fn waiting_for_a_decision_or_capacity_never_claims_agent_is_running() {
        let mut r = run();
        r.outcome.phase = RunPhase::Executing;
        r.outcome.work_result = WorkResult::Pending;
        r.outcome.waiting_on = WaitingOn::None;
        assert!(projection(&r, None, 90, false).contains("Agent running"));
        for waiting in [
            WaitingOn::Human,
            WaitingOn::Capacity,
            WaitingOn::Admission,
            WaitingOn::Reconciliation,
            WaitingOn::Authorization,
        ] {
            r.outcome.waiting_on = waiting;
            assert!(!projection(&r, None, 90, false).contains("Agent running"));
        }
    }

    #[test]
    fn compact_input_and_activity_survive_resize() {
        let mut editor = Editor::new();
        editor.event(Event::Paste("α\n界".into()));
        for width in [90, 24, 8, 1, 90] {
            let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
            terminal
                .draw(|f| {
                    draw_view(
                        f,
                        "Your answer",
                        Some(&editor),
                        "Enter send",
                        &Theme::from_hints(true, None, None, None, None),
                        true,
                        false,
                    )
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let area = content_area(Rect::new(0, 0, width, 8));
            let body_height = Paragraph::new("Your answer")
                .wrap(Wrap { trim: false })
                .line_count(area.width)
                .min(
                    6 - Paragraph::new("Enter send")
                        .wrap(Wrap { trim: false })
                        .line_count(area.width)
                        .min(3),
                ) as u16;
            assert_eq!(buffer[(area.x, body_height)].symbol(), ">");
            assert_eq!(editor.text.lines(), &["α", "界"]);
        }
        assert_eq!(
            activity_hint(Duration::ZERO, true),
            "| 0s elapsed · Ctrl+C cancel"
        );
        assert_eq!(
            activity_hint(Duration::from_millis(1200), true),
            "- 1s elapsed · Ctrl+C cancel"
        );
        assert_ne!(
            activity_hint(Duration::ZERO, false),
            activity_hint(Duration::from_millis(200), false)
        );
    }
    #[test]
    fn tiny_diff_emphasizes_only_changed_graphemes() {
        let palette = Theme::from_hints(true, None, None, None, None);
        let lines = diff_lines("- int radius = 20;\n+ int radius = 40;", &palette);
        assert_eq!(lines[0].spans[1].content, "2");
        assert_eq!(lines[1].spans[1].content, "4");
        assert!(
            lines[0].spans[1]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED)
        );
        assert_eq!(lines[0].spans[1].style.fg, None);
        for text in ["-é🙂\n+é界", "-\n+new", "-old\n+", "-same\n+same"] {
            let displayed = diff_lines(text, &palette)
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(displayed, text);
        }
    }
    fn with_validity(
        decision: Decision,
        world_changed: bool,
        changed_files: u32,
        detail: Option<&str>,
    ) -> RunRecord {
        let mut r = run();
        r.outcome.work_result = WorkResult::Ready;
        r.outcome.verification = VerificationState::Passed;
        r.coherence = Some(CoherenceRecord {
            version: 1,
            refreshed_from: None,
            validity: Some(Validity {
                decision,
                evaluated_at: chrono::Utc::now(),
                world_digest: String::new(),
                world_changed,
                changed_files,
                reasons: detail
                    .map(|detail| Reason {
                        code: ReasonCode::FactBroken,
                        fact_id: None,
                        path: None,
                        detail: detail.into(),
                    })
                    .into_iter()
                    .collect(),
                analysis: AnalysisLevel::Symbols,
            }),
            first_invalid_at: None,
            overridden: None,
        });
        r
    }

    #[test]
    fn coherence_line_follows_verification_for_every_verdict() {
        let cases = [
            (
                with_validity(Decision::Continue, false, 0, None),
                "Coherence: CONTINUE — world unchanged",
            ),
            (
                with_validity(Decision::Continue, true, 4, None),
                "Coherence: CONTINUE — 4 files changed underneath, none affect this work",
            ),
            (
                with_validity(Decision::Continue, true, 1, None),
                "Coherence: CONTINUE — 1 file changed underneath, none affect this work",
            ),
            (
                with_validity(
                    Decision::Refresh,
                    true,
                    1,
                    Some("auth::validate changed\nagain"),
                ),
                "Coherence: REFRESH — auth::validate changed again",
            ),
            (
                with_validity(Decision::Refresh, true, 1, None),
                "Coherence: REFRESH — files changed underneath the work",
            ),
            (
                with_validity(Decision::Stop, true, 1, None),
                "Coherence: STOP — this patch is already in the source",
            ),
        ];
        for (r, expected) in cases {
            let text = projection(&r, None, 120, false);
            let lines = text.lines().collect::<Vec<_>>();
            let at = lines
                .iter()
                .position(|line| line.starts_with("Verification"))
                .unwrap();
            assert_eq!(lines[at + 1], expected, "{text}");
            let ascii = projection(&r, None, 120, true);
            assert!(ascii.contains(&expected.replace('—', "-")), "{ascii}");
            assert!(!ascii.contains('—'));
        }
    }

    #[test]
    fn coherence_line_is_absent_without_validity_or_ready_work() {
        let r = run();
        assert!(!projection(&r, None, 90, false).contains("Coherence"));
        let mut pending = with_validity(Decision::Refresh, true, 1, Some("x"));
        pending.outcome.work_result = WorkResult::Pending;
        assert!(!projection(&pending, None, 90, false).contains("Coherence"));
        let mut plain = with_validity(Decision::Refresh, true, 1, Some("x"));
        plain.coherence.as_mut().unwrap().validity = None;
        assert!(!projection(&plain, None, 90, false).contains("Coherence"));
    }

    #[test]
    fn coherence_line_is_clipped_to_the_width() {
        let r = with_validity(Decision::Refresh, true, 1, Some(&"long detail ".repeat(20)));
        let text = projection(&r, None, 40, false);
        let line = text.lines().find(|l| l.starts_with("Coherence")).unwrap();
        assert!(UnicodeWidthStr::width(line) <= 40, "{line}");
        assert!(line.ends_with("..."));
    }

    #[test]
    fn unconfigured_checks_never_look_verified() {
        let mut r = run();
        r.outcome.work_result = WorkResult::Ready;
        r.outcome.verification = VerificationState::NotConfigured;
        let text = projection(&r, None, 90, false);
        assert!(text.contains("Unverified — no checks configured"));
        assert!(!text.contains("● verify"));
        let palette = Theme::from_hints(false, Some("truecolor"), Some("dark"), None, None);
        let rendered = styled_body(&text, &palette);
        assert!(
            !rendered
                .lines
                .iter()
                .any(|line| line.style == palette.success)
        );
    }
    #[test]
    fn malicious_content_cannot_control_terminal() {
        assert_eq!(
            sanitize(
                "a\x1b[31mred\x1b[0m\x1b]52;c;secret\x07b\x1bPpayload\x1b\\c\u{202e}d\u{009b}2J\x00"
            ),
            "aredbcd"
        );
        assert_eq!(sanitize("x\x1b]unfinished"), "x");
    }
    #[test]
    fn paste_unicode_resize_submit_once() {
        let mut editor = Editor::new();
        assert_eq!(editor.event(Event::Paste("α\n界🙂".into())), Input::Changed);
        for (w, h) in [(100, 30), (18, 5), (80, 24)] {
            editor.event(Event::Resize(w, h));
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|f| f.render_widget(&editor.text, f.area()))
                .unwrap();
        }
        editor.event(Event::Key(event::KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )));
        assert_eq!(
            editor.event(Event::Key(event::KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE
            ))),
            Input::Submit("α\n界".into())
        );
        assert_eq!(
            editor.event(Event::Key(event::KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE
            ))),
            Input::Submit(String::new())
        );
        assert_eq!(
            editor.event(Event::Key(event::KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL
            ))),
            Input::Eof
        );
        assert_eq!(
            editor.event(Event::Key(event::KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL
            ))),
            Input::Cancel
        );
    }
    #[test]
    fn back_tab_toggles_auto_apply_and_leaves_the_draft_untouched() {
        let mut editor = Editor::new();
        editor.text.insert_str("keep me");
        assert_eq!(
            editor.event(Event::Key(event::KeyEvent::new(
                KeyCode::BackTab,
                KeyModifiers::SHIFT
            ))),
            Input::ToggleAutoApply
        );
        assert_eq!(editor.text.lines(), &["keep me"]);
        // Crossterm reports Shift+Tab as BackTab with no explicit SHIFT
        // modifier on some terminals; either form must be intercepted.
        assert_eq!(
            editor.event(Event::Key(event::KeyEvent::new(
                KeyCode::BackTab,
                KeyModifiers::NONE
            ))),
            Input::ToggleAutoApply
        );
        assert_eq!(editor.text.lines(), &["keep me"]);
    }
    #[test]
    fn mode_hint_reflects_state_in_both_charsets() {
        assert_eq!(
            mode_hint_text(false, false),
            "⏸ review before apply · Shift+Tab"
        );
        assert_eq!(
            mode_hint_text(false, true),
            "⏵⏵ auto-apply on · Shift+Tab to pause"
        );
        assert_eq!(
            mode_hint_text(true, false),
            "[review before apply] Shift+Tab"
        );
        assert_eq!(
            mode_hint_text(true, true),
            ">> AUTO-APPLY ON - Shift+Tab to pause"
        );
    }
    #[test]
    fn auto_apply_command_parses_on_off_and_bare_toggle() {
        assert_eq!(
            parse_auto_apply_command("/auto-apply"),
            Some(AutoApplyCommand::Toggle)
        );
        assert_eq!(
            parse_auto_apply_command("  /auto-apply  "),
            Some(AutoApplyCommand::Toggle)
        );
        assert_eq!(
            parse_auto_apply_command("/auto-apply on"),
            Some(AutoApplyCommand::On)
        );
        assert_eq!(
            parse_auto_apply_command("/auto-apply off"),
            Some(AutoApplyCommand::Off)
        );
        assert_eq!(parse_auto_apply_command("/auto-apply maybe"), None);
        assert_eq!(parse_auto_apply_command("auto-apply on"), None);
        assert_eq!(parse_auto_apply_command("Add tests"), None);
    }
    #[test]
    fn resize_work_and_question_preserves_committed_projection() {
        let mut r = run();
        r.outcome.lifecycle = LifecycleState::Working;
        r.outcome.phase = RunPhase::Verifying;
        let before = serde_json::to_value(&r).unwrap();
        for width in [100, 18, 1, 80] {
            let _ = render(&projection(&r, None, width, false), width);
        }
        assert_eq!(serde_json::to_value(&r).unwrap(), before);
        r.outcome.waiting_on = WaitingOn::Human;
        for width in [80, 18] {
            assert!(projection(&r, None, width, true).contains("Waiting for you"));
        }
    }
    #[test]
    #[ignore = "subprocess helper, exercised by terminal_panic_restores_in_pty"]
    fn terminal_panic_fixture() {
        assert!(suitable());
        let inspection = std::env::var_os("DISPATCH_TEST_INSPECTION_PANIC").is_some();
        println!("primary-terminal-sentinel");
        io::stdout().flush().unwrap();
        let result = std::panic::catch_unwind(|| {
            let _screen = Screen::open(inspection).unwrap();
            println!("panic-fixture-ready");
            panic!("intentional terminal restoration test");
        });
        assert!(result.is_err());
        if inspection {
            // A real shell reads after return. On macOS that read also clears
            // the kernel's transient PENDIN flag after raw -> cooked mode.
            println!("panic-restored-read");
            let mut line = String::new();
            io::stdin().read_line(&mut line).unwrap();
        }
    }

    #[test]
    #[cfg(unix)]
    fn terminal_panic_restores_in_pty() {
        let temp = tempfile::tempdir().unwrap();
        for scenario in ["panic", "inspection-panic"] {
            let output = std::process::Command::new("python3")
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/tui_session.py"
                ))
                .arg(std::env::current_exe().unwrap())
                .arg(temp.path())
                .arg(temp.path())
                .arg(scenario)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{scenario}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
