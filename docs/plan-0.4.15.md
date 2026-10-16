# Plan: Dispatch 0.4.15, worktrees caught while they exist

**Built 2026-10-15/16** (§8). Base: `1ab57e1` (Dispatch 0.4.14). Built as
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
| Worktree | `<repo>/.claude/worktrees/<name>`, branch `worktree-<name>` | `~/.codex/worktrees/<4 hex>/<repo>`, detached | `~/.cursor/worktrees/<repo lowercased>/<name>`, branch `<name>` | `<repo>/.worktrees/hermes-<8 hex>`, branch `hermes/hermes-<same 8 hex>`; only from `hermes chat --worktree` (`-z` ignores the flag) |
| After the session | kept; `-p` leaves its lock | kept | kept | **removed, with its branch and the agent's changes**; `.worktrees/` is added to the checkout's `.gitignore` |
| Process signal | lock with pid and start time | none | none | lock `hermes pid=<pid>`, no start time (P0) |
| Pre-removal hook | `WorktreeRemove` (used since 0.4.6) | not needed: worktrees are kept | none known | `on_session_finalize` shell hook, runs before removal (P0) |

- **Discovery today** runs inside the owner tick (`coherence.poll_secs`: 10 s by
  default, 5 s in the 0.4.14 build). A worktree that lives less than a tick can
  be missed, and the Δ is only as fresh as the last tick.
- **Vanished worktrees today.** The owner notices a vanished worktree and keeps
  the last-seen Δ (`runtime_hooks::a_worktree_that_vanishes_unannounced_is_noted_and_cannot_be_finished`),
  but such Work cannot be finished.
- **Product value.** Without Dispatch, a Hermes worktree session's changes are
  gone when it ends unless the agent commits them somewhere else. Keeping the Δ
  rescues work, beyond tracking it.

### What P0 found (2026-10-15)

These come from Hermes's source, local measurements and one confirming Hermes
session; the details are in `~/dispatch-dev/p0-0415/hermes-source.md` and
`~/dispatch-dev/p0-0415/hook-confirm/`.

- **Hermes locks its worktree** with `git worktree lock --reason "hermes pid=<pid>"`.
  That is a liveness signal, but with a pid only and no start time. Guard
  against pid reuse: the process must have started no later than the worktree
  was created.
- **Hermes removes its worktree only when there are no unpushed commits.**
  - With commits, it keeps the worktree and says so. Unpushed means not on
    any remote-tracking ref; a repository with no remote-tracking refs counts
    as having none, so there even committed work is removed (from source,
    `_worktree_has_unpushed_commits`).
  - Without them, it runs `git worktree remove --force` and `git branch -D`,
    and uncommitted changes are lost. That happened in the 0.4.14 trial.
- **Hermes fires its session-end hooks before it removes the worktree.**
  - Chat path: `_cleanup_worktree` is registered with `atexit` right after
    setup, and `_run_cleanup` (`on_session_end`, `on_session_finalize`) is
    registered later. Exit handlers run in reverse order.
  - TUI path: removal runs after the TUI process exits.
  - So a Hermes shell hook (in `~/.hermes/config.yaml`, under Hermes's own
    first-use consent) can take the exact Δ while the worktree still exists.
