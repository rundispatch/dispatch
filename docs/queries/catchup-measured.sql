-- Catch-up, measured (0.5.0): the asks to catch up made to live agents, and
-- what followed each one delivered.
--
-- It only reads. Run it against a Dispatch state database:
--
--   sqlite3 <state>/dispatch.db < docs/queries/catchup-measured.sql
--
-- Every statement only reads. Do not add -readonly: the database uses a
-- write-ahead log, and a read-only connection fails when no process has the
-- database open, because it cannot create the log's shared-memory file.
--
-- Add `-header -column` for aligned output. Every row starts with its result
-- set's label. All events here are recorded on the Work asked:
--
-- - `catchup.written` {landings, targets, text}: the owner added these
--   landings to the Work's pending ask. An ask has no id: later writes join
--   it until a hook takes it;
-- - `catchup.delivered` {via, landings}: a hook gave the agent the ask, with
--   every landing it gathered, at that time;
-- - `catchup.expired` {why, landings}: the ask was no longer needed first;
-- - `attach.s0_moved` {proof, landed_run, recorded_at}: the Work's S0
--   followed a source it integrated. A move forward's proof is "ancestor of
--   workspace HEAD"; a move back's is "workspace HEAD no longer descends
--   from S0", and it never counts here;
-- - any event whose payload has `coherence.decision` is a verdict of
--   Dispatch's on the Work, as in `notes-measured.sql`.
--
-- A source holds a landing when it was recorded no earlier than the source
-- that landing recorded. No event records when that was, so landings are
-- compared by their `result.applied`: each records its source under the
-- source lock, an accept just after its `result.applied` and a batch just
-- before its members', and a move to a batch's source names its last member.
-- A move to the root's clean HEAD names no landed run: that source is
-- recorded as S0 moves, after every landing.
--
-- "Followed by" means recorded later, not caused by. The next verdict is
-- Dispatch's own, not a human judgment; it is the only agreement measured.
-- Nothing here shows whether an agent was relaunched, or time saved.

-- 1. catchup_counts: asks written, delivered by `via` and expired by `why`,
--    each with the landings they name. For writes, `repeated_landings`
--    counts a (Work, landing) written again, and the targets are counted by
--    kind: a file-level target should never be asked about.
WITH counted AS (
    SELECT event_type,
           CASE event_type
               WHEN 'catchup.delivered' THEN json_extract(payload_json, '$.via')
               WHEN 'catchup.expired' THEN json_extract(payload_json, '$.why')
           END AS detail,
           json_array_length(payload_json, '$.landings') AS landings,
           (SELECT COUNT(*) FROM json_each(payload_json, '$.targets')
            WHERE json_extract(value, '$.kind') = 'symbol') AS symbols,
           (SELECT COUNT(*) FROM json_each(payload_json, '$.targets')
            WHERE json_extract(value, '$.kind') = 'file') AS files
    FROM events
    WHERE event_type IN ('catchup.written', 'catchup.delivered', 'catchup.expired')
), repeated AS (
    SELECT COUNT(*) - 1 AS extra
    FROM events AS written, json_each(written.payload_json, '$.landings') AS landing
    WHERE written.event_type = 'catchup.written'
    GROUP BY written.run_id, landing.value
)
SELECT 'catchup_counts' AS result_set,
       substr(event_type, 9) AS kind, detail, COUNT(*) AS events,
       SUM(landings) AS landings,
       CASE WHEN event_type = 'catchup.written'
            THEN (SELECT SUM(extra) FROM repeated) END AS repeated_landings,
       CASE WHEN event_type = 'catchup.written' THEN SUM(symbols) END AS symbol_targets,
       CASE WHEN event_type = 'catchup.written' THEN SUM(files) END AS file_targets
FROM counted
GROUP BY event_type, detail
ORDER BY CASE event_type WHEN 'catchup.written' THEN 0
                         WHEN 'catchup.delivered' THEN 1 ELSE 2 END, detail;

