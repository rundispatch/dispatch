//! Packet B: the landing observation, captured before a patch is applied and
//! recorded after it is (`docs/plan-0.4.9.md` §3.2). This file is pure; the
//! application path gathers the inputs and persists the result.

use super::{Landing, OwnerView};
use crate::{AppliedBy, RunRecord};

/// The `interaction.landed` payload for `landed`, about to apply `patch`.
/// `counterparts` are the runs the view lists besides `landed`, as far as they
/// could be read. `world_after` is left `None` for the caller to fill in.
pub fn capture(
    landed: &RunRecord,
    applied_by: AppliedBy,
    patch: &[u8],
    world_before: Option<&str>,
    view: OwnerView,
    counterparts: &[RunRecord],
) -> Landing {
    let _ = (landed, applied_by, patch, world_before, view, counterparts);
    unimplemented!("0.4.9 packet B")
}
