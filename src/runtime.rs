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
pub mod codex;
pub mod cursor;
pub mod hermes;

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::{
    AttachmentRecord, RuntimeSession, WorkspaceOwner,
    orchestrator::{
        attach,
        background::{self, Watcher},
        registration::{self, Outcome, Registration},
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
    let began = process_started();
    validate(&event)?;
    if !matches!(event.kind, EventKind::WorkspaceRemoved) && unwatched(state, &event.cwd) {
        return Ok(Reply::Silent);
    }
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
    // A project whose owner stopped without `dispatch stop` is still watched:
    // its owner comes back while this session is registered.
    if !matches!(background::watcher(state, &root)?, Watcher::Watched(_))
        && !background::revive_in_background(state, &root)
    {
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
            start(state, &root, &workspace, session, resumed, began)
        }
        // Only a fresh start is known to stay in the checkout: a resumed
        // session reports it before re-entering its worktree.
        EventKind::Start { source, .. } if source == "startup" => {
            Ok(Reply::Notice(SHARED_CHECKOUT.into()))
        }
        EventKind::Start { .. } => Ok(Reply::Silent),
        EventKind::End { reason } => {
            // Within the 5 s `SessionEnd` timeout setup installs.
            let wait = Duration::from_secs(3);
            registration::publish_first(state, &workspace, wait)?;
            let mut run_id = attach::find_active_attachment(state, &workspace)?;
            if run_id.is_none() {
                // Claude Code reports a worktree session's end from the main
                // checkout: match it by session instead.
                let (provider, session) = (event.provider, event.session_id.as_str());
                registration::publish_session_first(state, &root, provider, session, wait)?;
                run_id = attach::find_session_attachment(state, &root, provider, session)?;
            }
            if let Some(run_id) = run_id {
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

/// Whether a session in `cwd` is outside every watched project, decided from
/// the files alone, without Git or the database: there is no repository
/// there, or the serve lock of the one there is free and no owner stopped
/// without `dispatch stop`. Anything it cannot read for certain is left to
/// Git.
fn unwatched(state: &State, cwd: &Path) -> bool {
    let Ok(cwd) = std::fs::canonicalize(cwd) else {
        return false;
    };
    match main_worktree(&cwd) {
        Some(None) => true,
        Some(Some(root)) => {
            matches!(background::watcher(state, &root), Ok(Watcher::NotWatched))
                && !background::stopped_owner(state, &root)
        }
        None => false,
    }
}

/// The main worktree of the repository Git would find from `cwd`, read from
/// the nearest `.git`: `Some(None)` when there is none. `None` when it is a
/// shape this does not read: a `.git` that is not a repository, a separate
/// Git directory or submodule, or a configuration that may move the work
/// tree.
pub(crate) fn main_worktree(cwd: &Path) -> Option<Option<PathBuf>> {
    for directory in cwd.ancestors() {
        let dot_git = directory.join(".git");
        let Ok(metadata) = std::fs::symlink_metadata(&dot_git) else {
            continue;
        };
        let (gitdir, common) = if metadata.is_dir() {
            (dot_git.clone(), dot_git)
        } else if metadata.file_type().is_symlink() {
            return None;
        } else {
            let gitdir = claude::linked_gitdir(directory)?;
            let common = PathBuf::from(
                std::fs::read_to_string(gitdir.join("commondir"))
                    .ok()?
                    .trim(),
            );
            let common = std::fs::canonicalize(gitdir.join(common)).ok()?;
            (gitdir, common)
        };
        let config = std::fs::read_to_string(common.join("config")).ok()?;
        let moves = config.lines().any(|line| {
            let line: String = line
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
                .to_ascii_lowercase();
            line.starts_with("worktree") || line == "bare=true" || line.starts_with("[include")
        });
        if moves
            || !gitdir.join("HEAD").is_file()
            || common.file_name() != Some(std::ffi::OsStr::new(".git"))
        {
            return None;
        }
        return Some(Some(common.parent()?.to_owned()));
    }
    Some(None)
}

/// The runtime is about to delete `workspace`: the last chance to keep its
/// Work's Δ, whether or not the project is still watched. An error here must
/// stop the deletion (the adapter's job).
fn removed(state: &State, workspace: &Path) -> Result<Reply> {
    registration::publish_first(state, workspace, Duration::from_secs(60))?;
    let Some(run_id) = attach::find_active_attachment(state, workspace)? else {
        return Ok(Reply::Silent);
    };
    let files_changed = attach::freeze_removed_workspace(state, &run_id)?;
    let id = &run_id;
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
            "Dispatch kept this worktree's changes as Work {id}; finish it, or close it: \
             dispatch finish {id} or dispatch close {id} --reason workspace-removed."
        )
    }))
}

