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
- 2026-10-07: Workers.
  - Four `claude --worktree 0410-a` to `0410-d` sessions were launched within a
    minute, before any build. Each worker built in its own target directory
    outside the root. All four registered through their hooks under the
    supervising 0.4.9, at a load average of about 10. None were dropped, and no
    run directory was left incomplete.
  - The handoffs of A, B and D were written into their worktrees at the user's
    request. Untracked files there would have entered their Δ, so the
    integrator moved them to `~/dispatch-dev/handoffs-0410/` before `finish`.
  - Dispatch reported no interactions between the packets. That was correct: A
    calls B's `world_commit`, but B kept its signature and changed only its
    body.
- 2026-10-07: Integration, in the order B (`e6fc651`), D (`6536e6f`), C
  (`55a233e`), A (`652d539`), each after the human's approval.
  - Before integrating, the integrator ran all four together, independently:
    fmt and clippy clean, the full suite 578/0, A's registration tests 3/3 and
    B's churn tests 2/2.
  - Each packet was finished (checks passed in its workspace), accepted (C, D
    and A ran the merged-tree checks, which passed), then committed.
  - After every landing, the remaining Work was rechecked: all CONTINUE, no
    interactions.
  - **Corrective work on D (`5f6d2f5`), approved:** `audit_names` and
    `NameCause` are no longer public. The real-data audit is an ignored unit
    test, `coherence::facts::tests::real_runs`, and reproduces D's counts.
    Approved deviations from the contract: the cause `Widespread`, the
    invariant `Ambiguous + Widespread == unbound` (the plan's own invariant was
    wrong), and a recorder parameter on `bind_names` that leaves binding
    unchanged.
  - **Corrective work on A, approved:** the hook's deadline counts from a
    timestamp `main` records, instead of the kernel's process start, which took
    about 60 lines of unsafe, platform-specific code: −61/+11 lines.
  - **D's report.** Six real runs and about 1,000 names:
    - per run, unbound A 46, B 156, C 51, D 40;
    - about 96% of unbound names are widespread (used in more files than
      binding reads), not ambiguous;
    - two cross-run hits, both judged harmless: `capture` counted itself
      within one commit range, and `outcome` was a real reliance on a stub
      whose signature did not change.
    - Recommendation: no change to binding. If a miss is ever observed,
      disambiguate qualified paths (`a::b::name` binds the declaration in
      `b.rs`).
- 2026-10-07: R.
  - **The release candidate:** version 0.4.10, a release build of `a23ff12` plus
    the version bump, copied to `~/dispatch-dev/dispatch-0.4.10-candidate/`. The
    full suite on the integrated tree: 578 passed, 0 failed.
  - **The stress trial** (`~/dispatch-dev/stress-0410`, a clone of Dispatch at
    `35d7f61`, its own state, hooks only in its `.claude/settings.local.json`,
    driver `trial.py`).
    - **Pressure:** three `cargo build` runs in worktrees inside the root, which
      put 1.5 GB of churning build output there; 10 CPU spinners; a 1 GiB disk
      write loop; load average 19–25.
    - **Six `claude -p --worktree` sessions started at once:** all six were
      registered by their hooks in 8.0–8.3 s, with one Work each.
    - **Hooks killed at 0.05, 0.2, 0.5, 1, 2 and 4 s** while the owner ran:
      - the first four had no durable S0; they left registration directories
        (some held only the lock), which the owner removed after the budget;
      - the last two had `registration.json`; the owner published them
        (`via: owner`), about 45–48 s after the hook started;
      - no incomplete run in `runs/`.
    - **A hook killed after `registration.json`, then the owner killed with
      SIGKILL and restarted:** the restarted owner published it, once.
    - **A repeated `SessionStart` (resume, then a new session):** one Work, the
      same S0, sessions recorded: 2.
    - **Attach while an ignored `target/` churned under the root** in three
      worktrees: 5 of 5 `attach --workspace` and 5 of 5 `attach --` succeeded,
      and no build output entered any S0 or Δ.
    - **Two tracked files rewritten together, continuously, during capture:** 6
      of 6 sessions were told "Dispatch could not track this session: the
      workspace changed while its starting state was being captured…". No mixed
      S0 was accepted. A workspace that never stops changing is not tracked; the
      honest outcome.
    - **Totals:**
      - 23 registration attempts: 7 registered by the hook (latency 6.2–8.3 s),
        3 published by the owner, 6 told untracked because of source movement,
        4 killed before a durable S0 (no Work, cleaned up);
      - 10 attaches during churn, all succeeded;
      - no incomplete runs, and no leftover registrations;
      - the owner's log keeps only the time since its last start, because the
        SIGKILL test restarted it.
  - **Found by the trial, fixed:** the registered, pending and removal notices
    named Work by its first 8 characters. All six simultaneous sessions read
    "Work 01M4BRE4". Every runtime notice now names the full Work ID.
  - **Production lines** since `35d7f61`: about 870. `registration.rs` is 616,
    about half doc comments and the contract types. `attach.rs` +123,
    `source.rs` +94, `facts.rs` about +25 (the recorder and `NameCause`), and
    `runtime.rs`, `main.rs` and `serve.rs` a handful each.
  - **Docs:**
    - the README gains the three registration outcomes and links
      `docs/self-hosting.md`;
    - `docs/attach.md` gains the registration rules;
    - `release-install.md` gains "Upgrading to 0.4.10";
    - the self-hosting guide gains what 0.4.10 changes;
    - the release notes are for 0.4.10.

## Evaluation of the contributions

The packets differ in size and kind, so this is not a model comparison, and the
integrator's review is not human preference data. No session reported usage,
so usage stays unknown.

| | A: registration | B: build output and S0 capture | C: self-hosting guide | D: name audit |
|---|---|---|---|---|
| Work | `01M4B5RP…` (discovered) | `01M4B5S2…` (discovered) | `01M4B5SA…` (discovered) | `01M4B5SJ…` (discovered) |
| Scope | its 4 files | its 2 files | its 1 file | its 2 files |
| Contract | followed; no change needed | followed (`world_commit` signature kept) | n/a | reported two needed deviations, both approved |
| Correctness | 13 tests over every boundary; reproduced the half-made `runs/` directory on the old code | reproduced the 0.4.9 field error; honest about what before/after evidence cannot see | every command run live against real state | invariant asserted on six real runs |
| Found beyond its scope | none needed | none | the check builds' `target/` inside the root under a 0.4.9 supervisor | the plan's invariant was wrong; the W range included B's commit |
| Corrective work | the kernel process-start reading replaced by a timestamp `main` records (−61/+11) | none | one line added on 0.4.10 (R) | the audit made test-only (`5f6d2f5`) |
| Duration (reported) | unknown | unknown | about 45 min (estimated) | unknown |
| Usage | unknown | unknown | unknown | unknown |
| Handoff | saved in its worktree (moved out by the integrator) | saved in its worktree (moved out) | saved outside the repository | saved in its worktree (moved out) |

**How Dispatch handled the work** (supervisor 0.4.9):
- **Registration:** all four workers registered through their hooks, with no
  drop. The launch before any build, and build output kept outside the root,
  avoided the bug this release fixes.
- **Interactions:** none reported between the packets, which was correct. A
  calls B's `world_commit`, but B changed only its body.
- **Coherence:** every remaining packet stayed CONTINUE after each landing, and
  the merged-tree checks passed on C, D and A.
- **Defects seen:** none new in 0.4.9 during this build.
