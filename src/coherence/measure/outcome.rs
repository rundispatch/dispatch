//! Packet C: classifying the first evaluation of a counterpart after a landing
//! (`docs/plan-0.4.9.md` §3.3). Pure and deterministic.

use chrono::{DateTime, Utc};

use super::{Class, Counterpart, Evaluation, Landing, Outcome};

/// The class of `evaluation`, by the first rule of §3.3 that applies.
pub fn classify(landing: &Landing, counterpart: &Counterpart, evaluation: &Evaluation) -> Class {
    let _ = (landing, counterpart, evaluation);
    unimplemented!("0.4.9 packet C")
}

/// The `interaction.outcome` payload for `counterpart` of `landing`.
pub fn outcome(
    landing: &Landing,
    counterpart: &Counterpart,
    evaluation: Evaluation,
    now: DateTime<Utc>,
) -> Outcome {
    let _ = (landing, counterpart, evaluation, now);
    unimplemented!("0.4.9 packet C")
}