- **One real session confirmed it** (`hermes chat -Q --yolo --worktree -q`,
  Hermes 0.18.2, 16 s, the agent's edit left uncommitted):
  - Both hooks ran while the worktree was still registered, still locked
    `hermes pid=<pid>`, and still held the agent's uncommitted change. Hermes
    removed it, and its branch, right after.
  - `on_session_finalize` (`"reason": "shutdown"`) is the session's end.
    `on_session_end` fires at the end of every turn, so it is not.
  - The payload's `cwd` is the main checkout, not the worktree. The worktree's
    path reaches the hook as `TERMINAL_CWD` in its environment. The payload
    carries `session_id`; the hook's parent pid is the pid in the lock.
  - Hooks run synchronously (`subprocess.run`, 60 s default timeout, 300 s
    maximum), so removal waits for the hook (from source, not timed).
  - Hermes records consent in `~/.hermes/shell-hooks-allowlist.json` on first
    use (`--accept-hooks` when not on a terminal). The probe's config change and
    allowlist were removed afterwards.
- **Discovery is cheap.** `git worktree list --porcelain` takes 11 ms on this
  repository and 21 ms with 60 linked worktrees, so a one-second loop costs
  about 1–2% of a core. No adaptive interval is needed.
- **Codex does not remove its worktrees,** so this release's vanishing-worktree
  problem does not arise for it. Its `Stop` hook marks the end of a turn, not of
  the session. Codex hooks stay out of scope.

## 3. What P0 had to answer

Answered in §2, except where noted.

P0's probes are short ad hoc runs of real agents, kept to the few needed. The
harness conformance trials run only when the user approves them
(`harness-trials.yml` is disabled until then).

- **Hermes:**
  - How long its worktrees live: as long as the session (16 s in the probe).
    Not measured over several sessions; with the hook, capture no longer
    depends on it.
  - Whether a hook fires before cleanup: yes, `on_session_finalize`, and
    cleanup waits for it.
  - How it cleans up: `git worktree unlock`, `git worktree remove --force`,
    `git branch -D`.
  - What names its process: the worktree lock, `hermes pid=<pid>`.
- **Codex:** out of scope, since Codex does not remove its worktrees.
- **Cost on a large repository:**
  - The one-second listing: measured, 11 to 21 ms.
  - The one- to two-second live-Δ capture of a busy worktree: not measured.
    P2 measures it and reports it here.

## 4. Contracts

### A. Fast discovery (all harnesses)

- Discovery leaves the coherence tick for its own loop in the owner. It runs
  about once a second and does only list, recognize and register, through the
  same recognizers and registration path. It is not a second discovery path.
- Git runs with `GIT_OPTIONAL_LOCKS=0`, as it does everywhere since 0.4.13.
- The loop's cost was measured in P0 (§2): 11 to 21 ms a listing. The interval
  is fixed at about a second (D2), with no new setting; correctness never
  depends on it.
- The loop keeps its cadence while the coherence tick is busy, for example
  while the owner runs a project's checks for Work finished by consent.

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
  `refs/heads/hermes/hermes-<same 8 hex>` is Hermes-made (observed:
  `.worktrees/hermes-dbc09083` on `refs/heads/hermes/hermes-dbc09083`).
- S0 is the merge base, as for every discovered worktree.
- Liveness comes from the lock `hermes pid=<pid>`: running while that process
  lives and started no later than the worktree was made, gone when it does not,
  `Unknown` otherwise.
- It lives in `src/runtime/hermes.rs`, as one more entry in the recognizer list.

### D. Exact capture through hooks (gated by P0)

- **Only where P0 shows it:** a harness whose hook fires before removal gets
  exact capture, as Claude's `WorktreeRemove` does. It is adapter-owned, and
  installed by `dispatch setup` like Claude's hooks.
- **Hermes shows it.** `dispatch setup` adds one `on_session_finalize` shell
  hook to `~/.hermes/config.yaml`. The hook finds its worktree from
  `TERMINAL_CWD` and its process from its parent pid, and records the exact Δ
  before Hermes removes the worktree. Hermes's own first-use consent stays in
  place; Dispatch never writes Hermes's allowlist.
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
| P1 | A, and B while the worktree exists: the fast loop, and a fresh Δ for Work whose runtime's worktrees can vanish | P0 |
| P2 | B once the worktree is gone: Work finished from its last-seen Δ (D1), and how status, watch and review show it | P0 |
| P3 | C and D, everything Hermes: the recognizer, its liveness, and the `on_session_finalize` hook that `dispatch setup` installs | P0 |
| R | docs, version, `Site impact:` line; a conformance rerun only if the user approves it (real agents use their tokens) | all |

- **Workers (D3):** P1 on Codex, P2 on Hermes, P3 and R on Claude Code.
- **Split by file, so no two packets create the same file:** P1 owns the
  owner's loops (`serve.rs`), P2 the finish and review path (`attach.rs` and
  the status wording), P3 `src/runtime/hermes.rs` and the setup integration.
  Small overlaps remain (`discovery.rs`, `liveness.rs`); Dispatch reports
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

## 8. Execution log

Built as planned: workers wrapped by the pinned Dispatch 0.4.14 (`2e77b308afd3`,
state `~/dispatch-dev/state-0415`), every packet checked with `check --merged`
before review and landed with `accept --expect`, and CI validating each
landing on draft PR #24. No coherence override was needed.

### P0 (2026-10-15)

- Hermes's worktree lifecycle was read from its source
  (`~/dispatch-dev/p0-0415/hermes-source.md`), and the discovery cost measured
  (§2).
- One confirming Hermes session, approved by the user, with a temporary probe
  hook (§2, `~/dispatch-dev/p0-0415/hook-confirm/`). The user's
  `~/.hermes/config.yaml` was restored byte for byte, and the allowlist the run
  created was removed.

### Packets

