//! Packet C: classifying the first evaluation of a counterpart after a landing
//! (`docs/plan-0.4.9.md` §3.3). Pure and deterministic.

use chrono::{DateTime, Utc};

use super::{
    CLASSIFIER_VERSION, Class, Counterpart, Evaluation, Landing, LandingStatus, Outcome, VERSION,
};
use crate::{Decision, coherence::interactions::Analysis};

/// The class of `evaluation`, by the first rule of §3.3 that applies.
pub fn classify(landing: &Landing, counterpart: &Counterpart, evaluation: &Evaluation) -> Class {
    if landing.status != LandingStatus::Observed {
        return Class::LandingUnobserved;
    }
    let (world_digest, delta_sha256) = match evaluation {
        Evaluation::Gone => return Class::CounterpartGone,
        Evaluation::Failed { .. } => return Class::EvaluationFailed,
        Evaluation::Evaluated {
            world_digest,
            delta_sha256,
            ..
        } => (world_digest, delta_sha256),
    };
    if counterpart.analysis == Analysis::Pending {
        return Class::CounterpartUnanalyzed;
    }
    if counterpart
        .prior
        .as_ref()
        .is_some_and(|prior| matches!(prior.decision, Decision::Refresh | Decision::Stop))
    {
        return Class::AlreadyInvalid;
    }
    if landing.world_after.as_ref() != Some(world_digest) {
        return Class::WorldMovedOn;
    }
    if *delta_sha256 != counterpart.identity.delta_sha256 {
        return Class::CounterpartMoved;
    }
    Class::Scorable
}

