-- Notes, measured (0.4.18): what followed each note delivered to a live agent.
--
-- It only reads. Run it against a Dispatch state database:
--
--   sqlite3 <state>/dispatch.db < docs/queries/notes-measured.sql
--
-- Every statement only reads. Do not add -readonly: the database uses a
-- write-ahead log, and a read-only connection fails when no process has the
-- database open, because it cannot create the log's shared-memory file.
--
-- Add `-header -column` for aligned output. Every row starts with its result
-- set's label. All events here are recorded on the Work the note was for:
--
-- - `note.written` {note, text}: the owner put the note in the Work's inbox;
-- - `note.delivered` {note, via}: a hook gave it to the agent, at that time;
-- - `note.expired` {note, lifecycle}: the Work stopped being in progress
--   first;
-- - `note.touched` {note, key}: the first change the Work's Δ made at a
--   target the note named (the declaration, or the file for a textual
--   overlap) after the note was written. `key` is the item's key,
--   `[kind, other run, path, name, change]`;
-- - `attach.s0_moved`: the Work's S0 followed a source it integrated;
-- - any event whose payload has `coherence.decision` is a verdict of
--   Dispatch's on the Work: `coherence.checked`, `coherence.invalidated`,
--   and the accept gate's in `result.applied`, `application.failed` and
--   `auto_apply.blocked`.
--
-- "Followed by" means recorded later, not caused by. The next verdict is
-- Dispatch's own, not a human judgment. A note "agrees" with it when that
-- verdict is REFRESH or STOP; that is the only agreement measured, and it is
-- quoted as counts.

-- 1. notes_delivered: one row per note delivered, with what followed it on
--    its Work.
WITH delivered AS (
    SELECT delivery.run_id,
           json_extract(delivery.payload_json, '$.note') AS note,
           json_extract(delivery.payload_json, '$.via') AS via,
           delivery.timestamp AS delivered_at,
           EXISTS (
               SELECT 1 FROM events AS touched
               WHERE touched.run_id = delivery.run_id
                 AND touched.event_type = 'note.touched'
                 AND json_extract(touched.payload_json, '$.note')
                     = json_extract(delivery.payload_json, '$.note')
                 AND touched.timestamp > delivery.timestamp
           ) AS touched,
           EXISTS (
               SELECT 1 FROM events AS moved
               WHERE moved.run_id = delivery.run_id
                 AND moved.event_type = 'attach.s0_moved'
                 AND moved.timestamp > delivery.timestamp
           ) AS s0_moved,
           (
               SELECT json_extract(verdict.payload_json, '$.coherence.decision')
               FROM events AS verdict
               WHERE verdict.run_id = delivery.run_id
                 AND verdict.timestamp > delivery.timestamp
                 AND json_extract(verdict.payload_json, '$.coherence.decision') IS NOT NULL
               ORDER BY verdict.timestamp, verdict.id
               LIMIT 1
           ) AS next_verdict
    FROM events AS delivery
    WHERE delivery.event_type = 'note.delivered'
)
SELECT 'notes_delivered' AS result_set,
       run_id, note, via, delivered_at, touched, s0_moved,
       COALESCE(next_verdict, 'none') AS next_verdict
FROM delivered
ORDER BY delivered_at, run_id, note;

-- 2. notes_by_next_verdict: the rows of 1, counted by the Work's next
--    verdict ('none' while it has none). `agreed` counts REFRESH and STOP.
WITH delivered AS (
    SELECT delivery.run_id,
           EXISTS (
               SELECT 1 FROM events AS touched
               WHERE touched.run_id = delivery.run_id
                 AND touched.event_type = 'note.touched'
                 AND json_extract(touched.payload_json, '$.note')
                     = json_extract(delivery.payload_json, '$.note')
                 AND touched.timestamp > delivery.timestamp
           ) AS touched,
           EXISTS (
               SELECT 1 FROM events AS moved
               WHERE moved.run_id = delivery.run_id
                 AND moved.event_type = 'attach.s0_moved'
                 AND moved.timestamp > delivery.timestamp
           ) AS s0_moved,
           (
               SELECT json_extract(verdict.payload_json, '$.coherence.decision')
               FROM events AS verdict
               WHERE verdict.run_id = delivery.run_id
                 AND verdict.timestamp > delivery.timestamp
                 AND json_extract(verdict.payload_json, '$.coherence.decision') IS NOT NULL
               ORDER BY verdict.timestamp, verdict.id
               LIMIT 1
           ) AS next_verdict
    FROM events AS delivery
    WHERE delivery.event_type = 'note.delivered'
)
SELECT 'notes_by_next_verdict' AS result_set,
       COALESCE(next_verdict, 'none') AS next_verdict,
       COUNT(*) AS delivered,
       SUM(COALESCE(next_verdict IN ('refresh', 'stop'), 0)) AS agreed,
       SUM(touched) AS touched,
       SUM(s0_moved) AS s0_moved
FROM delivered
GROUP BY next_verdict
ORDER BY next_verdict IS NULL, next_verdict;

-- 3. notes_unsettled: notes written that were neither delivered nor
--    expired. `unsettled` counts those of Work no longer in progress, and
--    must be 0 once an owner has looked at the project since that Work
--    finished: every note is delivered, or expires. `pending` counts those
--    of Work still in progress, which a hook may still deliver.
WITH open AS (
    SELECT json_extract(run.outcome_json, '$.lifecycle') = 'working' AS working
    FROM events AS written
    JOIN runs AS run ON run.id = written.run_id
    WHERE written.event_type = 'note.written'
      AND NOT EXISTS (
          SELECT 1 FROM events AS settled
          WHERE settled.run_id = written.run_id
            AND settled.event_type IN ('note.delivered', 'note.expired')
            AND json_extract(settled.payload_json, '$.note')
                = json_extract(written.payload_json, '$.note')
      )
)
SELECT 'notes_unsettled' AS result_set,
       COUNT(*) - COALESCE(SUM(working), 0) AS unsettled,
       COALESCE(SUM(working), 0) AS pending
FROM open;
