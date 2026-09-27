//! Agent runtimes that report their sessions to Dispatch (`dispatch hook
//! <provider>`), so Work appears without anyone typing `attach`. A provider
//! adapter turns its runtime's own hook input into a `RuntimeEvent`; `ingest`
//! is the same for every provider and knows nothing of Claude sessions or
//! Codex threads. The Work is the workspace, never a session: sessions come
//! and go in it and are recorded as its actors. Nothing here decides a
//! verdict, verifies, applies or deletes; the project owner follows the Work
//! like any other.
//!
//! Hook input is untrusted. It never chooses the integration root (Dispatch
//! derives it from the workspace's own repository), and acts only in a
//! project someone chose to watch with `dispatch start`.

pub mod claude;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use sha2::{Digest, Sha256};

use crate::{
    RuntimeSession,
    lock::OperationLock,
    orchestrator::{
        attach::{self, AttachRequest, RuntimeStart},
        background::{self, Watcher},
    },
    source,
    state::State,
};

/// The most hook input Dispatch reads.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// One runtime lifecycle event, in Dispatch's words.
pub struct RuntimeEvent {
    pub provider: &'static str,
    pub session_id: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub kind: EventKind,
}

pub enum EventKind {
    /// `source` is the runtime's own word for how the session began.
    Start {
        source: String,
        resumed: bool,
    },
    End {
        reason: String,
    },
    /// The runtime is about to delete the workspace at `cwd`.
    WorkspaceRemoved,
}

/// What the runtime should be told.
#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    Silent,
    /// Shown to the person in the agent's session.
    Notice(String),
}

const SHARED_CHECKOUT: &str = "Dispatch: this session works directly in your checkout, so \
     Dispatch cannot tell its edits from yours and does not track them. For tracked work, \
     start the agent in its own worktree (claude --worktree) or with dispatch attach -- claude.";

/// Validate an event's fields; anything malformed is refused whole.
pub fn validate(event: &RuntimeEvent) -> Result<()> {
    anyhow::ensure!(
        !event.session_id.is_empty()
            && event.session_id.len() <= 128
            && event
                .session_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "invalid session id"
    );
    anyhow::ensure!(
        event.cwd.is_absolute() && event.cwd.as_os_str().len() <= 4096,
        "the working directory must be an absolute path"
    );
    let word = match &event.kind {
        EventKind::Start { source, .. } => source.as_str(),
        EventKind::End { reason } => reason.as_str(),
        EventKind::WorkspaceRemoved => "removed",
    };
    anyhow::ensure!(
        word.len() <= 32
            && word
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_'),
        "invalid event detail"
    );
    anyhow::ensure!(
        event
            .model
            .as_ref()
            .is_none_or(|model| model.len() <= 128 && model.bytes().all(|b| b.is_ascii_graphic())),
        "invalid model"
    );
    Ok(())
}

/// Apply one event. Sessions are followed only in a watched project; a
/// workspace about to be deleted keeps its Work's Δ either way.
pub fn ingest(state: &State, event: RuntimeEvent) -> Result<Reply> {
    validate(&event)?;
    let cwd = source::resolve_source(Some(&event.cwd)).context("the working directory")?;
    let Some(workspace) = source::checkout_top(&cwd)? else {
        // A plain directory has no separate workspace to follow.
        return Ok(Reply::Silent);
    };
    if let EventKind::WorkspaceRemoved = event.kind {
        return removed(state, &workspace);
    }
    let root = source::repo_identity(&workspace)?
        .context("the checkout lost its repository")?
        .main_worktree;
    if !matches!(background::watcher(state, &root)?, Watcher::Watched(_)) {
        return Ok(Reply::Silent);
    }
    match event.kind {
        EventKind::Start { source, resumed } if workspace != root => {
            let session = RuntimeSession {
                provider: event.provider.into(),
                session_id: event.session_id,
                source,
                started_at: Utc::now(),
                ended_at: None,
                end_reason: None,
                model: event.model,
            };
            start(state, &root, &workspace, session, resumed)
        }
        // Only a fresh start is known to stay in the checkout: a resumed
        // session reports it before re-entering its worktree.
        EventKind::Start { source, .. } if source == "startup" => {
            Ok(Reply::Notice(SHARED_CHECKOUT.into()))
        }
        EventKind::Start { .. } => Ok(Reply::Silent),
        EventKind::End { reason } => {
            if let Some(run_id) = attach::find_active_attachment(state, &workspace)? {
                attach::record_session_end(
                    state,
                    &run_id,
                    event.provider,
                    &event.session_id,
                    &reason,
                )?;
            }
            Ok(Reply::Silent)
        }
        EventKind::WorkspaceRemoved => Ok(Reply::Silent),
    }
}