-- 2. catchup_s0: one row per delivered ask: when its Work's S0 then first
--    moved forward to a source holding every landing the ask names, how
--    many whole seconds after the delivery, and Dispatch's first verdict on
--    the Work after that move. NULL until each was recorded.
WITH delivered AS (
    SELECT delivery.id, delivery.run_id, delivery.timestamp, delivery.payload_json,
           (SELECT moved.id
            FROM events AS moved
            WHERE moved.run_id = delivery.run_id
              AND moved.event_type = 'attach.s0_moved'
              AND (moved.timestamp, moved.id) > (delivery.timestamp, delivery.id)
              AND json_extract(moved.payload_json, '$.proof') = 'ancestor of workspace HEAD'
              AND NOT EXISTS (
                  SELECT 1 FROM json_each(delivery.payload_json, '$.landings') AS asked
                  WHERE json_extract(moved.payload_json, '$.landed_run') IS NOT NULL
                    AND NOT EXISTS (
                        SELECT 1 FROM events AS needed, events AS source
                        WHERE needed.run_id = asked.value
                          AND needed.event_type = 'result.applied'
                          AND source.run_id = json_extract(moved.payload_json, '$.landed_run')
                          AND source.event_type = 'result.applied'
                          AND (needed.timestamp, needed.id) <= (source.timestamp, source.id)
                    )
              )
            ORDER BY moved.timestamp, moved.id
            LIMIT 1) AS move_id
    FROM events AS delivery
    WHERE delivery.event_type = 'catchup.delivered'
), followed AS (
    SELECT delivered.*, moved.timestamp AS moved_at,
           (SELECT json_extract(verdict.payload_json, '$.coherence.decision')
            FROM events AS verdict
            WHERE verdict.run_id = delivered.run_id
              AND (verdict.timestamp, verdict.id) > (moved.timestamp, moved.id)
              AND json_extract(verdict.payload_json, '$.coherence.decision') IS NOT NULL
            ORDER BY verdict.timestamp, verdict.id
            LIMIT 1) AS next_verdict
    FROM delivered
    LEFT JOIN events AS moved ON moved.id = delivered.move_id
)
SELECT 'catchup_s0' AS result_set, run_id,
       json_extract(payload_json, '$.landings') AS landings,
       json_extract(payload_json, '$.via') AS via,
       timestamp AS delivered_at, moved_at,
       CAST(round((julianday(moved_at) - julianday(timestamp)) * 86400) AS INTEGER) AS seconds,
       next_verdict
FROM followed
ORDER BY timestamp, id;

-- 3. catchup_unsettled: asks written with landings no delivery or expiry on
--    their Work named. A landing written twice must be named twice. Each row
--    lists the write's unsettled landings; a hook may still deliver those of
--    Work in progress.
WITH written AS (
    SELECT event.id, event.run_id, event.timestamp, landing.key, landing.value AS landing,
           ROW_NUMBER() OVER (PARTITION BY event.run_id, landing.value
                              ORDER BY event.timestamp, event.id) AS nth
    FROM events AS event, json_each(event.payload_json, '$.landings') AS landing
    WHERE event.event_type = 'catchup.written'
), settled AS (
    SELECT event.run_id, landing.value AS landing, COUNT(*) AS times
    FROM events AS event, json_each(event.payload_json, '$.landings') AS landing
    WHERE event.event_type IN ('catchup.delivered', 'catchup.expired')
    GROUP BY event.run_id, landing.value
), open AS (
    SELECT written.*
    FROM written
    LEFT JOIN settled
           ON settled.run_id = written.run_id AND settled.landing = written.landing
    WHERE written.nth > COALESCE(settled.times, 0)
    ORDER BY written.timestamp, written.id, written.key
)
SELECT 'catchup_unsettled' AS result_set, run_id, timestamp AS written_at,
       json_group_array(landing) AS landings
FROM open
GROUP BY id
ORDER BY timestamp, id;