/// The `interaction.outcome` payload for `counterpart` of `landing`.
pub fn outcome(
    landing: &Landing,
    counterpart: &Counterpart,
    evaluation: Evaluation,
    now: DateTime<Utc>,
) -> Outcome {
    let class = classify(landing, counterpart, &evaluation);
    let (world_evaluated, counterpart_delta_sha256, decision, reasons, error) = match evaluation {
        Evaluation::Gone => (None, None, None, Vec::new(), None),
        Evaluation::Failed {
            error,
            world_digest,
            delta_sha256,
        } => (world_digest, delta_sha256, None, Vec::new(), Some(error)),
        Evaluation::Evaluated {
            world_digest,
            delta_sha256,
            decision,
            reasons,
        } => (
            Some(world_digest),
            Some(delta_sha256),
            Some(decision),
            reasons,
            None,
        ),
    };
    Outcome {
        version: VERSION,
        classifier_version: CLASSIFIER_VERSION,
        landed_run_id: landing.landed.identity.run_id.clone(),
        counterpart_run_id: counterpart.identity.run_id.clone(),
        evaluated_at: now,
        world_evaluated,
        counterpart_delta_sha256,
        decision,
        reasons,
        error,
        predicted: !counterpart.interactions.is_empty(),
        class,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ReasonCode, coherence::measure::Prior};

    fn fixture(name: &str) -> Landing {
        let text = match name {
            "landed-observed.json" => {
                include_str!("../../../tests/fixtures/measurement/landed-observed.json")
            }
            "landed-observed-no-world.json" => {
                include_str!("../../../tests/fixtures/measurement/landed-observed-no-world.json")
            }
            "landed-stale.json" => {
                include_str!("../../../tests/fixtures/measurement/landed-stale.json")
            }
            "landed-unwatched.json" => {
                include_str!("../../../tests/fixtures/measurement/landed-unwatched.json")
            }
            "landed-unavailable.json" => {
                include_str!("../../../tests/fixtures/measurement/landed-unavailable.json")
            }
            other => panic!("unknown landing fixture {other}"),
        };
        serde_json::from_str(text).unwrap()
    }

    fn observed() -> Landing {
        fixture("landed-observed.json")
    }

    /// Counterpart `index` of the observed landing.
    fn counterpart(index: usize) -> Counterpart {
        observed().counterparts[index].clone()
    }

    /// An evaluation of `counterpart` in the observed landing's world, with
    /// its Δ as it was at the landing: scorable unless something else rules.
    fn evaluated(counterpart: &Counterpart, decision: Decision) -> Evaluation {
        Evaluation::Evaluated {
            world_digest: observed().world_after.unwrap(),
            delta_sha256: counterpart.identity.delta_sha256.clone(),
            decision,
            reasons: Vec::new(),
        }
    }

    fn failed() -> Evaluation {
        Evaluation::Failed {
            error: "baseline unreadable".into(),
            world_digest: None,
            delta_sha256: None,
        }
    }

    #[test]
    fn every_fixture_case_gives_the_expected_class_and_prediction() {
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../../tests/fixtures/measurement/cases.json"
        ))
        .unwrap();
        assert_eq!(cases.len(), 16);
        for case in &cases {
            let name = case["name"].as_str().unwrap();
            let landing = fixture(case["landing"].as_str().unwrap());
            let counterpart = &landing.counterparts[case["counterpart"].as_u64().unwrap() as usize];
            let evaluation: Evaluation =
                serde_json::from_value(case["evaluation"].clone()).unwrap();
            let class: Class = serde_json::from_value(case["expected"]["class"].clone()).unwrap();
            let predicted = case["expected"]["predicted"].as_bool().unwrap();

            assert_eq!(
                classify(&landing, counterpart, &evaluation),
                class,
                "{name}"
            );
            let outcome = outcome(&landing, counterpart, evaluation, Utc::now());
            assert_eq!(outcome.class, class, "{name}");
            assert_eq!(outcome.predicted, predicted, "{name}");
        }
    }

    #[test]
    fn landing_unobserved_wins_over_every_evaluation() {
        let counterpart = counterpart(0);
        for status in [
            LandingStatus::Stale,
            LandingStatus::Unwatched,
            LandingStatus::Unavailable,
        ] {
            let landing = Landing {
                status,
                ..observed()
            };
            for evaluation in [
                Evaluation::Gone,
                failed(),
                evaluated(&counterpart, Decision::Refresh),
            ] {
                assert_eq!(
                    classify(&landing, &counterpart, &evaluation),
                    Class::LandingUnobserved,
                    "{status:?} {evaluation:?}"
                );
            }
        }
    }

    #[test]
    fn a_gone_or_failed_evaluation_wins_over_a_pending_counterpart() {
        let pending = counterpart(2);
        assert_eq!(pending.analysis, Analysis::Pending);
        let landing = observed();
        assert_eq!(
            classify(&landing, &pending, &Evaluation::Gone),
            Class::CounterpartGone
        );
        assert_eq!(
            classify(&landing, &pending, &failed()),
            Class::EvaluationFailed
        );
        assert_eq!(
            classify(&landing, &pending, &evaluated(&pending, Decision::Refresh)),
            Class::CounterpartUnanalyzed
        );
    }

    #[test]
    fn a_failed_evaluation_with_digests_is_still_a_failure() {
        let counterpart = counterpart(0);
        let evaluation = Evaluation::Failed {
            error: "verify crashed".into(),
            world_digest: observed().world_after,
            delta_sha256: Some(counterpart.identity.delta_sha256.clone()),
        };
        assert_eq!(
            classify(&observed(), &counterpart, &evaluation),
            Class::EvaluationFailed
        );
    }

    #[test]
    fn counterpart_unanalyzed_wins_over_already_invalid() {
        let counterpart = Counterpart {
            analysis: Analysis::Pending,
            ..counterpart(3)
        };
        let evaluation = evaluated(&counterpart, Decision::Refresh);
        assert_eq!(
            classify(&observed(), &counterpart, &evaluation),
            Class::CounterpartUnanalyzed
        );
    }

    #[test]
    fn already_invalid_wins_over_world_moved_on_and_counterpart_moved() {
        let landing = Landing {
            world_after: None,
            ..observed()
        };
        for decision in [Decision::Refresh, Decision::Stop] {
            let counterpart = Counterpart {
                prior: Some(Prior {
                    decision,
                    world_digest: "c".repeat(64),
                }),
                ..counterpart(0)
            };
            let evaluation = Evaluation::Evaluated {
                world_digest: "f".repeat(64),
                delta_sha256: "9".repeat(64),
                decision: Decision::Continue,
                reasons: Vec::new(),
            };
            assert_eq!(
                classify(&landing, &counterpart, &evaluation),
                Class::AlreadyInvalid,
                "{decision:?}"
            );
        }
    }

    #[test]
    fn a_prior_continue_or_no_prior_is_not_already_invalid() {
        let landing = observed();
        let with_prior = counterpart(0);
        assert_eq!(
            with_prior.prior.as_ref().unwrap().decision,
            Decision::Continue
        );
        let without_prior = Counterpart {
            prior: None,
            ..counterpart(0)
        };
        for counterpart in [with_prior, without_prior] {
            let evaluation = evaluated(&counterpart, Decision::Refresh);
            assert_eq!(
                classify(&landing, &counterpart, &evaluation),
                Class::Scorable
            );
        }
    }

    #[test]
    fn world_moved_on_wins_over_counterpart_moved() {
        let counterpart = counterpart(0);
        let evaluation = Evaluation::Evaluated {
            world_digest: "f".repeat(64),
            delta_sha256: "9".repeat(64),
            decision: Decision::Refresh,
            reasons: Vec::new(),
        };
        assert_eq!(
            classify(&observed(), &counterpart, &evaluation),
            Class::WorldMovedOn
        );
    }

    #[test]
    fn an_unknown_world_after_is_world_moved_on_even_with_a_matching_digest() {
        let counterpart = counterpart(0);
        let evaluation = evaluated(&counterpart, Decision::Continue);
        let landing = Landing {
            world_after: None,
            ..observed()
        };
        assert_eq!(
            classify(&landing, &counterpart, &evaluation),
            Class::WorldMovedOn
        );
    }

    #[test]
    fn counterpart_moved_wins_over_scorable() {
        let counterpart = counterpart(1);
        let mut evaluation = evaluated(&counterpart, Decision::Continue);
        assert_eq!(
            classify(&observed(), &counterpart, &evaluation),
            Class::Scorable
        );
        if let Evaluation::Evaluated { delta_sha256, .. } = &mut evaluation {
            *delta_sha256 = "9".repeat(64);
        }
        assert_eq!(
            classify(&observed(), &counterpart, &evaluation),
            Class::CounterpartMoved
        );
    }

    #[test]
    fn a_last_seen_counterpart_is_scorable_for_that_alone() {
        let last_seen = counterpart(4);
        assert_eq!(last_seen.analysis, Analysis::LastSeen);
        assert!(last_seen.unresolved > 0);
        assert!(last_seen.prior.is_none());
        let evaluation = evaluated(&last_seen, Decision::Refresh);
        assert_eq!(
            classify(&observed(), &last_seen, &evaluation),
            Class::Scorable
        );
    }

    #[test]
    fn predicted_is_whether_the_counterpart_has_any_interaction() {
        let landing = observed();
        for counterpart in &landing.counterparts {
            let evaluation = evaluated(counterpart, Decision::Continue);
            let outcome = outcome(&landing, counterpart, evaluation, Utc::now());
            assert_eq!(outcome.predicted, !counterpart.interactions.is_empty());
        }
        let none = Counterpart {
            interactions: Vec::new(),
            ..counterpart(0)
        };
        let evaluation = evaluated(&none, Decision::Refresh);
        assert!(!outcome(&landing, &none, evaluation, Utc::now()).predicted);
    }

    #[test]
    fn outcome_fills_every_field_from_its_inputs() {
        let landing = observed();
        let counterpart = counterpart(0);
        let now = DateTime::parse_from_rfc3339("2026-10-06T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let evaluated = outcome(
            &landing,
            &counterpart,
            Evaluation::Evaluated {
                world_digest: "e".repeat(64),
                delta_sha256: "2".repeat(64),
                decision: Decision::Refresh,
                reasons: vec![ReasonCode::FactBroken],
            },
            now,
        );
        assert_eq!(
            evaluated,
            Outcome {
                version: VERSION,
                classifier_version: CLASSIFIER_VERSION,
                landed_run_id: "01J9LANDA10000000000000000".into(),
                counterpart_run_id: "01J9CPB0000000000000000000".into(),
                evaluated_at: now,
                world_evaluated: Some("e".repeat(64)),
                counterpart_delta_sha256: Some("2".repeat(64)),
                decision: Some(Decision::Refresh),
                reasons: vec![ReasonCode::FactBroken],
                error: None,
                predicted: true,
                class: Class::Scorable,
            }
        );

        let failed = outcome(
            &landing,
            &counterpart,
            Evaluation::Failed {
                error: "baseline unreadable".into(),
                world_digest: Some("e".repeat(64)),
                delta_sha256: None,
            },
            now,
        );
        assert_eq!(failed.world_evaluated, Some("e".repeat(64)));
        assert_eq!(failed.counterpart_delta_sha256, None);
        assert_eq!(failed.decision, None);
        assert!(failed.reasons.is_empty());
        assert_eq!(failed.error.as_deref(), Some("baseline unreadable"));
        assert_eq!(failed.class, Class::EvaluationFailed);
        assert_eq!(failed.evaluated_at, now);

        let gone = outcome(&landing, &counterpart, Evaluation::Gone, now);
        assert_eq!(
            (
                gone.world_evaluated,
                gone.counterpart_delta_sha256,
                gone.decision,
                gone.reasons,
                gone.error,
                gone.class,
            ),
            (None, None, None, Vec::new(), None, Class::CounterpartGone)
        );
    }

    #[test]
    fn outcome_survives_a_json_round_trip() {
        let landing = observed();
        let counterpart = counterpart(0);
        for evaluation in [
            Evaluation::Gone,
            Evaluation::Failed {
                error: "baseline unreadable".into(),
                world_digest: Some("e".repeat(64)),
                delta_sha256: Some("2".repeat(64)),
            },
            Evaluation::Evaluated {
                world_digest: "e".repeat(64),
                delta_sha256: "2".repeat(64),
                decision: Decision::Stop,
                reasons: vec![ReasonCode::AlreadyApplied],
            },
        ] {
            let outcome = outcome(&landing, &counterpart, evaluation, Utc::now());
            let text = serde_json::to_string(&outcome).unwrap();
            let again: Outcome = serde_json::from_str(&text).unwrap();
            assert_eq!(outcome, again);
        }
    }
}
