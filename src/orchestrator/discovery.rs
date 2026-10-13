//! Worktrees a runtime made, found in a watched project without hooks
//! (`docs/plan-0.4.13.md` §3 B). P0 gives the inventory and the owner's call;
//! packet P1 owns the rest of this file.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{RunRecord, state::State};

/// Worktrees a runtime made that were already there when this watch began:
/// listed with an attach command, never turned into Work by themselves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    pub version: u32,
    pub entries: Vec<InventoryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryEntry {
    pub path: PathBuf,
    pub first_seen: DateTime<Utc>,
    /// What the runtime recorded it branched from (Claude's `CLAUDE_BASE`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// The worktree's lock reason when first seen, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<String>,
}

impl Inventory {
    /// The inventory of the watch of `root`, or an empty one.
    pub fn load(state: &State, root: &Path) -> Inventory {
        std::fs::read(path(state, root))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }
}

/// Beside the watcher's record: `watchers/<key>.inventory.json`.
pub(crate) fn path(state: &State, root: &Path) -> PathBuf {
    super::background::watcher_file(state, root, "inventory.json")
}

/// One owner tick for `root`, with the runs the owner loaded.
pub(crate) fn scan(_state: &State, _root: &Path, _runs: &[RunRecord]) -> Result<()> {
    Ok(())
}
