//! Packet B: the landing observation, captured before a patch is applied and
//! recorded after it is (`docs/plan-0.4.9.md` §3.2). This file is pure; the
//! application path gathers the inputs and persists the result.

use sha2::{Digest, Sha256};

use super::{Counterpart, Identity, Landed, Landing, LandingStatus, OwnerView, Prior, VERSION};
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
    let delta_sha256 = hex::encode(Sha256::digest(patch));
    let mut landing = Landing {
        version: VERSION,
        status: LandingStatus::Observed,
        detail: None,
        landed: Landed {
            identity: Identity {
                run_id: landed.id.clone(),
                s0: Some(landed.baseline_commit.clone()),
                delta_sha256: delta_sha256.clone(),
            },
            applied_by,
        },
        world_before: world_before.map(str::to_owned),
        world_after: None,
        projection_computed_at: None,
        counterparts: Vec::new(),
    };
    let projection = match view {
        OwnerView::Unwatched => {
            landing.status = LandingStatus::Unwatched;
            return landing;
        }
        OwnerView::Unavailable(why) => {
            landing.status = LandingStatus::Unavailable;
            landing.detail = Some(why);
            return landing;
        }
        OwnerView::Watched(projection) => projection,
    };
    landing.projection_computed_at = Some(projection.computed_at);
    let interactions = projection.of(&landed.id);
    landing.counterparts = projection
        .participants
        .iter()
        .filter(|participant| participant.run_id != landed.id)
        .map(|participant| {
            let run = counterparts.iter().find(|run| run.id == participant.run_id);
            Counterpart {
                identity: Identity {
                    run_id: participant.run_id.clone(),
                    s0: run.map(|run| run.baseline_commit.clone()),
                    delta_sha256: participant.delta_sha256.clone(),
                },
                analysis: participant.analysis,
                unresolved: participant.unresolved,
                prior: run
                    .and_then(|run| run.coherence.as_ref()?.validity.as_ref())
                    .map(|validity| Prior {
                        decision: validity.decision,
                        world_digest: validity.world_digest.clone(),
                    }),
                interactions: interactions
                    .iter()
                    .filter(|(other, _)| *other == participant.run_id)
                    .map(|(_, interaction)| interaction.clone())
                    .collect(),
            }
        })
        .collect();
    let listed = projection
        .participants
        .iter()
        .find(|participant| participant.run_id == landed.id);
    let stale = match listed {
        None => Some("the view does not list the landed run"),
        Some(participant) if participant.delta_sha256 != delta_sha256 => {
            Some("the view lists an older patch of the landed run")
        }
        Some(_) => None,
    };
    if let Some(why) = stale {
        landing.status = LandingStatus::Stale;
        landing.detail = Some(why.to_owned());
    }
    landing
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coherence::interactions::{Analysis, Participant, Projection};

    fn run(id: &str, prior: Option<&str>) -> RunRecord {
        let coherence = prior.map_or(serde_json::Value::Null, |decision| {
            serde_json::json!({"version": 1, "validity": {
                "decision": decision, "evaluated_at": "2026-10-06T00:00:00Z",
                "world_digest": "dd", "analysis": "symbols"}})
        });
        serde_json::from_value(serde_json::json!({
            "id": id, "task": "t", "exact_prompt": "t", "source_path": "/s",
            "source_kind": "directory", "source_git_head": null, "source_fingerprint": "f",
            "baseline_path": "/b", "baseline_commit": format!("s0-{id}"), "status": "evaluated",
            "created_at": "2026-10-06T00:00:00Z", "completed_at": null,
            "environment": {"dispatch_version": "test", "os": "test", "architecture": "test",
                "execution_backend": "local", "timeout_secs": 30, "cpus": 1.0,
                "memory": "1g", "max_parallel": 1},
            "evaluation": null, "applied_candidate": null, "coherence": coherence,
        }))
        .unwrap()
    }

    fn view(landed_delta: Option<&str>) -> OwnerView {
        let participant = |run_id: &str, delta: &str| Participant {
            run_id: run_id.into(),
            delta_sha256: delta.into(),
            analysis: Analysis::Analyzed,
            unresolved: 0,
        };
        let mut participants = vec![participant("B", "bb"), participant("C", "cc")];
        if let Some(delta) = landed_delta {
            participants.insert(1, participant("A", delta));
        }
        OwnerView::Watched(Projection {
            version: 1,
            computed_at: "2026-10-06T09:00:00Z".parse().unwrap(),
            participants,
            edges: Vec::new(),
        })
    }

    /// The view decides the status in §3.2's order, and every other
    /// participant is listed in the view's order, whether its run was read
    /// or not.
    #[test]
    fn capture_assigns_the_status_and_lists_every_counterpart() {
        let patch = b"--- a/x\n+++ b/x\n";
        let sha = hex::encode(Sha256::digest(patch));
        let landed = run("A", None);
        let others = [run("C", Some("refresh"))];
        let capture = |view| capture(&landed, AppliedBy::Human, patch, Some("w0"), view, &others);

        let observed = capture(view(Some(&sha)));
        assert_eq!(observed.status, LandingStatus::Observed);
        assert_eq!(observed.landed.identity.delta_sha256, sha);
        assert_eq!(observed.landed.identity.s0.as_deref(), Some("s0-A"));
        assert_eq!(observed.world_before.as_deref(), Some("w0"));
        assert_eq!(observed.world_after, None);
        let [b, c] = &observed.counterparts[..] else {
            panic!("{:?}", observed.counterparts);
        };
        assert_eq!(
            (b.identity.run_id.as_str(), c.identity.run_id.as_str()),
            ("B", "C")
        );
        // B's run could not be read; C's was, with its stored verdict.
        assert_eq!((&b.identity.s0, &b.prior), (&None, &None));
        assert_eq!(c.identity.s0.as_deref(), Some("s0-C"));
        assert_eq!(c.prior.as_ref().unwrap().decision, crate::Decision::Refresh);

        for stale in [view(None), view(Some("older"))] {
            let landing = capture(stale);
            assert_eq!(landing.status, LandingStatus::Stale);
            assert!(landing.detail.is_some());
            assert_eq!(landing.counterparts.len(), 2);
        }
        let unavailable = capture(OwnerView::Unavailable("unreadable".into()));
        assert_eq!(unavailable.status, LandingStatus::Unavailable);
        assert_eq!(unavailable.detail.as_deref(), Some("unreadable"));
        assert!(unavailable.counterparts.is_empty());
        let unwatched = capture(OwnerView::Unwatched);
        assert_eq!(unwatched.status, LandingStatus::Unwatched);
        assert!(unwatched.detail.is_none() && unwatched.counterparts.is_empty());
    }
}
