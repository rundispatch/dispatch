# Plan: Dispatch 0.4.16, nothing runs twice

**Draft, 2026-10-18.** Base: `4f88fdc` (Dispatch 0.4.15 plus PR #25). First step on the road to
0.5.0 (`docs/roadmap-0.5.0.md`). Built as 0.4.15 was: workers wrapped by the pinned released
Dispatch 0.4.15, packets checked with `check --merged` and landed with `accept --expect`, CI
validating each landing on a draft PR.

## 1. Objective

Nothing Dispatch does should run twice, and nothing it does itself should be slow.

- **A tree that passed its checks is not checked again.** That holds for the same commands, in
  the same environment, whichever of `finish`, `check --merged` or `accept` asks.
- **Merged checks build warm,** not from scratch.
- **A native agent starts at once:** no suite runs before it, and no hashing of build output.
- **The owner and the hooks cost milliseconds, not seconds.** 0.4.18 adds per-tool hooks, and
  0.5.0 lands batches; both need this.

## 2. Evidence (P0, 2026-10-18)

Measured on the 0.4.14 and 0.4.15 builds' own records and on a clone of this repository. The
scripts and raw output are in `~/dispatch-dev/p0-0416/`.

### Check runs, replayed

The replay (`replay.py`) takes every suite run of a build in time order: each `finish`, each
`check --merged`, each accept-time merged check. It keys each by the tree it tested (base tree
id + Δ digest; dirty sources never match) and counts how many a memo of passed runs would have
skipped.

| Build | Landed Work | Actual runs | With a memo | Memo, and `finish` tests what would land |
|---|---|---|---|---|
| 0.4.15 | 7 | 17 (3.74 h), 2.43 per landed Work | 12 (2.63 h), 1.71 | 12 (2.62 h), 1.71 |
| 0.4.14 | 8 | 20 (4.80 h), 2.50 | 18 (4.42 h), 2.25 | 16 (4.00 h), 2.00 |

The 0.4.15 duplicates were `check --merged` re-testing exactly the tree `finish` had tested.
`orchestrator::check` calls `integration::run_merged` without the "source unmoved" skip that
accept applies (`integration::checks_skipped`). `cargo test` is over 95% of every run.

### The owner, with 6 Works

On a clone of this repository with 6 attached Works, one small edit each, the pinned 0.4.15 ran
`serve -vv` for 100 s while the source moved twice (`tick/run.sh`, `tick/serve.err`):

| Tick | Signal | Follow (6 Works) | Evaluate (6 Works) | Total |
|---|---|---|---|---|
| Nothing moved | 50–75 ms | 300–380 ms | none | about 0.4 s |
| Source moved | ~50 ms | ~340 ms | 2.2–2.5 s | about 2.9 s |
| First tick | 99 ms | 896 ms | 1247 ms (+163 ms footprints) | about 2.4 s |

Two costs stand out:
- **Every idle tick re-snapshots every worktree:** about 50 ms per Work, even when nothing
  changed. Only the fast loop's vanishing worktrees skip an unchanged follow (0.4.15 P1).
- **Every Work evaluates its own copy of the moved world:** about 400 ms each.

### A hook with nothing to do

`dispatch hook claude` with a `SessionStart` payload in a project nobody watches takes 202 ms
at the median and 235 ms at p95 (40 runs). Nearly all of it is Git: resolving the checkout and
the repository identity, at least 20 ms per call because `source::run_git` polls `try_wait`
every 20 ms. This is acceptable once per session. It is not acceptable once per tool call,
which 0.4.18 needs.

### Other probes

- **Copy-on-write:** on this APFS disk `std::fs::copy` clones a 1 GiB file in under a
  millisecond (`cp` takes 0.52 s). Seeding a check workspace from existing build output costs
  nothing on macOS. Linux falls back to a real copy unless the filesystem reflinks.
- **Ignored inputs:** this repository's checks read only build caches (`target/`) from ignored
  files. Today's scratch tree already leaves ignored files out, so a warm workspace changes
  nothing there.
- **Native start:** with an empty `checks.baseline`, `checks.verify` runs on S0 before the
  agent starts (`orchestrator.rs` 690-781). `source::fingerprint_tree` hashes ignored build
  output, about 11 s cold over a 1.4 GB `target/`, up to 7 times per start.

## 3. Contracts

### A. The verification memo

- **What is recorded:** a pass of the configured checks, never a failure. It is keyed by:
  - the tree that was checked: the Git tree id of the merged tree, or of the workspace tree
    for a finish in place, with ignored files left out;
  - the exact ordered commands;
  - an execution fingerprint: backend and image digest, `PATH`, resolved program paths,
    forwarded environment values, and the Dispatch version.

  An entry lives 24 h and is removed by `dispatch clean`.
- **Who consults it:** `integration::verify_integration` (accept), `orchestrator::check`
  (`--merged`), `attach::finish_locked` and `attach::finish_by_consent`. On a hit nothing runs,
  and the reuse is recorded as `checks.reused`, naming the run and logs it reuses. Output says
  so plainly: "checks: passed, reused from <run> at <time>".
- **`check --merged` applies the unmoved skip** that accept already uses.
- **Attached `finish` checks what would land:** the source as it is now plus Δ, in the check
  workspace (contract B). It falls back to S0 + Δ only when that fails, to show whose change
  broke it. One run then serves both finish and accept unless the source moves in between.
  A finish in a workspace Dispatch can't rebuild keeps today's behaviour.
- **What is recorded per landing:** the reused result is counted as reused, never as time
  saved.

### B. The warm check workspace

- **What it is:** one internal Git repository per project under the state directory. It is
  never a worktree of the user's repository, so discovery never sees it.
- **A run:**
  1. Stage the source's world through a kept index.
  2. Apply Δ with `git apply --cached`.
  3. Run `read-tree --reset -u`, then `git clean -fd`.

  Ignored build caches, and the times of unchanged files, survive between runs, so builds are
  incremental.
- **When it is used:** only under the project's source lock (`apply::source_lock_path`). A run
  that finds the lock held waits, with a message, instead of failing.
- **What each check records:** that it ran warm, and the load average when it started.
- **Removal:** `dispatch clean` removes the workspace. It replaces `source::create_scratch_tree`
  for merged checks.

### C. Native start

- **Hashing:** for Git sources, `source::fingerprint_tree` covers non-ignored files only.
- **Lazy baseline checks:**
  - Before launch, each check's program is resolved, keeping the exit 126/127 guard in
    `native::drive_inner`.
  - Checks run on S0 only for commands the candidate failed, consulting the memo first.
  - The failure classification is unchanged.

### D. Owner and hook costs

**Targets, with 6 Works on this repository:**
- a tick when nothing moved ≤ 100 ms (today about 0.4 s);
- a tick after the source moved ≤ 300 ms (today about 2.9 s);
- a hook with nothing to do ≤ 10 ms at the median (today 202 ms).

**How:**
- **The world, once per signal.** A stat cache keyed by size, mtime, ctime, inode and mode
  serves file hashes, re-hashing anything modified within 2 s of the last look. The baseline
  tree is kept per commit. Facts are derived once per (S0 commit, Δ digest), shared by
  evaluation and footprints.
- **A Work is followed only when its worktree changed:** a status against the kept index
  first, as the fast loop does. Δ is captured once per tick, also in `watch::check`.
- **`run_git`** waits on a thread with `recv_timeout` instead of polling every 20 ms.
- **The hook's fast path decides "not watched" without Git:** watcher records are keyed by
  path. A per-workspace pointer replaces `attach::find_active`'s scan of every run.
- **Runs load by source in one query,** and `State::load_run` stops rewriting `metadata.json`
  on read.
- **`source::inspect_source`** stops reading every `rev-parse` failure as a plain directory;
  only "not a repository" means that. This is the likeliest silent path behind the Hermes
  hook stress failure seen once in 0.4.15.

### E. AGENTS.md

Add the clauses for what this release ships, under a new section "What Dispatch does by itself
(approved 2026-10-17)":
- **Warm checks and reuse:**
  - one check workspace per project, used under the source lock, removed by `dispatch clean`,
    and recorded as warm;
  - a pass may be reused for an identical tree, commands and execution fingerprint, recorded as
    a reuse of the run that produced it;
  - failures are never reused.
- **A measurement line:** "A reused result is counted as reused, never as time saved."
- **Clarification:** a result cache is not a symbol index.

The other approved clauses come with the releases that ship them.

## 4. Packets

| Packet | Contract | Worker | Owns |
|---|---|---|---|
| P0 | §2 probes | integrator | done |
| P1 | A, verification memo | Claude Code | a new `src/coherence/memo.rs`; the memo calls in `integration.rs`, `orchestrator.rs` (`check`), `attach.rs` (finish) |
| P2 | B, warm check workspace | Codex | `source.rs` (`create_scratch_tree` and its replacement), the workspace setup in `integration::run_merged`, `dispatch clean` |
| P3 | C, native start | Hermes | `source.rs` (`fingerprint_tree`), `orchestrator.rs` (baseline checks), `native.rs` |
| P4 | D, owner and hook costs | Claude Code | `serve.rs`, `coherence/world.rs`, `coherence/facts.rs`, `source.rs` (`run_git`, `inspect_source`), `state.rs`, `attach.rs` (`find_active`), `runtime*.rs` (fast path) |
| R | E, docs, version, release notes | Claude Code | docs, AGENTS.md, `Cargo.toml` |

- **Overlap:** P1 and P2 meet in `integration.rs`. P2 owns how the workspace is made; P1 owns
  whether a run is needed. P3 and P4 meet in `source.rs`, in different functions.
- **Landing:** each packet passes `check --merged` before review. The worker exits before the
  user is asked, so its finish checks run while the user decides. Integrator edits are
  committed before an accept.

## 5. Deliberately not built

- An index of interactions (all pairs take about 1 ms at 50 Works).
- Filesystem notifications.
- Test selection.
- A pool of check workspaces.
- Batch landing (0.5.0).
- Anything that speaks to agents (0.4.18).

## 6. Exit criteria (measured on this release's own build)

- Suite runs per landed Work ≤ 1.75 (0.4.15: 2.43), with zero runs on a tree that has a
  recorded pass.
- Warm `cargo clippy --all-targets` in the check workspace ≤ 8 s (cold today: 17–27 s).
- Owner ticks and hook latency meet the targets in contract D, measured the way §2 measured
  them.
- No suite runs before a native agent launches.

## 7. Execution log

Built as planned: workers wrapped by the pinned Dispatch 0.4.15 (`dadc574be248`, state
`~/dispatch-dev/state-0416`), every packet landed with `accept --expect`, and CI validating
each landing on draft PR #26. No coherence override was needed. Three packets conflicted with
packets that landed before them and were re-applied (P1b, P4b, P5b).

### P0 (2026-10-18)

The replay, tick, hook, copy and ignored-input probes in §2, with their scripts in
`~/dispatch-dev/p0-0416/`.

### Packets

| Packet | Worker | Work | Finish | Landed |
|---|---|---|---|---|
| P2 warm check workspace | Codex 0.155.1, gpt-6-sol | `01M56DF92HVPHQVAZQ1EB13PJZ` | CONTINUE, 760 passed | `04eb03a`, `--expect b648d73d7a08`; source unmoved, no rerun |
| P3 native start | Hermes Agent 0.18.2 | `01M56DF928SV91WZ45KKCM0B1P` | CONTINUE, 760 passed | `469b4a7`, `--expect 44bd58963d63`; merged checks on top of P2 (764 passed) |
| P1 verification memo | Claude Code | `01M56DF92H3W7FG46D6YDGQPA7` | 769 passed | not landed: conflicted with P2 in three files; closed as superseded by P1b |
| P1b the memo on top of P2 | Claude Code | `01M56HVJ1T6NGZT6W2TK0G3KRJ` | CONTINUE, 777 passed | `3eab908`, `--expect cec43cfee0df`; merged checks on top of P2 and P3 (781 passed) |
| P4 owner and hook costs | Claude Code | `01M56DF92H3ND2XQ8SH04BM8TA` | CONTINUE, 768 passed | refused at accept: REFRESH, its patch no longer applied to `attach.rs` after P1b; closed as superseded by P4b |
| P4b P4's Δ re-applied | the integrator, no agent | `01M57N0RZN3ZGRSGTETCM5NY2A` | CONTINUE, 793 passed | `4818ef3`, `--expect bd1209c35b10`; source unmoved, no rerun |
| R docs, version | Claude Code | `01M57NMFC0X1XEABYKB9JYG9FS` | CONTINUE, 795 passed | `416d414`, `--expect df8ee4388e77`; source unmoved, no rerun |
| P5 two CI failures | Claude Code | `01M57QK0NC45G4M9ZQ4T5KP00M` | REFRESH, `check` also edited by R; 795 passed | not landed: closed as superseded by P5b |
| P5b P5's Δ re-applied | the integrator, no agent | `01M57VVKJWF7HCEV03M62E727G` | CONTINUE, 797 passed | `e5f2626`, `--expect 987c7e75e830`; source unmoved, no rerun |

- **Conflicts.**
  - P1 and P2 both changed `integration.rs`, `orchestrator.rs` and `clean.rs`: P2 replaced
    the scratch tree that P1's memo was written against. P2 landed first, and P1's patch was
    re-applied on top of it in a fresh workspace (P1b).
  - P4 merged cleanly with P1b in a 3-way test, but accept applies without 3-way, so once P1b
    landed P4's patch no longer applied to `attach.rs` (shifted context, no real conflict).
    The integrator applied P4's frozen Δ 3-way on the new source (one trivial conflict: two
    adjacent doc comments, both kept) and attached it as P4b.
  - R and P5 both edited `check` in `orchestrator.rs`: R one string, P5 one argument, about
    84 lines apart. P5's patch applied cleanly, but 0.4.15 refuses an accept when a landing
    edited the same declaration (`same_symbol_edited`, REFRESH). P5's frozen Δ was applied on
    the new source and attached as P5b, rather than overriding the verdict.
  - All three are evidence for 0.5.0: workers that hear about each other, a stale agent that
    catches up in place (P5's REFRESH rested only on a symbol fact in a file R changed), and
    a landing that applies 3-way (S0's objects are kept).
  - The three superseded finishes cost one suite run each.
- **Review sent work back:**
  - P3 removed strict mode's fence on ignored files; strict mode keeps the whole-tree
    fingerprint.
  - P1b: content-converting attributes or config (LF and CRLF under `text=auto`) made two
    different sources one memo key, a possible false pass; such sources get no key.
  - P4: the per-workspace pointer was dropped in favour of the query, and projection repair
    on read was kept, only when stale.
  - R found four defects in the landed packets, fixed with tests: `clean` cleared the memo
    before you confirmed (P1); an explicit `checks.baseline` no longer ran before the agent
    (P3); `check --merged`'s closing line was stale; a missing check program was reported as
    a failed baseline.
- **Found by CI** (packet P5):
  - `merged_check` failed on Linux in 1 of 5 CI-image runs (2 of 20 in P5's loop):
    `source::capture_world` copied the Git index with a new mtime, so Git trusted stat data
    for a same-size edit made in the second the index was written, and the captured world
    kept the old file. A bug since 0.4.10 that could also leave such an edit out of an
    attached or registered Work's S0. The copy keeps the index's mtime now; 40 of 40 runs
    pass.
  - `auto_apply_concurrency` failed once: P2 had moved the source lock after the gate, so an
    auto-apply could evaluate a source another application was writing. The lock is held
    from evaluation to apply again, as in 0.4.15, and passed down to the merged checks. The
    exact CI run was not reproduced (10 of 10 in Docker); a deterministic test shows the old
    order.
- **CI on PR #26:** green on P2's and P3's commits. P1b's commit failed the four
  `merged_check` tests on Linux and P4b's the `auto_apply_concurrency` test, each passing on
  macOS and under load; R's commit was green, since both failures are intermittent.

### Workers, as their harnesses reported

- **Codex (P2):** 30m53s; 193,362 tokens and 15.2M cached, as reported at exit.
- **Hermes (P3):** 1h16m, 252 messages (235 tool calls); tokens not shown.
- **Claude Code (P1, P1b, P4, R, P5):** usage not reported at exit.

### Against §6

- **Suite runs per landed Work:** 11 for 6 landed Works (1.83; 0.4.15: 2.43), with no tree tested
  twice. Not met as run: three of the 11 were the finishes of superseded Works (8 for 6 without
  them, 1.33), and the build ran on 0.4.15, which has no memo and whose finish checks S0 with
  Δ. Replayed with 0.4.16's finish rule and memo, the same runs come to 9 (1.50). The memo's
  first real use is 0.4.17's build.
- **Warm clippy:** 4.2 s in the check workspace after a one-line edit (21.8 s cold; one pair,
  load averages 53.3 and 48.7). Met.
- **Owner and hooks** (6 Works on this repository, §2's method):
  - a tick when nothing moved: 36 to 64 ms (target ≤ 100 ms). Met.
  - a tick after the source moved: 86 to 334 ms; a move that records verdicts took 311 to
    427 ms in 6 of 15 samples, mostly at load averages above 10 (target ≤ 300 ms). Met at
    ordinary load, not under heavy load.
  - a hook with nothing to do: 4.0 to 4.4 ms at the median at load averages 4.3 to 5.1
    (target ≤ 10 ms), and 8 to 20 ms at load average 30. Met at ordinary load.
- **No suite before a native agent launches:** met, unless `checks.baseline` asks for one.

## Carried, not in this release

- A guard for a capture that overlaps a removal (0.4.15 P5's note).
- Ties between same-second modification times in `freshest_patch` (filesystems with
  one-second times only).
- Accept applies Δ without a 3-way merge, so shifted context refuses a patch with no real
  conflict (P4). For 0.5.0's batch landing.
- A REFRESH that rests only on a symbol fact a landing changed (P5) is the case 0.5.0's
  catch-up is for.
- A native run's snapshot still copies and walks ignored build output (P3: 121 s to start
  with a 3.8 GB `target/` at load average 39). Candidate: clone the tree (`clonefile` on APFS,
  reflinks on Linux).
- The 0.4.15 stress failure in `hermes_worktrees`, still unexplained. It did not recur in
  this build's CI; `inspect_source` now reports Git failures instead of treating them as a
  plain directory (P4).
