//! Whether the agent of attached Work still runs, from what its runtime
//! records (`docs/plan-0.4.13.md` §3 C). P0 gives the owner's call; packet P2
//! owns the rest of this file.

use std::path::Path;

use anyhow::Result;

use crate::{RunRecord, state::State};

/// One owner tick for `root`, with the runs the owner loaded.
pub(crate) fn observe(_state: &State, _root: &Path, _runs: &[RunRecord]) -> Result<()> {
    Ok(())
}
