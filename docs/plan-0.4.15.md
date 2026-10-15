# Plan: Dispatch 0.4.15, worktrees caught while they exist

**Draft, 2026-10-15.** Base: `1ab57e1` (Dispatch 0.4.14). Built as
0.4.14 was: workers wrapped by a pinned released Dispatch, packets landed with
`check --merged` and `accept --expect`, and CI validating each landing.

## 1. Objective

An agent's worktree should become Work within about a second of appearing,
whatever the agent. The work in it should survive the agent removing its own
worktree.

- **Fast discovery.** Discovery runs on its own light loop, about once a
  second, for every recognized harness. The coherence tick keeps its interval.
- **Vanishing worktrees.**
  - While a worktree exists, Dispatch keeps its Δ fresh.
  - When the agent removes the worktree, the Δ Dispatch last saw is kept, dated
    and labelled as last seen.
  - Where the harness offers a hook that fires before removal, Dispatch takes
    the exact Δ there, as it already does with Claude Code's `WorktreeRemove`.
- **Hermes worktrees recognized.**
- **Hermes's conformance cell for its own worktree mode passes.**

## 2. Evidence (from 0.4.14)

| | Claude Code 2.1.296 | Codex 0.155.1 | Cursor Agent 2026.10.14 | Hermes Agent 0.18.2 |
|---|---|---|---|---|
| Worktree | `<repo>/.claude/worktrees/<name>`, branch `worktree-<name>` | `~/.codex/worktrees/<4 hex>/<repo>`, detached | `~/.cursor/worktrees/<repo lowercased>/<name>`, branch `<name>` | `<repo>/.worktrees/hermes-<8 hex>`, branch `hermes/<same>`; only from `hermes chat --worktree` (`-z` ignores the flag) |
| After the session | kept; `-p` leaves its lock | kept | kept | **removed, with its branch and the agent's changes**; `.worktrees/` is added to the checkout's `.gitignore` |
| Process signal | lock with pid and start time | none | none | not yet observed |
| Pre-removal hook | `WorktreeRemove` (used since 0.4.6) | hooks exist (`SessionStart`, `Stop`); timing unknown | none known | shell hooks exist (`hermes hooks`); events unknown |

- **Discovery today** runs inside the owner tick (`coherence.poll_secs`: 10 s by
  default, 5 s in the 0.4.14 build). A worktree that lives less than a tick can
  be missed, and the Δ is only as fresh as the last tick.
- **Vanished worktrees today.** The owner notices a vanished worktree and keeps
  the last-seen Δ (`runtime_hooks::a_worktree_that_vanishes_unannounced_is_noted_and_cannot_be_finished`),
  but such Work cannot be finished.
- **Product value.** Without Dispatch, a Hermes worktree session's changes are
  gone when it ends unless the agent commits them somewhere else. Keeping the Δ
  rescues work, beyond tracking it.

### What P0 found so far (2026-10-15, no agent runs)

These come from Hermes's source and local measurements; the details are in
`~/dispatch-dev/p0-0415/hermes-source.md`.

- **Hermes locks its worktree** with `git worktree lock --reason "hermes pid=<pid>"`.
  That is a liveness signal, but with a pid only and no start time. Guard
  against pid reuse: the process must have started no later than the worktree
  was created.
- **Hermes removes its worktree only when there are no unpushed commits.**
  - With commits, it keeps the worktree and says so.
  - Without them, it runs `git worktree remove --force` and `git branch -D`,
    and uncommitted changes are lost. That happened in the 0.4.14 trial.
- **Hermes fires its session-end hooks before it removes the worktree.**
  - Chat path: `_cleanup_worktree` is registered with `atexit` right after
    setup, and `_run_cleanup` (`on_session_end`, `on_session_finalize`) is
    registered later. Exit handlers run in reverse order.
  - TUI path: removal runs after the TUI process exits.
  - So a Hermes shell hook (in `~/.hermes/config.yaml`, under Hermes's own
    first-use consent) can take the exact Δ while the worktree still exists.
    One short real run still has to confirm this, and show the payload.
- **Discovery is cheap.** `git worktree list --porcelain` takes 11 ms on this
  repository and 21 ms with 60 linked worktrees, so a one-second loop costs
  about 1–2% of a core. No adaptive interval is needed.
- **Codex does not remove its worktrees,** so this release's vanishing-worktree
  problem does not arise for it. Its `Stop` hook marks the end of a turn, not of
  the session. Codex hooks stay out of scope.

## 3. What P0 must answer

P0's probes are short ad hoc runs of real agents (Hermes and Codex), kept to the
few needed. The harness conformance trials run only when the user approves them
(`harness-trials.yml` is disabled until then).

