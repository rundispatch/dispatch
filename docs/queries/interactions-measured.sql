-- Interactions, measured (0.4.9): precision and misses of interactions.
--
-- Read-only. Run it against a Dispatch state database:
--
--   sqlite3 -readonly <state>/dispatch.db < docs/queries/interactions-measured.sql
--
-- Add `-header -column` for aligned output. Every row starts with its result
-- set's label. The events are described in `docs/plan-0.4.9.md` §3 and
-- `src/coherence/measure.rs`; what these numbers can and cannot show is in
-- `docs/coherence-validation.md`, "Measuring interactions (0.4.9)".
--
-- "Invalidated" means Dispatch's own next verdict on the counterpart was
-- REFRESH or STOP. It is not a confirmed conflict and not a human judgment.
-- Only `scorable` outcomes are scored. Ratios are NULL when their denominator
-- is zero, and every ratio comes with the counts it was made from. Outcomes
-- are grouped by `classifier_version` so that classifiers are never mixed.
-- Both events are recorded on the landed run, so joins also match on run_id.

-- 1. landings_by_status: every `interaction.landed`, by the status of the
--    owner's view at the landing, with the counterparts it listed.
SELECT 'landings_by_status' AS result_set,
       json_extract(payload_json, '$.status') AS status,
       COUNT(*) AS landings,
       SUM(json_array_length(payload_json, '$.counterparts')) AS counterparts
FROM events
WHERE event_type = 'interaction.landed'
GROUP BY status
ORDER BY status;

-- 2. outcomes_by_class: every `interaction.outcome`, by class. Only
--    `scorable` outcomes are used below.
SELECT 'outcomes_by_class' AS result_set,
       json_extract(payload_json, '$.classifier_version') AS classifier_version,
       json_extract(payload_json, '$.class') AS class,
       COUNT(*) AS outcomes
FROM events
WHERE event_type = 'interaction.outcome'
GROUP BY classifier_version, class
ORDER BY classifier_version, class;

-- 3. scorable_predicted_by_invalidated: the 2x2 of predicted (the landing
--    listed at least one interaction with the counterpart) by invalidated.
--    precision = predicted_invalidated / predicted;
--    misses = invalidated and not predicted.
WITH scorable AS (
    SELECT json_extract(payload_json, '$.classifier_version') AS classifier_version,
           json_extract(payload_json, '$.predicted') = 1 AS predicted,
           json_extract(payload_json, '$.decision') IN ('refresh', 'stop') AS invalidated
    FROM events
    WHERE event_type = 'interaction.outcome'
      AND json_extract(payload_json, '$.class') = 'scorable'
)
SELECT 'scorable_predicted_by_invalidated' AS result_set,
       classifier_version,
       COUNT(*) AS scorable,
       SUM(predicted AND invalidated) AS predicted_invalidated,
       SUM(predicted AND NOT invalidated) AS predicted_valid,
       SUM(NOT predicted AND invalidated) AS unpredicted_invalidated,
       SUM(NOT predicted AND NOT invalidated) AS unpredicted_valid,
       SUM(predicted) AS predicted,
       ROUND(1.0 * SUM(predicted AND invalidated) / NULLIF(SUM(predicted), 0), 3) AS precision,
       SUM(NOT predicted AND invalidated) AS misses
FROM scorable
GROUP BY classifier_version
ORDER BY classifier_version;

-- 4. scorable_predicted_by_rule: the predicted row of 3, by interaction rule.
--    Each scorable outcome is joined to its counterpart's interactions in the
--    landing. An outcome counts once per rule, so one with interactions under
--    two rules counts under both, and the rows do not add up to 3. Misses have
--    no rule, so they are only in 3.
WITH predicted AS (
    SELECT DISTINCT
           json_extract(outcome.payload_json, '$.classifier_version') AS classifier_version,
           json_extract(interaction.value, '$.rule') AS rule,
           outcome.id AS outcome_id,
           json_extract(outcome.payload_json, '$.decision') IN ('refresh', 'stop') AS invalidated
    FROM events AS outcome
    JOIN events AS landing
      ON landing.run_id = outcome.run_id
     AND landing.event_type = 'interaction.landed'
     AND json_extract(landing.payload_json, '$.landed.run_id')
         = json_extract(outcome.payload_json, '$.landed_run_id')
    JOIN json_each(landing.payload_json, '$.counterparts') AS counterpart
      ON json_extract(counterpart.value, '$.run_id')
         = json_extract(outcome.payload_json, '$.counterpart_run_id')
    JOIN json_each(counterpart.value, '$.interactions') AS interaction
    WHERE outcome.event_type = 'interaction.outcome'
      AND json_extract(outcome.payload_json, '$.class') = 'scorable'
)
SELECT 'scorable_predicted_by_rule' AS result_set,
       classifier_version,
       rule,
       COUNT(*) AS predicted,
       SUM(invalidated) AS predicted_invalidated,
       SUM(NOT invalidated) AS predicted_valid,
       ROUND(1.0 * SUM(invalidated) / COUNT(*), 3) AS precision
FROM predicted
GROUP BY classifier_version, rule
ORDER BY classifier_version, rule;

-- 5. landings_without_outcomes: landings that listed counterparts for which
--    no outcome is recorded, because no owner evaluated them afterwards (or
--    has not yet: the owner looks at landings from the last 24 hours). These
--    are unknown, not scored, and not misses.
WITH expected AS (
    SELECT landing.id AS landing_id,
           landing.timestamp AS landed_at,
           json_extract(landing.payload_json, '$.landed.run_id') AS landed_run_id,
           json_extract(landing.payload_json, '$.status') AS status,
           json_extract(counterpart.value, '$.run_id') AS counterpart_run_id,
           NOT EXISTS (
               SELECT 1
               FROM events AS outcome
               WHERE outcome.run_id = landing.run_id
                 AND outcome.event_type = 'interaction.outcome'
                 AND json_extract(outcome.payload_json, '$.landed_run_id')
                     = json_extract(landing.payload_json, '$.landed.run_id')
                 AND json_extract(outcome.payload_json, '$.counterpart_run_id')
                     = json_extract(counterpart.value, '$.run_id')
           ) AS missing
    FROM events AS landing,
         json_each(landing.payload_json, '$.counterparts') AS counterpart
    WHERE landing.event_type = 'interaction.landed'
)
SELECT 'landings_without_outcomes' AS result_set,
       landed_run_id,
       status,
       landed_at,
       COUNT(*) AS counterparts,
       SUM(missing) AS without_outcome,
       group_concat(CASE WHEN missing THEN counterpart_run_id END, ' ') AS missing_counterparts
FROM expected
GROUP BY landing_id
HAVING SUM(missing) > 0
ORDER BY landed_at, landing_id;