| Packet | Worker | Work | Merged checks before review | Landed |
|---|---|---|---|---|
| P1 fast loop | Codex 0.155.1, gpt-6-sol | `01M50J8KTTE2C4HVPARN2MQ0GD` | CONTINUE, 738 passed | `cde671b`, `--expect a8a1a8af82c6`, merged checks rerun on top of P2 |
| P2 finish from last seen | Hermes Agent 0.18.2, gpt-6.1-sol | `01M50J8KTXE423FG0330603F67` | CONTINUE, 738 passed | `a3a859a`, `--expect 8b7ec9c932d7` |
| P3 Hermes end to end | Claude Code | `01M50J8KTTPMFR5GZH40ZMS50V` | CONTINUE, 748 passed | `0154c5b`, `--expect 6fe043e45494`, merged checks rerun on top of P1 and P2 (754 passed) |
| P4 session end after removal; runner | Claude Code | `01M50Y9JNMK63YVTCPY7S6MFVF` | CONTINUE, 755 passed | `354c9c1`, `--expect 6676fe0ebaa5` |
| R docs, version | Claude Code | `01M50Y1ZQSW8X0Q66CRNW4823Z` | CONTINUE, 754 passed | `2ed389f`, `--expect 7b63741115e0`, merged checks rerun on top of P4 |
| P5 freshest Δ on removal | Claude Code | `01M51BZXA1Z3FP8TET210695F3` | CONTINUE, 756 passed | `d732e1b`, `--expect 5d204d2863c0`, merged checks rerun |

- **Overlap.** P1 and P3 both changed `discovery.rs`. P3's check before
  accept read CONTINUE at the symbol level with 11 files changed underneath,
  and its merged checks passed on top of P1 and P2.
- **Review sent work back twice:**
  - a capture lock and second patch file in P1, for wrapping a process inside a
    Hermes-made worktree, which no observed case does; P1 went from +334 to
    +291 production lines;
  - two cost figures in R taken from the wrong column of P1's table (the full
    follow instead of status-first: about 15 ms and 47 ms on this repository).
- **Found before the conformance trial, by reading** (packet P4):
  - the runner read the agent's tree after the agent exited, when Hermes had
    already removed it;
  - a Hermes session's end was never recorded once its worktree was removed.
- **Found by CI** (packet P5): on macOS, `watch_acceptance` saw last-seen Work
  cancelled as having no changes. The owner takes a Work's Δ twice a pass, and
  an edit landing between the two was published in `delta-live.patch` but not
  kept. P5 keeps the newer of the two. On a filesystem with one-second
  timestamps a tie goes to `delta-live.patch`. Not built: a guard for a capture
  that overlaps the removal itself (P5's handoff has the idea).
- **One unexplained CI failure.** On `354c9c1`, `tests under load` failed once in
  `hermes_worktrees::the_finalize_hook_keeps_the_exact_changes_before_hermes_removes_the_worktree`:
  the hook exited 0, but nothing was recorded.
  - Passed: the rerun of that job, stress CI on every other commit, and 52 local
    stress repeats in the same image.
  - Ruled out by reading: a second registration, `publish_durable`, the repo
    key, and Dispatch touching worktree locks.
  - Left: the hook's silent paths that depend on a Git command succeeding.
    `inspect_source` reads a failing `rev-parse --is-inside-work-tree` as a
    plain directory.
  - Carried to the next release, with diagnostics if it recurs.

### Workers, as their harnesses reported

- **Codex (P1):**
  - 440,541 tokens in total: 348,897 input (plus 25,399,424 cached) and 91,644
    output (52,279 reasoning);
  - asked once to run a `ps` timing probe outside its sandbox, approved.
- **Hermes (P2):**
  - 55m53s, 220 messages (213 tool calls);
  - stopped once at its 60-tool-call turn budget, and finished after a second
    message;
  - tokens not shown.
- **Claude Code (P3, P4, R, P5):** usage not reported at exit.

### Conformance (user-approved, 2026-10-16)

- **Scope:** only the Hermes worktree cell, with the candidate built from
  `2ed389f` and the runner from the same commit. Real Hermes 0.18.2 ran three
  short sessions without the Dispatch hook, so the last-seen path.
- **Result: pass.** For B, A and C, S0 + Δ equalled the agent's tree as the
  runner last saw it. Sessions' starts and ends were recorded, finish
  succeeded, and the verdicts held.
- **Evidence:** `~/dispatch-dev/conformance-0415/trials/hermes-worktree/`.
  Every other cell keeps its 0.4.14 result.

## Not in this release, carried from 0.4.14

- File-level facts derived from a path named only in comments or configuration,
  which caused three false REFRESH verdicts. It needs a design of its own and
  more evidence.
- `status` with no runs errors.
- `watch` and the CLI suggest different next steps for REFRESH Work.
