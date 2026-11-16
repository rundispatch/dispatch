-- Batches, measured (0.5.0): what each batch held, the merged checks that
-- verified it, and the suite runs and approvals per landed Work.
--
-- It only reads. Run it against a Dispatch state database:
--
--   sqlite3 <state>/dispatch.db < docs/queries/batches-measured.sql
--
-- Every statement only reads. Do not add -readonly: the database uses a
-- write-ahead log, and a read-only connection fails when no process has the
-- database open, because it cannot create the log's shared-memory file.
--
-- Add `-header -column` for aligned output. Every row starts with its result
-- set's label. A batch is named by its digest, and each of its events is
-- recorded alike on every Work it names:
--
-- - `batch.proposed` {batch, members, left_out}: an offer, on each member
--   and each result left out. An offer made twice is recorded twice;
-- - `batch.verified` {batch, tree, passed, runs, lands, failed}: the merged
--   checks on the batch at accept. `runs` counts those that ran rather than
--   reuse a pass: first on the whole batch, then any bisecting it;
-- - `batch.landed` {batch, members}: the prefix that landed;
-- - `result.applied` and `review.accepted` name the batch on each member.
--
-- These are counts as recorded, not quality, time saved or effort: no event
-- records a step done by hand. "Followed by" means recorded later, not
-- caused by, and nothing here agrees or disagrees with a verdict.

-- 1. batch_members: each Work a batch proposed (in order, with why there),
--    left out (with the verdict and reasons), and landed, once however many
--    offers recorded it. A batch landed with `accept --expect` alone has no
--    proposal recorded.
WITH members AS (
    SELECT json_extract(event.payload_json, '$.batch') AS batch,
           'proposed' AS kind, json_extract(member.value, '$.run_id') AS run_id,
           member.key + 1 AS position, NULL AS decision,
           json_extract(member.value, '$.why') AS why
    FROM events AS event, json_each(event.payload_json, '$.members') AS member
    WHERE event.event_type = 'batch.proposed'
    UNION
    SELECT json_extract(event.payload_json, '$.batch'), 'left_out',
           json_extract(member.value, '$.run_id'), NULL,
           json_extract(member.value, '$.decision'),
           json_extract(member.value, '$.reasons')
    FROM events AS event, json_each(event.payload_json, '$.left_out') AS member
    WHERE event.event_type = 'batch.proposed'
    UNION
    SELECT json_extract(event.payload_json, '$.batch'), 'landed',
           member.value, member.key + 1, NULL, NULL
    FROM events AS event, json_each(event.payload_json, '$.members') AS member
    WHERE event.event_type = 'batch.landed'
)
SELECT 'batch_members' AS result_set, batch, kind, run_id, position, decision, why
FROM members
ORDER BY batch, CASE kind WHEN 'proposed' THEN 0 WHEN 'left_out' THEN 1 ELSE 2 END,
         position, run_id, decision, why;

-- 2. batch_check_runs: each verification of a batch, its copies on the
--    members counted once: the merged-check runs on the whole batch (0 when
--    a recorded pass was reused) and those bisecting it once it failed.
--    `verifications` counts accepts that verified it with the same outcome.
--    `passed` is NULL when the merged checks did not run.
SELECT 'batch_check_runs' AS result_set,
       json_extract(payload_json, '$.batch') AS batch,
       json_extract(payload_json, '$.passed') AS passed,
       json_extract(payload_json, '$.lands') AS lands,
       json_extract(payload_json, '$.failed') AS failed,
       COUNT(*) / COUNT(DISTINCT run_id) AS verifications,
       COUNT(*) / COUNT(DISTINCT run_id)
           * MIN(json_extract(payload_json, '$.runs'), 1) AS full_runs,
       COUNT(*) / COUNT(DISTINCT run_id)
           * MAX(json_extract(payload_json, '$.runs') - 1, 0) AS bisection_runs
FROM events
WHERE event_type = 'batch.verified'
GROUP BY payload_json
ORDER BY MIN(timestamp), MIN(id);

-- 3. landing_counts: the Work landed (`result.applied`), of it in batches,
--    and the batches. The approvals: `review.accepted` is recorded only once
--    Work landed, on each member of a batch, which is one approval; an
--    auto-apply is none. Then every suite run of any Work, landed or not, as
--    0.4.17 and 0.4.18 counted them, and the reuses of a recorded pass,
--    which run nothing. A suite run is:
--    - `coherence.merged_checks` that ran (one that could not build its
--      tree records no `passed`), by finish, check --merged or accept;
--    - a native run's baseline or verify phase (`check.started`, ordinal 1);
--    - an attached finish that checked Δ in its workspace, which records no
--      event of its own: its `checks` rows, one per phase, unless they are
--      in the logs of a merged run or reuse recorded on that Work. The table
--      keeps a Work's last checks, so an earlier finish of it is missed.
--    A batch's record of its pass on its later members is not a reuse.
--    No row until some Work landed.
SELECT 'landing_counts' AS result_set, *
FROM (
    SELECT (SELECT COUNT(DISTINCT run_id) FROM events
            WHERE event_type = 'result.applied') AS landed_works,
           (SELECT COUNT(DISTINCT run_id) FROM events
            WHERE event_type = 'result.applied'
              AND json_extract(payload_json, '$.batch') IS NOT NULL) AS in_batches,
           (SELECT COUNT(DISTINCT json_extract(payload_json, '$.batch')) FROM events
            WHERE event_type = 'result.applied') AS batches,
           (SELECT COUNT(DISTINCT COALESCE(json_extract(payload_json, '$.batch'), id))
            FROM events WHERE event_type = 'review.accepted') AS approvals,
           (SELECT COUNT(*) FROM events
            WHERE event_type = 'coherence.merged_checks'
              AND json_extract(payload_json, '$.passed') IS NOT NULL)
         + (SELECT COUNT(*) FROM events
            WHERE event_type = 'check.started'
              AND json_extract(payload_json, '$.ordinal') = 1)
         + (SELECT COUNT(DISTINCT checks.run_id || ' ' || checks.phase) FROM checks
            WHERE NOT EXISTS (
                      SELECT 1 FROM events AS started
                      WHERE started.run_id = checks.run_id
                        AND started.event_type = 'check.started')
              AND NOT EXISTS (
                      SELECT 1 FROM events AS recorded
                      WHERE recorded.run_id = checks.run_id
                        AND recorded.event_type IN ('coherence.merged_checks', 'checks.reused')
                        AND instr(checks.stdout_path,
                                  json_extract(recorded.payload_json, '$.logs') || '/') = 1))
           AS suite_runs,
           (SELECT COUNT(*) FROM events
            WHERE event_type = 'checks.reused'
              AND NOT (json_extract(payload_json, '$.batch') IS NOT NULL
                       AND json_extract(payload_json, '$.members') IS NULL)) AS reuses
)
WHERE landed_works > 0;
