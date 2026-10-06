//! Measuring interactions (0.4.9). When Work lands, Dispatch records what the
//! project owner had reported about how it interacted with the other Work not
//! yet integrated (`interaction.landed`). It then records Dispatch's own next
//! evaluation of each of those other pieces (`interaction.outcome`). Both are
//! events on the landed run.
//!
//! This is evidence, not causation: a later invalidation is not assumed to
//! come from the most recent landing. Whatever could confound it is kept with
//! the evidence, and the outcome's `class` says when it cannot be scored.
//! Nothing here changes a verdict, an apply, or any policy.
//!
//! The contract is fixed for 0.4.9. The canonical examples are in
//! `tests/fixtures/measurement/`, and the plan is `docs/plan-0.4.9.md` §3.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    AppliedBy, Decision, ReasonCode,
    coherence::interactions::{Analysis, Interaction, Projection},
};

pub mod landing;
pub mod outcome;

/// Recorded once on a landed run, after `result.applied`.
pub const LANDED: &str = "interaction.landed";
/// Recorded once per (landed run, counterpart), on the landed run.
pub const OUTCOME: &str = "interaction.outcome";
pub const VERSION: u32 = 1;
pub const CLASSIFIER_VERSION: u32 = 1;

/// What the owner's interaction view was when the Work landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LandingStatus {
    /// The view listed the landed run with the very patch being applied.
    Observed,
    /// The view was readable but did not list the landed run, or listed an
    /// older patch of it.
    Stale,
    /// No owner held the project.
    Unwatched,
    /// An owner held the project, but its view could not be read.
    Unavailable,
}

/// The view as the applying process found it, before applying. It is the
/// input of `landing::capture`.
#[derive(Clone, Debug)]
pub enum OwnerView {
    Unwatched,
    Unavailable(String),
    Watched(Projection),
}

/// Which Work, from which S0, with which Δ. `s0` is the run's baseline
/// commit, and `None` only when a counterpart's run could not be read.
/// `delta_sha256` is the SHA-256 of the exact patch bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub run_id: String,
    pub s0: Option<String>,
    pub delta_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Landed {
    #[serde(flatten)]
    pub identity: Identity,
    pub applied_by: AppliedBy,
}

/// A counterpart's stored verdict when the other Work landed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prior {
    pub decision: Decision,
    pub world_digest: String,
}

/// Another unintegrated piece of Work, as the view showed it at the landing.
/// `interactions` are seen from the landed run's side: `Side::A` is the
/// landed run. An empty list is a counterpart with no reported interaction,
/// which is what makes misses measurable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counterpart {
    #[serde(flatten)]
    pub identity: Identity,
    pub analysis: Analysis,
    pub unresolved: u32,
    pub prior: Option<Prior>,
    pub interactions: Vec<Interaction>,
}

/// The `interaction.landed` payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Landing {
    pub version: u32,
    pub status: LandingStatus,
    pub detail: Option<String>,
    pub landed: Landed,
    /// The world digest the accept gate evaluated, `None` when it produced no
    /// validity.
    pub world_before: Option<String>,
    /// The world digest right after applying, `None` if it could not be
    /// observed.
    pub world_after: Option<String>,
    pub projection_computed_at: Option<DateTime<Utc>>,
    /// Every other participant, interacting or not. Empty when unwatched or
    /// unavailable.
    pub counterparts: Vec<Counterpart>,
}

/// What the owner's first evaluation of a counterpart after a landing found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Evaluation {
    /// There was no Δ left to evaluate: it was applied, closed, lost or
    /// missing.
    Gone,
    Failed {
        error: String,
        world_digest: Option<String>,
        delta_sha256: Option<String>,
    },
    Evaluated {
        world_digest: String,
        delta_sha256: String,
        decision: Decision,
        reasons: Vec<ReasonCode>,
    },
}

/// Whether an outcome can be scored, and if not, why. It is decided by the
/// first rule that applies, in this order (see `outcome::classify`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    LandingUnobserved,
    CounterpartGone,
    EvaluationFailed,
    CounterpartUnanalyzed,
    AlreadyInvalid,
    WorldMovedOn,
    CounterpartMoved,
    Scorable,
}

/// The `interaction.outcome` payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub version: u32,
    pub classifier_version: u32,
    pub landed_run_id: String,
    pub counterpart_run_id: String,
    pub evaluated_at: DateTime<Utc>,
    pub world_evaluated: Option<String>,
    pub counterpart_delta_sha256: Option<String>,
    pub decision: Option<Decision>,
    pub reasons: Vec<ReasonCode>,
    pub error: Option<String>,
    /// The landing listed at least one interaction with this counterpart.
    pub predicted: bool,
    pub class: Class,
}

#[cfg(test)]
mod tests {
    use super::*;

    const LANDINGS: [&str; 5] = [
        include_str!("../../tests/fixtures/measurement/landed-observed.json"),
        include_str!("../../tests/fixtures/measurement/landed-observed-no-world.json"),
        include_str!("../../tests/fixtures/measurement/landed-stale.json"),
        include_str!("../../tests/fixtures/measurement/landed-unwatched.json"),
        include_str!("../../tests/fixtures/measurement/landed-unavailable.json"),
    ];

    /// The fixtures every packet builds on are valid contract values, and
    /// survive a round trip unchanged.
    #[test]
    fn the_fixtures_are_valid_contract_values() {
        for text in LANDINGS {
            let landing: Landing = serde_json::from_str(text).unwrap();
            let again: Landing =
                serde_json::from_value(serde_json::to_value(&landing).unwrap()).unwrap();
            assert_eq!(landing, again);
            assert_eq!(landing.version, VERSION);
        }
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../tests/fixtures/measurement/cases.json"))
                .unwrap();
        for case in &cases {
            serde_json::from_value::<Evaluation>(case["evaluation"].clone()).unwrap();
            serde_json::from_value::<Class>(case["expected"]["class"].clone()).unwrap();
        }
        for line in include_str!("../../tests/fixtures/measurement/events.jsonl").lines() {
            let event: serde_json::Value = serde_json::from_str(line).unwrap();
            let payload = event["payload"].clone();
            match event["event_type"].as_str().unwrap() {
                LANDED => drop(serde_json::from_value::<Landing>(payload).unwrap()),
                OUTCOME => drop(serde_json::from_value::<Outcome>(payload).unwrap()),
                other => panic!("unexpected event type {other}"),
            }
        }
    }
}
