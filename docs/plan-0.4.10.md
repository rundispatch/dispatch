# Plan: Dispatch 0.4.10, no silent gaps

## Context

0.4.9 was built by four parallel Claude Code workers supervised by Dispatch
0.4.8, and that build exposed two defects in the released supervision path:
1. **`SessionStart` hooks killed mid-registration.** Under heavy load, Claude
   Code's 60 s timeout killed the hook while Dispatch was still registering the
   session. Two workers were left untracked, with half-made run directories.
2. **`attach` failing on build output.** `dispatch attach` failed when cargo
   removed a build file while Dispatch was reading it.

**Public invariant, for eligible isolated sessions in watched projects** (a
runtime session in its own worktree of a project an owner watches):

> Every such session either becomes trustworthy Work, is recoverably pending,
> or is told it is not tracked. There is no silent loss and no half-created
> Work.

No interaction policy changes in this release.

## 1. Root cause (verified on `main`, `35d7f61`)

`attach::create`, which the hook calls, took `source::fingerprint_tree(&root)`
after capturing S0. That function excludes only `.git` and `.dispatch`, so it
hashed every file under the integration root, ignored ones included.
`claude --worktree` places worktrees inside the root (`.claude/worktrees/`),
each with its own `target/`.

- **The 60 s kills.** B and C registered at 11:04, before anything had been
  built. A and D registered at 11:19, with gigabytes of build output under the
  root, and were killed.
- **The `attach` failure** ("failed to inspect …/049-a/target/…o.tmp") was the
  same walk, meeting a file cargo had just deleted.
- **The fingerprint was unused.** For attached Work it is read only in strict
  mode (`coherence::gate`), or when the run's config snapshot cannot be read;
  every reader fails closed on a mismatch.

**P0 fix:** attached runs fingerprint the root only in strict mode. Otherwise
the value is `attach::NOT_FINGERPRINTED`, which never matches.
`attach_outside_strict_mode_never_walks_ignored_build_output` fails without the
fix ("failed to walk … build/sealed: Permission denied").

## 2. Decisions (agreed 2026-10-07)

1. **Registration is synchronous, with a fallback.** A session's Work is
   published inside the hook when it can be. Pending exists only for a hook that
   runs out of time.
2. **The state machine.** At the deadline:
   - **published** = registered;
   - **durable S0 but not published** = pending;
   - **no durable S0** = untracked.

   Once untracked is decided, nothing can publish that registration afterwards.
   The deadline covers the whole `SessionStart` path, from the hook process
   starting.
3. **The strict-mode limitation is accepted.** Strict mode still fingerprints
   the root's whole tree. Packet B only makes that walk tolerate files that
   disappear.
4. **No file is owned by two parties at once.** P0 creates
   `src/orchestrator/registration.rs` and hands it to packet A; the integrator
   does not edit it while A owns it.

## 3. The contract (`src/orchestrator/registration.rs`)

### 3.1 Where Work is built
- Every attach form assembles its run in `<state>/registrations/<run_id>/`,
  never in `runs/`. `runs/` only ever receives complete Work.
- **Publishing** inserts the database record, then renames the directory to
  `runs/<run_id>/`. Every path stored in the record already names
  `runs/<run_id>/…`. Publishing is idempotent: a record that already exists, or
  a rename that is already done, is not an error.
- Only an eligible runtime session can be pending. Foreign and wrapped attach
  either publish, or fail and remove their registration directory.

### 3.2 The S0 boundary
- **S0** is the commit `source::world_commit` makes of the workspace, in its
  repository's object store. The baseline, the strict-mode fingerprint and the
  record are all derived from it.
- **`registration.json`** (`Registration`) is written with
  `state::write_durably` right after S0 is captured. It holds `run_id`,
  `workspace`, `root`, `s0_commit`, `session`, `resumed`, `consented` and
  `started_at`. Its existence is what "durable S0" means.
