# Plan: Dispatch 0.5.0, Dispatch coordinates

**Approved, 2026-11-09.** Base: `764336b` (Dispatch 0.4.18). The last step of the roadmap to 0.5.0
(`docs/roadmap-0.5.0.md`). Wave 1 is built under the pinned released 0.4.18 and wave 2 under a release
build of this branch. Packets are landed with `accept --expect`, and CI validates each landing on a draft
PR. If any of the six conditions is unmet at the end, the same content ships as 0.4.19.

## Context

0.5.0 is the last step of the roadmap approved 2026-10-17 (`docs/roadmap-0.5.0.md`). Dispatch stops being
only a gate that checks agents' work and becomes a coordinator they work through. It is tagged only when
the roadmap's six conditions hold. Otherwise the same content ships as 0.4.19 (user, 2026-11-09).

Where the conditions stand after 0.4.18 (main `764336b`):

| # | Condition | State |
|---|---|---|
| 1 | Agents know about each other | Built in 0.4.18 but never measured on real work: its workers ran under 0.4.17, which writes no notes |
| 2 | Stale work catches up in place | Not built. Stop only records "turn ended", and notes ask nothing |
| 3 | Landing is one decision | Not built. `accept` with no id refuses when 2 or more results wait |
| 4 | Out of the box | Met in 0.4.17 (install line, `setup`, `accept`) |
| 5 | Fast | Native start 3.3 s, 1.71 suite runs per landed Work. "Lands in seconds" not measured |
| 6 | Proven on itself | Needs a build with overlapping packets coordinated through Dispatch, plus four-harness coordination trials |

Hand re-applies were 4 in 0.4.17 and 1 in 0.4.18, all `patch_conflict` from changed context. Landing Work
one at a time also refuses the second Work of any "changes / uses" or same-declaration pair (REFRESH from
`FactBroken` or `SameSymbolEdited`), so overlapping work can't land together today.

**Decisions (user, 2026-11-09):**
- 3-way landing is in.
- Catch-up for Claude uses a Stop block at the end of a turn, and an `asyncRewake` waiter for an idle
  session.
- Approved: P0 probes on real Claude Code and Hermes sessions; the four-harness conformance trials; a
  candidate supervisor (a release build of the branch) for wave 2.
- If any condition is unmet, the release ships as 0.4.19.
- Standing from the roadmap: coordination means aware agents, a safe landing order and catch-up in place,
  not task splitting. One approval per batch. Catch-up is symbol-level only, once per landing, and never
  relaunches. No commit on land.

## Evidence (code at `764336b`; three read-only maps and one adversarial review, 2026-11-09)

- **Accept** (`orchestrator.rs` `decide` 2211, `review_locked` 3375; `apply.rs` `apply_locked` 382):
  - The source lock is held from evaluation to the apply.
  - `coherence::gate` (coherence.rs:461) evaluates afresh. L0 is `git apply --check`, which gives
    `PatchConflict`; L1 is `facts::check`; L2 is the merged checks in the warm check workspace.
  - `source::apply_checked` (source.rs:1085) checks the world-digest fence **twice per call** (1101, 1144).
    It reads the candidate's `diff_path` and applies to the working tree only.
  - `persist_applied` (apply.rs:181) records `result.applied`, `interaction.landed` and `s0::record`.
    `s0::record` makes one `refs/dispatch/source` commit, whose parent is HEAD, and appends a
    `sources.json` entry.
- **No id:** `sole_awaiting_review` (orchestrator.rs:3556) is the 0.4.12 exactly-one rule.
- **Trees and the world:**
  - `memo::merged_tree` (memo.rs:111) and `prepare_check_workspace` (source.rs:1269) each build a merged
    tree with one plain `git apply --cached`. The check workspace has its own `.git`, and its index equals
    its checkout.
  - L0 runs `git -C <source> apply --check`. L1 reads S1 from `<source>/<path>` for `world.changes` only
    (facts.rs:1196, 1300). `world::observe` lists with `ls-files -co --exclude-standard`.
  - So pointing `WorkView.source` at the check workspace (Git sources) evaluates a member against any
    tree.
  - The check workspace's digest is not the root's: scratch copies include nested repos' files
    (source.rs:1176), and the world skips them.
  - L1 can't run *on* a tree that already contains Δ: every edited declaration would read as
    `SameSymbolEdited`.