/// The runtime is about to delete `workspace`: the last chance to keep its
/// Work's Δ, whether or not the project is still watched. An error here must
/// stop the deletion (the adapter's job).
fn removed(state: &State, workspace: &Path) -> Result<Reply> {
    let Some(run_id) = attach::find_active_attachment(state, workspace)? else {
        return Ok(Reply::Silent);
    };
    let files_changed = attach::freeze_removed_workspace(state, &run_id)?;
    let id = &run_id[..8.min(run_id.len())];
    // With the person's consent the owner runs the checks next; say so. The
    // changes are already kept, so nothing here may fail the hook.
    let consented = state
        .load_run(&run_id)
        .is_ok_and(|run| run.environment.unsafe_local)
        && crate::consent::project_root(workspace)
            .and_then(|project| crate::consent::consent(state, &project))
            .is_ok_and(|consent| consent.is_valid());
    Ok(Reply::Notice(if files_changed == 0 {
        format!("Dispatch closed Work {id}: the worktree held no changes.")
    } else if consented {
        format!(
            "Dispatch kept this worktree's changes as Work {id} and will run the project's \
             checks on them, as you allowed; then it waits for your review."
        )
    } else {
        format!(
            "Dispatch kept this worktree's changes as Work {id}; \
             finish or reject it: dispatch finish {id} or dispatch reject {id}."
        )
    }))
}

/// A session starting in a separate workspace: new Work the first time, with
/// S0 taken now, before the session's first turn; another session on the same
/// Work afterwards. One lock per workspace makes duplicate or concurrent starts
/// land on one Work.
fn start(
    state: &State,
    root: &Path,
    workspace: &Path,
    session: RuntimeSession,
    resumed: bool,
) -> Result<Reply> {
    let key = hex::encode(Sha256::digest(workspace.to_string_lossy().as_bytes()));
    let _lock = OperationLock::acquire_wait(
        &state
            .root
            .join("locks")
            .join(format!("workspace-{key}.lock")),
        "another session is registering this workspace",
        std::time::Duration::from_secs(10),
    )?;
    if let Some(run_id) = attach::find_active_attachment(state, workspace)? {
        attach::record_session_start(state, &run_id, session)?;
        return Ok(Reply::Silent);
    }
    let provider = session.provider.clone();
    // Authority to run the project's checks comes only from the person's
    // consent for exactly these checks, never from the session.
    let consented = crate::consent::consent(state, root)?.is_valid();
    let name = workspace
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let run = attach::create(
        state,
        AttachRequest {
            workspace: workspace.to_owned(),
            root: Some(root.to_owned()),
            task: Some(format!("{provider} session in {name}")),
            agent: Some(provider),
            pid: None,
            command: None,
            allow_unsafe_local: consented,
            auto_apply: false,
            runtime: Some(RuntimeStart { session, resumed }),
        },
    )?;
    Ok(Reply::Notice(format!(
        "Dispatch is tracking this worktree as Work {}; see it with dispatch watch.",
        &run.id[..8.min(run.id.len())]
    )))
}
