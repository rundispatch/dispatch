//! `dispatch clean`: remove the workspaces Dispatch made for Work that is
//! over, and only after the person confirms exactly what is listed. Nothing
//! is ever removed by a timer.

use super::*;
use crate::orchestrator::attach;

pub async fn clean(state: &State, dry_run: bool, yes: bool, options: Options) -> Result<()> {
    let listed = attach::cleanable(state)?;
    if listed.is_empty() {
        println!("Nothing to clean: no workspace Dispatch made is left over.");
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
                &item.run_id[..8.min(item.run_id.len())],
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
    let removed = attach::clean(state, &listed)?;
    println!("Removed {removed} workspace(s) Dispatch made.");
    Ok(())
}