- **3-way inputs:**
  - Patches carry full blob ids (`--binary --full-index`, source.rs:682).
  - The S0 pre- and post-image blobs are in `runs/<id>/baseline/.git/objects` (source.rs:2102).
  - CI's Linux image pins Git 2.39.5 (`ci/test.Dockerfile:16`). `merge-tree --merge-base` needs a newer
    Git, so the tool is `git apply --3way --cached`.
- **Order:**
  - `interactions::footprint`, `settle`, `between` and `edges` are pure and public. Frozen Work goes
    through `settle(fresh, true, None)`, as the owner does (serve.rs:1326).
  - `Uses` and one-writer `File` edges are directed by `writer`. `SameDeclaration`, `TextualOverlap` and
    two-writer `File` edges are not.
- **Catch-up data:**
  - `interaction.landed` (`measure::Landing`, status `Observed`) carries counterparts with symbol-level
    interactions, including live `Working` Work (serve.rs:1564, landing.rs:39).
  - "Breaks what this Work uses" is `Uses`, `writer` = the landed side, `Target::Symbol`, `change` in
    {Signature, Removes}. "Edited the same declaration" is `SameDeclaration` on a `Target::Symbol`.
  - `inbox::settle` parses every `*.json` in `inbox/` as notes (inbox.rs:255), so catch-ups need their own
    directory.
- **Claude hooks** (`runtime/claude.rs`):
  - `Stop` (timeout 5) leaves a "turn ended" mark. A Stop payload over 64 KiB prints "could not track"
    (57-61, main.rs:741).
  - `head` reads only string fields.
  - `HookSet` holds (event, matcher, timeout) for one command. `ours()` accepts only `… hook claude`
    (295), and `with_set` writes only `command` and `timeout`.
  - Claude Code writes the end of a turn to the transcript after Stop returns (`TURN_SETTLE` 10 s,
    attach.rs:1350).
  - A resumed `--worktree` session reports SessionStart from the main checkout (runtime.rs:194).
- **Claude Code 2.1.296 docs** (`code.claude.com/docs/en/hooks`, read 2026-11-09):
  - **Stop:** `{"decision":"block","reason"}` keeps Claude working with `reason`. `stop_hook_active` is
    true on a continuation, and there is a cap of 8 consecutive continuations.
  - **Background hooks:** `asyncRewake: true` runs in the background and "wakes Claude immediately even
    when the session is idle" on exit 2, with stderr shown as a system reminder. Claude Code enforces
    `timeout` (default 600 s, no stated maximum), and each execution is its own process.
  - Behaviour mid-turn and at exit isn't documented.
- **Hermes 0.18.2:** `pre_verify` fires at the end of a turn that edited files. `{"action":"continue",
  "message"}` keeps the agent going, bounded by `agent.max_verify_nudges` (`agent/conversation_loop.py`
  ~5280). Hermes can't be woken when idle.
- **S0:** hook-registered `claude --worktree` Work is Attached with no owner, so it can be moved
  (s0.rs:84). Wrapped Work keeps its S0, and the 0.4.18 build's workers were all wrapped.
- **Older items rechecked.**
  - Open:
    - a broken pipe panics;
    - `status` with no Work skips "Found, not tracked" (orchestrator.rs:1737);
    - `presenter::session()` runs `git` from `PATH` (presenter.rs:1688);
    - watch and the CLI give different REFRESH next steps (watch.rs:541, orchestrator.rs:1018).
  - Fixed: false REFRESHes from named files, and the version probe.
  - Left out (no real failure): the overlap guard and `freshest_patch` ties.

## Packets

### P0 Evidence (integrator; `~/dispatch-dev/p0-0500/`)