- **Hermes:**
  - How long its worktrees live, measured over several sessions.
  - Whether any hook event fires before its cleanup, with what payload, and
    whether it can block cleanup.
  - Whether its cleanup is `git worktree remove --force`, a plain `rm`, or
    something else.
  - Whether anything names its process.
- **Codex:** the `Stop` hook's payload and timing relative to the worktree.
  Does it fire while the worktree still exists?
- **Cost on a large repository** (Dispatch itself, and one with 50+ linked
  worktrees):
  - a one-second `git worktree list --porcelain` plus the marker reads;
  - a one- to two-second live-Δ capture of a busy worktree (tracked and
    untracked files).

## 4. Contracts

### A. Fast discovery (all harnesses)

- Discovery leaves the coherence tick for its own loop in the owner. It runs
  about once a second and does only list, recognize and register, through the
  same recognizers and registration path. It is not a second discovery path.
- Git runs with `GIT_OPTIONAL_LOCKS=0`, as it does everywhere since 0.4.13.
- The loop's cost, measured in P0, is reported in the plan. If it is too high
  on a large repository, the interval adapts to what the listing costs;
  correctness never depends on the interval.

### B. Vanishing worktrees

- **Which runtimes.** A runtime whose worktrees can vanish is declared in its
  recognizer: Hermes now, others if evidence shows it.
- **While its worktree exists,** its Work keeps `delta-live.patch` fresh on the
  fast loop: every one to two seconds, and only when the tree changed.
- **When the worktree is gone,** the Work keeps that Δ. Status and watch say
  that the agent removed its worktree, and when the kept changes were last
  seen.
- **Decision D1:** can such Work be finished and reviewed from the kept Δ? It
  is labelled as last seen, and accept still runs the merged checks.

### C. Hermes recognizer

- A worktree at `<root>/.worktrees/hermes-<8 hex>` on branch
  `refs/heads/hermes/<same 8 hex>` is Hermes-made.
- S0 is the merge base. Liveness follows P0's evidence, `Unknown` otherwise.
- It lives in `src/runtime/hermes.rs`, as one more entry in the recognizer list.

### D. Exact capture through hooks (gated by P0)

- **Only where P0 shows it:** a harness whose hook fires before removal gets
  exact capture, as Claude's `WorktreeRemove` does. It is adapter-owned, and
  installed by `dispatch setup` like Claude's hooks.
- **Otherwise,** contract B's last-seen Δ is the answer, and the docs say so.

### E. Conformance

- The runner's Hermes worktree cell passes.
- A synthetic test covers a vanishing worktree: an agent stand-in that writes,
  then removes its own worktree.
- Every other cell stays as it is.

## 5. Packets

| Packet | Contract | Depends on |
|---|---|---|
| P0 | §3 probes; the evidence amends §2 and §4 | — |
| P1 | A, fast discovery | P0 (cost) |
| P2 | B and C, vanishing worktrees and Hermes | P0 |
| P3 | D, Hermes session-end hook capture: `dispatch setup` installs it, the same capture path as Claude's `WorktreeRemove` (gated on the confirming run) | P0 |
| R | docs, version, `Site impact:` line; a conformance rerun only if the user approves it (real agents use their tokens) | all |

- **Workers on several harnesses again.** Hermes now works through OpenAI Codex
  and can take a packet; Cursor's free plan resets monthly.
- **Overlap is fine.** P1 and P2 both touch the owner loop; Dispatch reports
  where they interact.

## 6. Deliberately not built

- **No filesystem-notification dependency** (FSEvents, inotify) unless P1 shows
  that one-second polling misses worktrees in practice.
- **No locking of an agent's worktree** to stop its cleanup: it would make the
  agent fail, and AGENTS.md rules out pessimistic locking.
- **No change to how any agent behaves,** no process scanning, and no
  automatic finish.
- **No recognition without a reliable marker.**

## 7. Decisions for the user

- **D1. Finishing from a kept, last-seen Δ.**
  - Recommended: yes, labelled as last seen, with the merged checks at accept as
    usual.
  - Otherwise the person can only close it, as today.
- **D2. Discovery interval.** Recommended: one second by default, adapting only
  if P0 shows real cost, and not a new setting.
- **D3. Workers.** Recommended: Claude Code, Codex and Hermes, at least one packet
  each.

The user chose every recommendation on 2026-10-15:
- D1: a kept, last-seen Δ can be finished and accepted;
- D2: about one second, with no new setting;
- D3: Claude Code, Codex and Hermes workers.

## Not in this release, carried from 0.4.14

- File-level facts derived from a path named only in comments or configuration,
  which caused three false REFRESH verdicts. It needs a design of its own and
  more evidence.
- `status` with no runs errors.
- `watch` and the CLI suggest different next steps for REFRESH Work.