- **A capture that moved is never accepted.** `world_commit` (packet B)
  compares the workspace's change signal before and after, and retries once.
  If it moves again, it returns an error, and the outcome is untracked. Objects
  are written with `core.fsync=loose-object`.

### 3.3 The decision
- The file `decision` in the registration directory is created exclusively
  (`create_new`), so it has exactly one writer:
  - **the worker** writes `publishing`, only after `registration.json` is
    durable and immediately before publishing;
  - **the deadline** writes `pending` if `registration.json` exists, and
    `untracked` if it does not.
- If the worker finds a decision already there, it stops and never publishes.
- If the deadline finds `publishing`, it waits for the publish within the
  remaining margin. If the run is published, the session is registered;
  otherwise it is pending, and the worker or the owner completes it.
- `BUDGET` is 40 s from the hook process starting. `dispatch setup` installs
  `SessionStart` with a 60 s timeout.

### 3.4 Notices
- **Registered:** "Dispatch is tracking this worktree as Work `<id>`; see it with
  dispatch watch." (unchanged)
- **Pending:** `notice_pending(id)`.
- **Untracked:** `NOTICE_UNTRACKED`.

### 3.5 Reconciliation (`reconcile`, called on every owner tick, and first by any hook for that workspace)

| What the registration directory holds | What happens |
|---|---|
| no `registration.json`, younger than `BUDGET` | left alone (it may still be running) |
| no `registration.json`, older than `BUDGET`, or `decision = untracked` | removed, and logged |
| `registration.json`, with `decision` `pending` or `publishing`, or no decision and older than `BUDGET` | published under the workspace lock |
| `registration.json` whose S0 commit can no longer be read | not published; logged as a failed registration and kept for inspection |

- **Any hook for a workspace with a registration whose S0 is durable publishes
  it first,** under the workspace lock. That covers a repeated `SessionStart`
  (the session is recorded on the same Work), `SessionEnd`, and
  `WorktreeRemove`, which publishes and then keeps the exact Δ as before.
- **No duplicate Work:** one workspace lock, one decision, and idempotent
  publishing.
- **Partial directories left in `runs/` by 0.4.8 and 0.4.9** are not touched.

### 3.6 Evidence
- The `attach.created` payload gains `registration_ms` (from `started_at`) and
  `via` (`hook` or `owner`).
- The owner logs every registration it removes or fails to publish.

## 4. Packets