1. **Claude probe (approved).** A scratch repo with probe hooks only in its `.claude/settings.local.json`.
   Payloads are saved as fixtures.
   - **Stop block.**
     - Does the model act on `reason`, and what does the person see?
     - Is `stop_hook_active` true on the next Stop?
     - Does an `idle_prompt` Notification still follow?
     - Record the Stop payload's field order: is `stop_hook_active` before `last_assistant_message`?
   - **asyncRewake waiter.**
     - Does it wake an idle session, and what does the person see?
     - What happens if it exits 2 during a turn?
     - Is a 1 h, then 8 h, `timeout` honoured?
     - What happens to the waiter at `/exit`, Ctrl-D and a hangup?
     - Is it listed in later `background_tasks`?
     - Are hooks re-read mid-session?
2. **Hermes probe (approved).**
   - Does `pre_verify` fire on an edit turn, with `TERMINAL_CWD`?
   - Does the `continue` message reach the model?
   - What is the default `max_verify_nudges`?
   - Does the allowlist prompt once?
   - The config is restored byte for byte.
3. **Git.** `git apply --3way --cached` with `GIT_ALTERNATE_OBJECT_DIRECTORIES`, on Dispatch's own
   patches. Run it on Git 2.39.5 (the CI image) and on the macOS runner's Git (one CI job prints
   `git --version`).
