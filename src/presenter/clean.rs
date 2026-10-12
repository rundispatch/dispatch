//! `dispatch clean`: remove the workspaces Dispatch made for Work that is
//! over, and only after the person confirms exactly what is listed. Nothing
//! is ever removed by a timer. Workspaces you or an agent's runtime made are
//! listed, never removed.

use super::*;
use crate::{WorkspaceOwner, orchestrator::attach};

pub async fn clean(state: &State, dry_run: bool, yes: bool, options: Options) -> Result<()> {
    let (listed, kept) = attach::cleanable(state)?;
    remove(state, &listed, dry_run, yes, options).await?;
    if kept.is_empty() {
        return Ok(());
    }
    println!("\nKept, because Dispatch did not make them:");
    for item in &kept {
        let work = item
            .work
            .iter()
            .map(|(id, why)| format!("Work {id} ({why})"))
            .collect::<Vec<_>>()
            .join(", ");
        let made_by = match item.owner {
            WorkspaceOwner::Runtime => "the agent's runtime",
            _ => "you",
        };
        println!(
            "  {} · made by {made_by} · {work}",
            item.workspace.display()
        );
    }
    println!(
        "Inspect one with git -C <path> status. Once nothing uses it, remove it with git worktree remove <path>, or through the runtime that made it. A locked worktree may still belong to a running agent session."
    );
    Ok(())
}

async fn remove(
    state: &State,
    listed: &[attach::Cleanable],
    dry_run: bool,
    yes: bool,
    options: Options,
) -> Result<()> {
    if listed.is_empty() {
        println!(
            "Nothing to clean: no workspace Dispatch made is left over. Dispatch removes only the workspaces it made; worktrees you or an agent's runtime made stay until you remove them."
        );
        return Ok(());
    }
    let lines = listed
        .iter()
        .map(|item| {
            let branch = item
                .branch
                .as_deref()
                .map(|branch| format!(" (branch {branch})"))
                .unwrap_or_default();
            format!(
                "  {}{branch} · Work {} · {}",
                item.workspace.display(),
                item.run_id,
                item.why
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    if dry_run {
        println!("Would remove:\n{lines}\nNothing was removed.");
        return Ok(());
    }
    let confirmed = if yes {
        true
    } else {
        anyhow::ensure!(
            suitable(),
            "dispatch clean asks before removing anything; pass --yes to remove the listed workspaces without asking, or --dry-run to only list them"
        );
        let mut ui = Ui::new(options)?;
        let question = format!(
            "Remove these workspaces Dispatch made? Their work is over, and what they held is kept in each Work's changes.\n{lines}"
        );
        let rows = [Choice::new("Remove them"), Choice::new("Cancel")];
        ui.select(&question, &rows, 1).await? == Some(0)
    };
    if !confirmed {
        println!("Nothing was removed.");
        return Ok(());
    }
    let removed = attach::clean(state, listed)?;
    println!("Removed {removed} workspace(s) Dispatch made.");
    Ok(())
}
