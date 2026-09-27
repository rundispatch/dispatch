//! `dispatch watch` on a terminal: the project view, where the person can act
//! on a Work item without typing its ID. Every action is exactly the command
//! they would type, run as them under the same locks and authority; `watch`
//! still owns nothing and records nothing of its own.

use std::path::PathBuf;

use super::*;
use crate::orchestrator::{self, ReviewDecision, background, serve};

const HINT: &str = "↑↓ move · f finish · a accept · r reject · d or Enter review · q leave";

pub async fn interactive(state: &State, root: PathBuf, options: Options) -> Result<()> {
    let mut ui = Ui::new(options)?;
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut header = String::new();
    let mut focus = 0_usize;
    let mut notice = String::new();
    let mut journal: Option<i64> = None;
    let mut loaded: Option<std::time::Instant> = None;
    loop {
        // The event journal is the doorbell; the clock ages finished Work out.
        let now = crate::db::Database::open_read_only(state.db_path())
            .and_then(|db| db.latest_event_id())
            .ok()
            .flatten();
        if loaded.is_none_or(|at| at.elapsed() >= Duration::from_secs(2)) || now != journal {
            header = background::project_line(state, &root);
            rows = serve::project_rows(&serve::load_source_runs(state, &root)?);
            focus = focus.min(rows.len().saturating_sub(1));
            journal = now;
            loaded = Some(std::time::Instant::now());
        }
        let marker = if options.ascii { ">" } else { "›" };
        let listed = if rows.is_empty() {
            "  no Work in the last hour".to_owned()
        } else {
            rows.iter()
                .enumerate()
                .map(|(i, (_, line))| format!("{} {line}", if i == focus { marker } else { " " }))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let body = format!("{} · {header}\n{listed}\n\n{notice}", root.display());
        ui.draw(&body, None, HINT)?;
        let Some(event) = ui.next().await? else {
            return Ok(());
        };
        let Event::Key(key) = event else { continue };
        let selected = rows.get(focus).map(|(id, _)| id.clone());
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => focus = focus.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                focus = (focus + 1).min(rows.len().saturating_sub(1));
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            _ if selected.is_none() => {}
            KeyCode::Char('a') => {
                notice = outcome(orchestrator::decide(
                    state,
                    selected.as_deref(),
                    &root,
                    ReviewDecision::Accept {
                        despite_refresh: false,
                    },
                    Vec::new(),
                    None,
                    true,
                ));
            }
            KeyCode::Char('r') => {
                let id = selected.unwrap();
                let question = format!(
                    "Reject {}? Nothing is applied, and a workspace Dispatch made is kept.",
                    &id[..8.min(id.len())]
                );
                let rows = [Choice::new("Reject"), Choice::new("Cancel")];
                notice = if ui.select(&question, &rows, 1).await? == Some(0) {
                    outcome(orchestrator::decide(
                        state,
                        Some(&id),
                        &root,
                        ReviewDecision::Reject,
                        Vec::new(),
                        None,
                        true,
                    ))
                } else {
                    "Nothing changed.".into()
                };
            }
            KeyCode::Char('f') => notice = finish(&mut ui, state, &selected.unwrap()).await?,
            KeyCode::Char('d') | KeyCode::Enter => {
                let run = state.load_run(&selected.unwrap())?;
                notice = if run.outcome.review == ReviewState::Pending
                    && run.outcome.work_result == WorkResult::Ready
                {
                    review_goal(&mut ui, state, run, options, "").await?;
                    String::new()
                } else {
                    "Only a result waiting for review can be reviewed.".into()
                };
            }
            _ => {}
        }
    }
}

fn outcome(result: Result<String>) -> String {
    match result {
        Ok(line) => line,
        Err(error) => format!("Refused: {error:#}"),
    }
}

/// `f`: `dispatch finish` for active attached Work. Running the project's
/// checks needs the person's word, unless the Work already carries it.
async fn finish(ui: &mut Ui, state: &State, id: &str) -> Result<String> {
    let run = state.load_run(id)?;
    if run.mode != crate::RunMode::Attached || run.outcome.lifecycle == LifecycleState::Finished {
        return Ok("Only attached Work still in progress can be finished.".into());
    }
    let checks = crate::coherence::run_config(&state.run_dir(id))
        .map(|config| config.checks.verify)
        .unwrap_or_default();
    let mut allow = false;
    if !checks.is_empty() && !run.environment.unsafe_local {
        let question = format!(
            "Finishing runs this project's checks on your machine, with your permissions:\n{}",
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
            return Ok("Nothing changed.".into());
        }
        allow = true;
    }
    Ok(
        match crate::orchestrator::attach::finish_quietly(state, id, allow).await {
            Ok(run) => format!(
                "Finished {}: {}.",
                &run.id[..8.min(run.id.len())],
                match run.outcome.verification {
                    crate::VerificationState::Passed => "checks passed, waiting for review",
                    crate::VerificationState::Failed => "checks failed",
                    _ => "waiting for review",
                }
            ),
            Err(error) => format!("Refused: {error:#}"),
        },
    )
}