4. **Baselines:**
   - accept wall time (unmoved, and with a memo hit);
   - manual steps per landing (0.4.18's log);
   - Stop hook cost today.

If the waiter can't wake an idle session cleanly, or it survives `/exit`, the idle half of P2 is dropped and
recorded. Condition 2 then rests on the Stop block.

### P3 3-way merge and the landing seam (Claude Code, Opus 5.5; lands first of P1 and P3)

- **One stager.** `stage(base, patches) -> Tree | Conflict` applies ordered patches into an index.
  - Plain `apply --cached` first. If a patch fails, `git apply --3way --cached`, with that run's baseline
    objects as alternates.
  - The root's index and HEAD are never touched.
  - `prepare_check_workspace`, `memo::merged_key` (which takes the staged tree) and P1 all use it.
  - It is refused (no 3-way) for sha256 repositories and converting sources, whenever `memo::world()` is
    `None`.
- **L0 (in `assess`, so check, status, watch, the owner and accept agree).** When `apply --check` fails,
  stage 3-way on the world commit:
  - A conflict is REFRESH `PatchConflict` as today.
  - A merged tree equal to the world's is STOP `AlreadyApplied`.
  - Otherwise, a new CONTINUE reason `merged_three_way`.
  - L1 then runs as today **against the world**, never on the merged tree.
  - `merged_three_way` counts in `rests_on_merged_checks`, so a 3-way result whose checks are skipped for
    any reason becomes REFRESH.
- **The landing seam.**
  - `apply_checked` takes the patch to apply explicitly.
  - A single 3-way landing applies `runs/<id>/delta-landed.patch` (world tree → merged tree) with the
    fence.
  - Δ stays as frozen. `--expect` binds Δ's digest, and events record both digests.
  - `apply_locked` is split into `land(run, validity, patch, fence)` and the recording tail, so P1 can
    reuse both.
- **Tests:**
  - context lines changed with no overlapping edit: lands 3-way, checks run;
  - overlapping edits stay REFRESH;
  - a binary file changed on both sides stays REFRESH;
  - skipped checks give REFRESH;
  - failing checks refuse;
  - the root index is untouched;
  - both digests are recorded;
  - status, check and accept agree.

### P1 Batch landing (Claude Code, Opus 5.5; built in parallel against P3's seam, lands after it)

- **When it applies.** `dispatch accept` with no id, for Git sources, when 2 or more results are
  eligible:
  - finished, ready, pending review and unapplied;
  - verification passed or not configured;
  - the same frozen `checks.verify`, execution and authority as the batch.

  Others are listed as "accept it on its own". One eligible result, `accept <id>` and directory sources
  behave as today.
- **Order.**
  - Kahn's algorithm over the eligible set's edges, recomputed without an owner. A `Uses` writer comes
    before its reader, and a one-writer `File` writer before the other side.
  - Symmetric edges and cycles are ordered by finish time, then id.
  - Changer first is the safe order: the user's break is then caught by L1 at the gate instead of landing
    silently.
- **Evaluation.**
  - Under the source lock, stage P0 in the warm check workspace once. For each member:
    - check out T(k-1) with `read-tree --reset -u` and `clean -fd`;
    - evaluate it with `WorkView.source` = the workspace, observed as Git with its own `Worlds`;
    - on CONTINUE, stage Δk to get T(k), 3-way if needed;
    - on REFRESH or STOP, the member leaves with its reasons.
  - Per-member validities are proposal data only: never stored, never a fence.
  - The fence D0 is `world::observe(root)`.
- **Proposal and approval: one path.**
  - Compute the proposal under the lock, then release it. Print:
    - the ordered members, with id, task, Δ digest and why each one comes where it does;
    - those left out, and why;
    - the batch digest, sha256 over the ordered (run id, Δ digest) pairs;
    - that if the combined checks fail, only the prefix before the failure lands.
  - In a terminal, a menu with Cancel focused. Confirming re-runs the same thing as `accept --expect
    <batch digest>`.
  - Without a terminal, it prints the `--expect` command and exits non-zero. This replaces the
    "N results wait" error.
  - `--expect` is a 12 to 64 hex prefix of the batch digest. A member's Δ digest there is refused, with
    the advice to name its id.
- **Verification (`--expect`).**
  - Re-evaluate under the lock. A different ordered set is refused, and nothing applies.
  - The merged checks run once on T(n). They are recorded on the first member (`coherence.merged_checks`
    with `batch`), and on the others as `checks.reused` of that run.
  - If no member has checks configured, the batch lands on L0 and L1, as a single accept would.
  - **On failure:** binary-search the prefixes for the boundary between a passing and a failing prefix
    (at most ⌈log₂ n⌉ more runs, each tree once, recorded in the memo). The prefix before it lands, and
    the first failing member is reported with its logs.
    - If T(1) fails, nothing lands, and the message says the source's own checks may be failing.
    - This is not a retry: every run checks a different tree.
- **Landing.**
  - Take every member's run lock (in id order) before the source lock.
  - The verified prefix lands as **one patch**: `git diff --binary --full-index T0 Tm` from the workspace
    repo, applied with one fenced `apply_checked` against D0. Nothing half-lands.
  - Then, for each member in order, the recording tail:
    - `result.applied`;
    - `interaction.landed` with `batch`, D0 and D(m), whose counterparts exclude co-batched members;
    - `review.accepted {batch}`;
    - the workspace release.
  - One `refs/dispatch/source` commit for the batch, with one `sources.json` entry per member, all
    naming that commit.
  - Every path the patch touches must then hold its verified blob.
  - One shared `Worlds` for all the hashing.
  - Never calls `decide` per member.
- **Events:** `batch.proposed`, `batch.verified {tree, passed, runs}` and `batch.landed {members, tree}`,
  on each member run.
- **Tests:**
  - a Uses pair: changer first, the user left out as REFRESH;
  - three independent Works: one merged-check run;
  - a same-declaration pair: the later one left out;
  - a member that needs 3-way;
  - an `--expect` mismatch, and a changed set, both refuse;
  - declining applies nothing;
  - bisection's boundary and run count;
  - T(1) failing lands nothing;
  - per-member records equal a single accept's, with co-batched members excluded from counterparts;
  - verified blobs in the root;
  - a directory source behaves as today.

### P2 Catch-up in place, Claude (Claude Code, Opus 5.5)

- **Trigger (owner, each tick, after `notify::tell`).**
  - A recorded landing with an `Observed` `interaction.landed`.
  - Live hooked Work: Claude, `Working`, its session not ended, its workspace present, in a watched
    project.
  - `Uses` with `writer` = the landed side on a `Target::Symbol` with `change` in {Signature, Removes}, or
    `SameDeclaration` on a `Target::Symbol`. Never file-level, `TextualOverlap` or a reverse `Uses`.
- **One pending catch-up per Work.**
  - It names every triggering landing since its last ask, each at most once.
  - "Asked" is recorded in `Kept` before the file is written, so a crash gives at most one ask.
  - It expires if the Work's S0 has already moved to a record at or after those landings, or when the
    Work leaves `Working`.
  - 0.4.18's notes are unchanged. The catch-up is a separate ask.
- **Text.**
  - Fixed, at most about 8 lines: the landed Works and their runtime, each target with old → new
    signature when known, and "These changes are in `refs/dispatch/source` (commit `<sha>`)."
  - It asks: commit your work, `git merge refs/dispatch/source`, update what uses them, and run the
    project's checks. It says Dispatch asks once per landing.
  - It never says "you must".
- **Files:** `state/catchup/<key>.json` and `state/catchup/delivered/`, with a settle step of their own.
  The hook side opens no SQLite, takes no lock, runs no Git and makes no network call.
- **Stop (sync).**
  1. `head` reads `hook_event_name`, `cwd` and `stop_hook_active` (extended to booleans) before `parse`.
  2. If `stop_hook_active`, nothing is claimed.
  3. Otherwise claim by rename, print `{"decision":"block","reason":…}`, and skip `ingest` (no needs-you
     mark).

  Any payload size works on this path.
- **Idle waiter.**
  - A second Stop entry, `hook claude --wait`, with `asyncRewake: true` and P0's timeout.
  - It exits 0 at once when the project is unwatched.
  - It polls each second, and claims only once the transcript's mtime has settled after `TURN_SETTLE`
    and hasn't changed since. A new turn is the next Stop's to handle. P0 may drop this rule.
  - On a claim: stderr, then exit 2.
  - It exits 0 when a newer waiter replaces it (`state/waiters/<session>.pid`), when the session's
    process is gone (the worktree lock's pid and start: one `ps`, then `kill(pid, 0)`), or before
    Claude's timeout.
  - A hidden max-wait flag is for tests, and tests kill children on drop.
- **Hook sets.**
  - `HookSet` gains a command suffix and an async flag. `ours()` accepts both suffixes.
  - `INSTALLED` gains the waiter, and `EARLIER` gains 0.4.18's set.
  - Install, uninstall and upgrade round-trip tests, including two Stop entries.
- **Events:** `catchup.written {landings, targets, text}`, `catchup.delivered {via: stop|wake}` and
  `catchup.expired {why}`. Success is measured: S0 moved to a record at or after the landing, then the
  next verdict.
- **Tests:**
  - the trigger matrix;
  - grouping and once-per-landing under concurrent hooks;
  - the expiry when S0 has already moved;
  - Stop JSON, with `stop_hook_active` and a 1 MB payload;
  - the waiter's claim, supersede, parent-gone, unwatched and timeout cases, with no process left after
    the suite;
  - the hook-set round trips;
  - a stand-in end to end: landing, then the ask, then the merge, then S0 moves, then CONTINUE.

### Q Small defects (Codex; reviewed; wave 1)

- A broken pipe exits quietly (CLI only, never the owner).
- `status` and `status --json` show "Found, not tracked" when no Work exists.
- `presenter::session()` runs Git through `plain_git_command`.
- One regression test each.

### P5 REFRESH next steps (Claude Code, Opus 5.5; wave 2)

- One REFRESH next step for watch (watch.rs:541) and the CLI (`next_step`, orchestrator.rs:1018).
- `patch_conflict` says what 3-way already tried.
- **Overlaps P2h on purpose,** at `next_step` and watch's REFRESH detail (see Order).

### P2h Catch-up for Hermes, and how asked Work reads (Claude Code, Opus 5.5; wave 2)

- **Hermes:** a `pre_verify` hook returns `{"action":"continue","message"}` with the pending catch-up, if
  P0 confirms it. Otherwise it arrives at the next turn through `pre_llm_call`.
  - The Hermes block grows to three hooks, and `EARLIER` becomes a list.
  - Setup says the allowlist asks once more.
- **Status and watch:** asked Work shows "asked to catch up with `<landing>`" and when. Its REFRESH next
  step says the agent was asked, and to check again once its S0 moves.

### P4 Queries (Hermes; reviewed; wave 2)

- **`docs/queries/catchup-measured.sql`:**
  - asks, deliveries by `via`, and expiries;
  - whether S0 moved to a record at or after the landing, and how long after;
  - the next verdict;
  - unsettled asks.
- **`docs/queries/batches-measured.sql`:**
  - proposed, left-out and landed members;
  - merged-check runs, bisection included;
  - suite runs and approvals per landed Work.
- Counts only. Agreement only with Dispatch's own later verdict. Tested against a fixture database.

### C Coordination conformance (integrator; approved; on the release build)

- `docs/harness-conformance.md` gains coordination cells for Claude, Codex, Cursor and Hermes:
  - notes (Claude and Hermes; "none, no hook" for the others);
  - catch-up (Claude by Stop and wake; Hermes by `pre_verify` or the next turn);
  - S0 follows the agent's merge;
  - batch landing.
- Scratch repos with two overlapping tasks each. Evidence in `~/dispatch-dev/conformance-0500/`. Defects
  become fix packets, and C runs again.

### R Docs, AGENTS.md, version, release notes (Claude Code, Opus 5.5, last)

- **Version:** 0.5.0, or 0.4.19 if a condition is unmet.
- **Release notes:** "Dispatch coordinates", ending with `Site impact:` (accept's output, the new hooks).
- **AGENTS.md, under "What Dispatch does by itself":**
  - **Catch-up:** symbol-level, once per landing, through hooks, fixed text. Never a relaunch, a merge on
    the agent's behalf or a second ask. It amends 0.4.18's "asks nothing of them".
  - **Batch landing:** one approval, and only a verified prefix lands.
  - **3-way:** never without merged checks.
- **Docs:**
  - `reference.md`: accept, and the events;
  - `attach.md`: catch-up and the waiter;
  - `coherence.md`: order, 3-way;
  - `coherence-validation.md`: claims;
  - the roadmap's status;
  - "Upgrading to 0.5.0": hooks re-offered once, Hermes asks once more, one sleeping waiter per idle
    Claude session in a watched project.

## Review gate for non-Claude packets (Q, P4)

As in 0.4.18:
- a line-by-line review before landing, in `notes-0500/review-non-claude.md`;
- contract tests by a Claude worker pinned to `--model claude-opus-5-5`;
- fixes in a "b" packet by Opus 5.5 that lands together with the original, never alone;
- models proven from transcripts.

Safety-critical code stays with Opus workers: the stager, the root apply, the batch fence, the hook fast
path and the waiter.

## Order, workers, landing

- **Setup:**
  - Supervisor: the pinned 0.4.18 (`~/dispatch-dev/dispatch-0.4.18/`), state `~/dispatch-dev/state-0500`.
  - Clone: `~/dispatch-dev/integration-0500` on `release-0.5.0` from `764336b`, with the plan as its first
    commit.
  - A draft PR for CI, asked for first.
- **Claude workers** run as `claude --worktree <name> --model claude-opus-5-5` from the clone.
  - The clone's `.claude/settings.local.json` holds the supervisor's hooks (state-0500) and
    `worktree.baseRef: head`. The user's `~/.claude` is not touched.
  - Each worker is hook-registered, movable Work, so notes, S0 moves and catch-up reach it.
  - The user gives standing consent for the clone once (`dispatch setup`, pinned binary).
  - Codex and Hermes run wrapped as before, with no notes.
- **Wave 1 (pinned 0.4.18):** P0, then P3, P1, P2 and Q in parallel.
  - P1 and P3 overlap in the stager, gate and seam. P3 lands first. P1 then hears of it through 0.4.18's
    notes and can merge `refs/dispatch/source`, so its S0 follows. These are the first real measures of
    condition 1.
  - Landings are one at a time with `accept --expect`.
- **Switch,** once P1, P2 and P3 have landed, CI is green and no wave-1 Claude session is live (Claude
  Code reads hooks at session start, unless P0 shows otherwise):
  - build the head for release into `~/dispatch-dev/candidate-0500/` (read-only);
  - `dispatch stop` the 0.4.18 owner;
  - clone the state to `state-0500-switch` (kept);
  - start the candidate on `state-0500`;
  - point the clone's hooks at it.

  After the switch, only the candidate can read `state-0500`, because of the new reason code. Rolling back
  means the kept copy and 0.4.18. Recorded either way.
- **Wave 2 (candidate):**
  - P5 and P2h, two live Claude Works that change the same declarations, so whichever lands second is
    asked to catch up;
  - P4;
  - fixes from wave 1's reviews.

  Two or more ready results land through one `dispatch accept` proposal. The integrator reviews every
  member's diff, the user approves, and the batch lands with `accept --expect <batch digest>`.
- **R last,** then C on the release build of the final head.
- **CI** must be green on all three jobs after every landing.

## Exit criteria (measured on this build; all six for 0.5.0)

1. **Agents know about each other.** Real notes delivered on this build's work, with `notes_unsettled` 0
   and agreement counts from `notes-measured.sql` (no target).
2. **Catch-up in place.**
   - At least one catch-up on this build's own work was delivered and followed by its agent's S0 moving
     to a record at or after that landing, then CONTINUE, with no relaunch.
   - Every ask is once per (landing, Work), and none comes from file-level facts. Counts from
     `catchup-measured.sql`.
3. **One decision.**
   - Every wave-2 landing of 2 or more ready results is one approval.
   - One merged-check run per passing batch, with bisection runs counted.
   - Approvals and manual steps per landed Work, against about 8.
4. **Out of the box.** Install line, `setup` and `accept` on a temp HOME with the release build: at most
   3 commands and no flags.
5. **Fast.**
   - No passing tree verified twice (`coherence.merged_checks` by `tree_id`).
   - An already verified result lands within 10 s (p50 of 10).
   - Native start at most 10 s.
   - Suite runs per landed Work at most 1.75.
6. **Proven on itself.**
   - Wave 2 is supervised by the candidate, with at least one symbol-level interaction between live
     Works recorded.
   - The four-harness coordination cells pass.
- **Also:**
  - Stop p95 at most 15 ms with a pending catch-up.
  - An idle waiter uses no measurable CPU, and no waiter or owner is left after the full suite.
  - Re-applies by hand against 0.4.18's 1.
  - Review gate met, models verified, and production LOC reported (estimate +1.3k to +1.8k).

## Not built

- Splitting a task among agents.
- Relaunching an agent, or merging or refreshing on its behalf.
- Catch-up for Codex and Cursor (no hook), and an idle wake for Hermes.
- Delivery on a resumed session's start.
- S0 moves for wrapped Work while its wrapper lives.
- Batches for directory sources, and from watch's keys.
- Landing members after a failing one; automatic retries; commit on land.
- Warm build seeding (0.4.18's M). The overlap guard and `freshest_patch` ties.

## Verification

- **Each packet:**
  - `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`;
  - finish, then accept;
  - CI on the draft PR (Linux, macOS, stress).
- **End to end, on a release build in a temp HOME with stand-in Claude Code sessions** (as
  `rel-0418/e2e.py`):
  1. Three ready Works (independent, a changer, its user). `dispatch accept` proposes the changer first
     and leaves the user out as REFRESH. One merged-check run, then one landing: the verified blobs in the
     root, one `refs/dispatch/source` commit and an entry per member.
  2. Live B uses f, and A lands a change to f's signature.
     - Mid-turn, B's Stop gets one block.
     - Idle, B's waiter exits 2 once.
     - B merges, S0 moves, the verdict is CONTINUE, and B lands.
  3. Changed context lands 3-way with checks. An overlapping edit stays REFRESH.
  4. A batch whose combined checks fail: bisection names the boundary, and the prefix lands.
- **Hook timing:** Stop p95 over 1,000 calls, with and without a pending catch-up. Waiter CPU while idle.
- **Queries:** `notes-measured.sql`, `catchup-measured.sql` and `batches-measured.sql` on `state-0500` at
  the end.

## Execution log

- 2026-11-09: plan approved. Branch `release-0.5.0` from `764336b`.