| Packet | Owner | Files (exclusive) |
|---|---|---|
| P0, W, R | integrator | `src/orchestrator/attach.rs` (P0's one change, before A starts), `src/orchestrator/serve.rs`, `src/orchestrator.rs`, this plan, README, `docs/attach.md`, `docs/coherence.md`, `AGENTS.md`, release notes, `tests/attach_cli.rs` |
| A | worker | `src/orchestrator/registration.rs` (handed over from P0), `src/runtime.rs`, `src/orchestrator/attach.rs` (after P0), `tests/runtime_registration.rs` (new) |
| B | worker | `src/source.rs`, `tests/attach_build_churn.rs` (new) |
| C | worker | `docs/self-hosting.md` (new) |
| D | worker | `src/coherence/facts.rs` (an additive audit function), `tests/name_audit.rs` (new) |

```
P0 ─┬─ A ─┐
    ├─ B ─┤
    ├─ C ─┼─ W ── R (stress trial, release)
    └─ D ─┘
```

- **Integration order:** B, D, C, A, then W and R.
- **A relies on B only through the contract:** `world_commit` keeps its
  signature, and may return an error when the workspace moved during capture.

## 5. Supervision

- **Pinned release:** Dispatch 0.4.9 at `~/dispatch-dev/dispatch-0.4.9/dispatch`.
  Its archive matches `SHA256SUMS`, and the binary matches `BUILD.json`'s
  `binary_sha256` (`628624d1…`, head `35d7f61`).
- **State:** `~/dispatch-dev/state-0410`.
- **Integration clone:** a fresh clone, `~/dispatch-dev/integration-0410`, on
  `release-0.4.10`.
- **Excluded from git:**
  - `dispatch.yml`, with checks fmt, clippy and the lib tests and no shared
    target directory;
  - `.claude/settings.local.json`: the four hooks, pointing at the pinned binary
    and the state above, plus `worktree.baseRef: head`.
- **The supervising 0.4.9 still has the root-cause bug.** Each worker builds
  with its own `CARGO_TARGET_DIR=~/dispatch-dev/targets-0410/<packet>`, outside
  the root, so the 0.4.9 walk stays small, and workers start before anyone
  builds. A worker the hook still drops is evidence, attached by hand as in
  0.4.9.
- **No auto-apply.** Candidate binaries are tested with their own state
  directories.

## 6. Stress trial (R)

The candidate 0.4.10 runs against a clone of Dispatch (a real Rust project)
under `~/dispatch-dev/stress-0410/`, with its own state, outside `/tmp`.
1. **Load:** cargo builds in 2–3 worktrees inside the root, CPU load on every
   core, and an I/O loop.
2. **4–6 near-simultaneous `claude -p --worktree` sessions:** registered,
   pending-then-published, or untracked with a notice. Latencies come from
   `attach.created`.
3. **A hook stopped at each registration boundary;** the owner reconciles it
   per §3.5.
4. **`kill -9` of the owner, then a restart,** with Work pending and Work
   registered.
5. **A repeated `SessionStart`:** the same Work and the same S0.
6. **`attach --workspace` and `attach --` while ignored `target/` churns:** both
   succeed, and none of it enters S0 or Δ.
7. **A pair of relevant files rewritten in a loop during capture:** consistent S0
   or an explicit failure, never mixed.
8. **This release itself, built by workers under 0.4.9:** registrations,
   drops, recoveries, interactions and invalidations.

Recorded: attempted registrations, registered, recovered, untracked, the
latency distribution, cleanups, attach attempts during churn, failures caused by
source movement, unresolved-name counts by cause, the evaluation of each worker,
and Dispatch's interactions and coherence changes. No aggregate score.

## 7. Definition of done

- In tests and in the stress trial, every eligible `SessionStart` ends
  registered, pending-then-published, or untracked with its notice.
- `runs/` never holds incomplete Work, and `registrations/` leftovers are
  resolved by §3.5 alone.
- No duplicate Work under repeated, concurrent, killed or restarted delivery.
- `attach` succeeds under cargo churn, and no mixed S0 is accepted.
- C's commands were run live. D's report is in this log.
- fmt, clippy and the full suite are clean. The growth in production lines is
  checked, and every new doc and command is checked by hand.

## 8. Deferred

- **Cleaning up partial directories from 0.4.8 and 0.4.9:** they are ambiguous,
  and only reported.
- **The cost of fingerprinting in strict mode:** an ignore-aware fingerprint
  would be a semantic change.
- **The cost of `find_active_attachment`:** measured in the stress trial,
  indexed only if it shows up.
- **Any change to name binding:** this release audits only.
- **The runtime's real configured hook timeout:** `BUDGET` assumes the 60 s
  `dispatch setup` installs.

## Progress log

- 2026-10-07: Setup.
  - The pinned 0.4.9 is verified as in §5.
  - `~/dispatch-dev/integration-0410` is on `release-0.4.10` from `35d7f61`.
  - The excluded config and hooks are written; the owner started.
  - The 0.4.8 owner still watching the 0.4.9 integration clone was stopped.
- 2026-10-07: P0.
  - The root-cause fix in `attach::create`, with its regression test (it fails
    without the fix).
  - `src/orchestrator/registration.rs`: the contract types, constants and
    notices, plus a no-op `reconcile`, which the owner already calls each tick.
  - This plan.
