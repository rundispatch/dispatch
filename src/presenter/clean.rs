//! `dispatch clean`: remove the workspaces Dispatch made for Work that is
//! over, and internal check workspaces, only after the person confirms exactly
//! what is listed. Nothing is ever removed by a timer. Workspaces you or an
//! agent's runtime made are listed, never removed.

use super::*;
use crate::{WorkspaceOwner, lock::OperationLock, orchestrator::attach};
use std::{fs, path::PathBuf};

pub async fn clean(state: &State, dry_run: bool, yes: bool, options: Options) -> Result<()> {
    let (listed, kept) = attach::cleanable(state)?;
    let check_workspaces = check_workspaces(state)?;
    if !dry_run {
        crate::coherence::memo::clear(state)?;
    }
    remove(state, &listed, &check_workspaces, dry_run, yes, options).await?;
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
    check_workspaces: &[PathBuf],
    dry_run: bool,
    yes: bool,
    options: Options,
) -> Result<()> {
    if listed.is_empty() && check_workspaces.is_empty() {
        println!(
            "Nothing to clean: no workspace Dispatch made is left over. Dispatch removes only the workspaces it made; worktrees you or an agent's runtime made stay until you remove them."
        );
        return Ok(());
    }
    let mut lines = listed
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
        .collect::<Vec<_>>();
    lines.extend(
        check_workspaces
            .iter()
            .map(|path| format!("  {} · internal check workspace", path.display())),
    );
    let lines = lines.join("\n");
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
        let question = if check_workspaces.is_empty() {
            format!(
                "Remove these workspaces Dispatch made? Their work is over, and what they held is kept in each Work's changes.\n{lines}"
            )
        } else {
            format!(
                "Remove these workspaces Dispatch made? Check workspaces hold temporary verification and build files; Work records are kept separately.\n{lines}"
            )
        };
        let rows = [Choice::new("Remove them"), Choice::new("Cancel")];
        ui.select(&question, &rows, 1).await? == Some(0)
    };
    if !confirmed {
        println!("Nothing was removed.");
        return Ok(());
    }
    let mut removed = attach::clean(state, listed)?;
    for path in check_workspaces {
        let Some(key) = path.file_name() else {
            continue;
        };
        let lock = state
            .root
            .join("locks")
            .join(format!("source-{}.lock", key.to_string_lossy()));
        let _source_lock = OperationLock::acquire(&lock, "the project's checks are in use")?;
        if path.is_dir() {
            fs::remove_dir_all(path)?;
            removed += 1;
        }
    }
    println!("Removed {removed} workspace(s) Dispatch made.");
    Ok(())
}

fn check_workspaces(state: &State) -> Result<Vec<PathBuf>> {
    let root = state.root.join("check-workspaces");
    let metadata = match fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "check workspaces path is not a directory: {}",
        root.display()
    );
    let mut found = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.len() == 64
            && name
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            && entry.file_type()?.is_dir()
        {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}
