//! Registering a runtime session as Work, without silent gaps (0.4.10).
//! Contract: `docs/plan-0.4.10.md` §3. Owned by packet A after P0; the
//! integrator does not edit this file while A owns it.
//!
//! An eligible session (its own worktree, in a watched project) ends in exactly
//! one of:
//! - **registered**: its Work was published (record and `runs/<id>/`) before the
//!   hook returned;
//! - **pending**: its S0 is durable in `registrations/<id>/registration.json`
//!   but the Work is not yet published; the hook or the project owner
//!   publishes it;
//! - **untracked**: no durable S0 by the deadline. The session is told so, and
//!   nothing can publish it afterwards.
//!
//! Work is assembled in `<state>/registrations/<id>/`, never in `runs/`, and
//! published by a database insert plus a rename into `runs/<id>/`. Publishing is
//! idempotent. The single `decision` file in the registration directory is
//! created exclusively (`create_new`), so the first writer wins: the worker
//! writes `publishing` (only after `registration.json` is durable), the deadline
//! writes `pending` or `untracked`. Whoever loses must not contradict it.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{RuntimeSession, state::State};

/// The whole `SessionStart` path, from the hook process starting, must decide
/// within this. `dispatch setup` installs `SessionStart` with a 60 s timeout;
/// the margin covers process start and the reply under load.
pub const BUDGET: Duration = Duration::from_secs(40);

/// Under the state root: one directory per registration in progress.
pub const DIRECTORY: &str = "registrations";
/// The durable S0 boundary: once this exists, the session's Work can always be
/// published.
pub const RECORD: &str = "registration.json";
/// Created exclusively; holds a `Decision`.
pub const DECISION: &str = "decision";

pub const NOTICE_UNTRACKED: &str = "Dispatch could not capture this session's starting state in \
     time. This session is not being tracked.";

/// What a pending session is told; `id` is the short Work id.
pub fn notice_pending(id: &str) -> String {
    format!(
        "Dispatch captured this session's starting state and will finish tracking it as \
         Work {id} shortly; see it with dispatch watch."
    )
}

/// Everything needed to publish the Work later, from S0 alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub version: u32,
    pub run_id: String,
    pub workspace: PathBuf,
    pub root: PathBuf,
    /// The commit `source::world_commit` made of the workspace: S0.
    pub s0_commit: String,
    pub session: RuntimeSession,
    pub resumed: bool,
    /// Valid project check consent when the session started.
    pub consented: bool,
    /// When the hook process started: registration latency is measured from it.
    pub started_at: DateTime<Utc>,
}

/// The content of the `decision` file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// The worker, after `registration.json` was durable, about to publish.
    Publishing,
    /// The deadline, with a durable S0 and nothing published: publish later.
    Pending,
    /// The deadline, without a durable S0: never publish.
    Untracked,
}

/// What a `SessionStart` ended in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Registered { run_id: String },
    Pending { run_id: String },
    Untracked,
}

/// The directory of registration `run_id`.
pub fn directory(state: &State, run_id: &str) -> PathBuf {
    state.root.join(DIRECTORY).join(run_id)
}

/// Called by the project owner on every tick: publish pending registrations of
/// `root` whose S0 is durable, and remove those known to have failed
/// (`docs/plan-0.4.10.md` §3.5). Never deletes ambiguous state.
pub fn reconcile(state: &State, root: &Path) -> Result<()> {
    let _ = (state, root);
    Ok(())
}