/// A session starting in a separate workspace: new Work the first time, with
/// S0 taken now, before the session's first turn; another session on the same
/// Work afterwards. Registration decides within its budget from `began`.
fn start(
    state: &State,
    root: &Path,
    workspace: &Path,
    session: RuntimeSession,
    resumed: bool,
    began: (Instant, DateTime<Utc>),
) -> Result<Reply> {
    let registration = Registration {
        version: 1,
        run_id: ulid::Ulid::new().to_string(),
        workspace: workspace.to_owned(),
        root: root.to_owned(),
        s0_commit: String::new(),
        session,
        resumed,
        consented: false,
        started_at: began.1,
        reopens: None,
        not_reopened: None,
    };
    // The full id: sessions started together share their first characters.
    Ok(
        match registration::register(state, registration, began.0)? {
            None => Reply::Silent,
            Some(Outcome::Registered { run_id }) => Reply::Notice(format!(
                "Dispatch is tracking this worktree as Work {}; see it with dispatch watch.",
                run_id
            )),
            Some(Outcome::Pending { run_id }) => {
                Reply::Notice(registration::notice_pending(&run_id))
            }
            Some(Outcome::Untracked) => Reply::Notice(registration::NOTICE_UNTRACKED.into()),
        },
    )
}

/// When this process started: `main` records it before anything else, and a
/// hook's deadline counts from it. Exec and runtime start-up before `main` are
/// not counted; the budget's margin under the runtime's timeout covers them.
static STARTED: std::sync::OnceLock<(Instant, DateTime<Utc>)> = std::sync::OnceLock::new();

/// Record this process's start; called first thing in `main`.
pub fn mark_process_start() {
    let _ = STARTED.set((Instant::now(), Utc::now()));
}

fn process_started() -> (Instant, DateTime<Utc>) {
    *STARTED.get_or_init(|| (Instant::now(), Utc::now()))
}

/// The runtime's own advice for Work in a workspace it made that began from
/// source other than the project's; none for any other workspace.
pub fn start_advice(attachment: &AttachmentRecord) -> Option<&'static str> {
    if attachment.workspace_owner != WorkspaceOwner::Runtime {
        return None;
    }
    match attachment.sessions.first()?.provider.as_str() {
        "claude" => Some(claude::START_ADVICE),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
            ])
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
    }

    /// A repository at `<temp>/repo` with a linked worktree at `<temp>/wt`.
    fn project() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = fs::canonicalize(temp.path()).unwrap().join("repo");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        git(&repo, &["init", "--quiet"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "--quiet", "-m", "initial"]);
        git(&repo, &["worktree", "add", "--quiet", "-b", "wt", "../wt"]);
        let worktree = repo.with_file_name("wt");
        (temp, repo, worktree)
    }

    #[test]
    fn the_main_worktree_is_read_as_git_would_find_it_or_left_to_git() {
        let (temp, repo, worktree) = project();
        for path in [
            repo.clone(),
            repo.join("src"),
            worktree.clone(),
            worktree.join("src"),
        ] {
            let main = source::repo_identity(&path).unwrap().unwrap().main_worktree;
            assert_eq!(main_worktree(&path), Some(Some(main)), "{}", path.display());
        }
        let plain = fs::canonicalize(temp.path()).unwrap().join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(main_worktree(&plain), Some(None));

        // Shapes this does not read are Git's to answer.
        fs::create_dir_all(plain.join("fake/.git")).unwrap();
        assert_eq!(main_worktree(&plain.join("fake")), None, "not a repository");
        let separate = plain.join("separate");
        fs::create_dir_all(&separate).unwrap();
        git(
            &separate,
            &["init", "--quiet", "--separate-git-dir", "../separate.git"],
        );
        assert_eq!(main_worktree(&separate), None);
        git(&repo, &["config", "core.worktree", "/elsewhere"]);
        assert_eq!(main_worktree(&worktree), None);
    }

    #[test]
    fn a_session_outside_every_watched_project_is_decided_without_git() {
        let (temp, repo, worktree) = project();
        let state = State {
            root: temp.path().join("state"),
        };
        assert!(unwatched(&state, &worktree));
        let serving = background::acquire(&state, &repo).unwrap();
        assert!(!unwatched(&state, &worktree));
        assert!(!unwatched(&state, &repo.join("src")));
        let plain = temp.path().join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert!(unwatched(&state, &plain));
        drop(serving);

        // A configuration Git cannot read: Git fails, the files still say
        // nobody watches this project, and the session is left alone.
        fs::write(repo.join(".git/config"), "this is [not a config\n").unwrap();
        assert!(source::inspect_source(&worktree).is_err());
        assert!(unwatched(&state, &worktree));
        let event = RuntimeEvent {
            provider: "claude",
            session_id: "s1".into(),
            cwd: worktree.clone(),
            model: None,
            kind: EventKind::Start {
                source: "startup".into(),
                resumed: false,
            },
        };
        assert_eq!(ingest(&state, event).unwrap(), Reply::Silent);
        assert!(!state.db_path().exists(), "nothing opened the database");
    }
}
