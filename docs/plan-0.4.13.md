# Plan: Dispatch 0.4.13, Work that appears and ends by itself

**Draft, 2026-10-13.** Base: `8fc2fdc` (Dispatch 0.4.12). Follows
`docs/plan-0.4.12.md` §7 (the 0.4.13 follow-up). The work runs as parallel
packets, built through Dispatch 0.4.12 itself.

## 1. Objective

A person runs `dispatch start` in a project and works with Claude Code the way
they already do. Work appears, says whether its agent is still running, and
says when the agent has gone, with no hooks installed. Dispatch infers
nothing it cannot observe.

- **Discovery without hooks.** Claude Code worktrees created while the project
  is watched become Work. Worktrees already there at the first watch are listed
  with an attach command; they are not turned into Work.
- **Liveness from what the runtime records.** A Claude worktree's Git lock
  names its session's process. When that exact process is gone, the session has
  ended, including `claude -p` sessions, which send no `SessionEnd`. Ending a
  session never finishes Work.
- **Live progress that is not fake.** Working Work shows the files it changes
  now and how long it has been tracked. It never shows unset delivery fields as
  "0 files" or "0.0s".
- **First use without a terminal ceremony.** Project setup is separate from
  provider funding. Non-interactive setup can save exact check commands, but it
  cannot grant standing host authority.

Dispatch stays harness-neutral. Recognizing a runtime's worktrees and reading
its lock belong to that runtime's adapter (`src/runtime/claude.rs`). For an
agent that makes no worktrees, Dispatch's own workspace is the path:
`dispatch attach -- <agent>`.

## 2. Evidence (P0, observed 2026-10-13 with Claude Code 2.1.296)

From `~/dispatch-dev/trial-0412/` (plan-0.4.12 §9, "Real-agent trial"), plus one
interactive probe:

| Signal | Observed |
|---|---|
| `.git/worktrees/<name>/CLAUDE_BASE` | Present for every worktree Claude made (`claude --worktree`, `claude -p --worktree`), holding the commit it branched from. Absent for a worktree a person made (`git worktree add`) |
| `git worktree lock` reason | `claude session <name> (pid <pid> start <Www Mmm dd HH:MM:SS YYYY>)`, start in UTC; `TZ=UTC0 ps -o lstart= -p <pid>` prints the same text for the live process |
| Running interactive session | Locked, the PID live, start equal |
| Interactive session after `/exit` | Lock removed; a clean worktree is removed by Claude itself |
| `claude -p` session after it exits | Lock left in place with a PID that no longer runs; no `SessionEnd` sent |
| Worktree session `SessionEnd` | `cwd` = the main checkout (fixed in 0.4.12 P4: matched by session) |
| This repository | 15 linked Claude worktrees (14 `agent-*`) from September, none locked |

Not yet observed: the lock reason of a subagent (`isolation: worktree`) or
background-session worktree, and the format on Linux. Until they are observed,
only a lock reading `claude session …` counts as a session.

## 3. Contracts

### A. Shared: what a Claude worktree records (P0, integrator)

`runtime::claude::worktree_signals(gitdir) -> WorktreeSignals`, where `gitdir` is
the worktree's `.git/worktrees/<name>` directory:
- `base: Option<String>`: `CLAUDE_BASE`'s commit, a hint of provenance, never S0;
- `lock: Option<Lock>`: the parsed `locked` file, as `Session { name, pid,
  start }` for `claude session <name> (pid <pid> start <text>)`, or `Other(text)`;
- `fn liveness(&Lock) -> Liveness`: `Running` when the PID runs with that exact
  start, `Gone` when it does not run or runs with another start, `Unknown` when
  the start cannot be read. It is thin and adapter-owned, comparing with
  `TZ=UTC0 ps -o lstart=`.

P0 also adds two empty modules the owner tick calls, `discovery::scan` and
`liveness::observe`, so the workers do not edit the tick. P0 also adds the
`Inventory` record type below.

### B. Discovery without hooks (packet P1)

- **Each tick:** the owner runs `git worktree list --porcelain` for the watched
  root. That is cheap, and it never walks a directory. For each linked worktree
  that is not the root, the adapter decides whether its runtime made it: for
  Claude, `CLAUDE_BASE` present and the path under `<root>/.claude/worktrees/`.
- **Inventory:** on the first scan of a watch, every recognized worktree goes into an
  `Inventory` (`watchers/<key>.inventory.json`: path, first seen, base, lock
  reason). The inventory persists across owner restarts. These worktrees are never
  registered automatically; `status`/`watch` list them as "found, not tracked"
  with `dispatch attach --workspace <path>`.
- **Registration:** a recognized worktree that appears later, whose lock reads
  `claude session …` with a `Running` process, becomes Work through the existing
  registration and publication path, under the workspace lock:
  - S0 is its merge base with the root. A worktree that appeared after watching
    began holds only its session's changes, so confidence is full.
  - Provenance is a new `Discovered { commit }`, shown as "found by Dispatch;
    started from <commit>".
  - A hook registering the same worktree, before or after, reconciles as
    today: one active Work per workspace, and a later `SessionStart` joins it.
- **Not registered:** a recognized worktree with any other lock, or none, stays
  in the inventory as attachable. That covers subagent and background worktrees
  until their lock is observed. Human worktrees are never registered.
- **Cost:** one active-workspace set built from the loaded runs per tick, never
  `find_active_attachment` per worktree. No notice spam: a worktree that fails
  to register is noted once.
- **Untouched:** no change to hooks, `attach`, or Claude settings.

### C. Liveness from the lock (packet P2)

- Each tick, for unfinished attached Work in a recognized worktree, the owner
  reads `worktree_signals` and `liveness`:
  - `Running`: record the agent's process identity on the attachment once
    (`agent_process`, which already exists) and show "agent running".
  - The lock is gone, or `Gone`, after Dispatch recorded an exact `Running`
    identity: record the open session's end (`runtime.session_ended`, reason
    `process_gone` or `lock_released`), exactly as a hook end would. The Work
    becomes `idle`, with 0.4.12's "session ended — finish, resume or close".
  - `Unknown`, or never seen `Running`: record nothing. Unknown never proves an end.
- Never finish, apply, close or kill. Auto-finish stays limited to exact
  workspace removal with consent.
- A resumed session in the same worktree locks it again. A new session is
  recorded as today (hooks), or noted as running again (no hooks).

### D. Live progress (packet P3)

- **Working Work** shows files changed now, from the owner's live view of the
  workspace (persisted in the owner's projection each tick it follows Work). It
  also shows "tracked for 12m" from the attachment's start. Process runtime is
  unknown unless observed.
- **Status and watch** render "files: unknown" before the owner has looked,
  never "0".
- **The inventory:** P3 renders C's/B's inventory ("found, not tracked") in
  `status`, `watch` and `watch --json` from P0's `Inventory` type, while P1
  fills it.

### E. First use (packet P4)

- **`dispatch start` on a project without checks:** interactively, it suggests
  checks from the existing detector, shows the exact commands, and offers
  "continue without checks". Non-interactively, it watches at once and
  reports that no checks are configured.
- **`dispatch setup --checks --verify '<cmd>' …` without a terminal** writes
  the exact commands to `dispatch.yml` and says that standing consent still
  needs `dispatch setup --checks` in a terminal, or the per-invocation
  `--allow-unsafe-local`. It never grants consent.
- **Consent:** the consent text says checks run project code, including
  code the agent wrote, on the host with the person's permissions.
- **Untouched:** no implicit `git init`, commit or dependency install, and no
  change to funding setup.

## 4. Packets, parallel

P0 lands first. Then P1–P4 run at the same time, each in its own Dispatch-made
workspace, owning its own files. Each lands one at a time after human review;
later packets are rechecked against what landed (Dispatch's interactions and
verdicts show any overlap). R closes the release.

| Packet | Owns | May touch only for its wiring |
|---|---|---|
| P0 (integrator) | `runtime/claude.rs` `worktree_signals`/`liveness` + tests, stub `orchestrator/discovery.rs` and `orchestrator/liveness.rs`, `Inventory` type, two calls in `serve.rs` tick, plan | — |
| P1 discovery | `orchestrator/discovery.rs`, discovered registration in `registration.rs`/`attach.rs`, `BaselineProvenance::Discovered` | `models.rs` (the variant) |
| P2 liveness | `orchestrator/liveness.rs`, `process.rs` (if needed) | `attach.rs` `record_session_end` reasons |
| P3 live progress | `presenter/watch.rs`, `presenter.rs`, status rendering in `orchestrator.rs`, live counts in the owner's projection | `serve.rs` follow code |
| P4 first use | `setup.rs`, `presenter/setup.rs`, `consent.rs` wording, `background.rs` start, `main.rs` setup args | — |
| R (docs) | release notes, upgrade notes, `attach.md`, `product-guide.md`, README, version | — |

Each packet:
- adds focused regression tests that fail on the old code;
- runs fmt and clippy, and runs focused tests while working;
- runs the full suite once at hand-off, because four parallel full suites on this
  Mac cause timing flakes;
- reports production LOC.

## 5. Acceptance

1. No hooks: `dispatch start`, then `claude --worktree a` creates Work. It shows
   "agent running", then "session ended" after `/exit`. The same holds for
   `claude -p --worktree`.
2. Hooks installed: one Work per worktree whichever path sees it first. S0 and
   sessions are consistent.
3. Worktrees present at the first watch are listed and not registered, across
   an owner restart.
4. A `claude session` lock with a dead PID or a reused PID ends the session.
   Unknown does not.
5. Working Work shows live files and tracked time, never fake zeros.
6. Non-interactive first `start` and `setup --checks --verify` behave as E.
   Consent is never granted without a terminal.
7. The 0.4.12 state upgrade test and the P0 script still hold. A real-agent
   trial with and without hooks passes.

Release blockers:
- duplicate Work for one workspace;
- a registered human worktree;
- an end inferred from unknown liveness;
- any auto-finish on an ordinary exit;
- consent granted without a terminal;
- a directory walk in the owner tick.

## 6. Deliberately not built

- Process scanning or CWD association.
- Recognition for other agents, without observed layouts.
- Automatic finish, close or cleanup.
- Removal of foreign worktrees.
- A persistent index.
- A `WorktreeCreate` hook.
- Implicit Git init or commits.
- Consent from repository content or non-interactive commands.

## 7. Execution log

### Setup (2026-10-13)

| Item | Value |
|---|---|
| Supervisor | `~/dispatch-dev/dispatch-0.4.12/dispatch`, the `v0.4.12` release: archive checked against `SHA256SUMS`, binary SHA-256 equals `BUILD.json` (`fd843cde…`), `BUILD.json` head `8fc2fdc`; read-only |
| Supervision state | `~/dispatch-dev/state-0413` (mode 700) |
| Integration clone | `~/dispatch-dev/integration-0413`, `release-0.4.13` from `origin/main` at `8fc2fdc`, no upstream; `dispatch.yml` (fmt, clippy, full `cargo test`) and `.claude/` excluded |
| P0 | `859e607`: plan; `runtime::claude` worktree signals and liveness with tests on the real lock text; `BaselineProvenance::Discovered`; stub `discovery`/`liveness` modules called from the owner tick; `Inventory`. fmt, clippy pass; `cargo test` 666 passed, 0 failed, 5 ignored |
| Workers | Four root-wrapped interactive Claude Code sessions started together, each in its own herdr tab, `--permission-mode auto` (the user's choice for speed; every diff is still reviewed and independently validated before a human accepts it) |
| Recording | Each worker, and a live `dispatch watch` of all four, recorded with `asciinema rec` (asciicast v2, idle limit 3 s) to `~/dispatch-dev/recordings-0413/*.cast`; local files, nothing uploaded. The terminal was 45×34 when recording began |

| Packet | Work | Workspace S0 |
|---|---|---|
| P1 discovery | `01M4V439XF7W895ZPDV5KG474Q` | `859e607` |
| P2 liveness | `01M4V43DJ51VC213Q7KXN2DVBC` | `859e607` |
| P3 live progress | `01M4V43HH0XMYKSTBWJD01PY8X` | `859e607` |
| P4 first use | `01M4V43NEDC7JJ3Q2C0FN8VV27` | `859e607` |

### Integration

Each packet: the worker's handoff reviewed; one review round where needed; an
independent run of fmt, clippy and the full suite on the worker's changes
applied to a `git archive` of the clone's HEAD with its own `CARGO_TARGET_DIR`
(`notes-0413/verify.sh`); after the human's approval the worker exits, Dispatch
freezes Δ and runs its checks; the frozen Δ's tree is compared with the
reviewed tree (`notes-0413/land.sh`); `check`, `accept`, commit.

| Packet | Review | Worker suite | Independent suite | Finish checks | Frozen = reviewed | Accept | Commit |
|---|---|---|---|---|---|---|---|
| P4 first use | "Continue without checks" focused, so Enter alone never writes to the project | 672/0/5 | 672/0/5 on `859e607` | all passed | `bd8342b` | source unmoved, merged checks not needed | `b4df615` |
| P3 live progress | none needed (decisions recorded: `status` drops the fake zeros for all working Work; no live count in JSON yet) | 676/0/5 | 676/0/5 on `859e607` | all passed | `a2d893b` | CONTINUE (symbols, P4's 8 files underneath); merged checks passed on P3 + P4 | `7b194f3` |

Workers paused once together at the account's usage limit and resumed by
themselves; parallel Opus sessions reach the limit sooner.
| P2 liveness | `claude::liveness(lock)` (one `ps`, no `sysctl`) when the lock names the recorded pid; only runtime-owned workspaces, so a person's attach with `--pid` is never touched | 674/0/5 | 691/0/5 on `7b194f3` (P3 + P4) | all passed | `1665ec1` | first accept **refused** by merged checks: `source_at_start::the_owner_observes_runtime_work_after_publication_and_only_then` failed because the random Work ID in the hook reply contained "S0"; after that test was fixed, the retry's merged checks passed | `5aa8633` |

**Integration fixes (integrator), each found by parallel integration:**

- `8d651e9` test: P3's unit test named every `Inventory` field; P1 adds fields,
  so the merged tree did not compile. The test builds the inventory from JSON.
- `8cde666` fix: the owner's per-tick `git status` in the person's repository
  took Git's optional `index.lock`, so the person's own `git commit` could fail
  while Dispatch watched (since 0.4.5). Found when a PTY test's own `git commit`
  in a watched fixture exited 128 under the load of parallel suites. Every Git
  command Dispatch runs now sets `GIT_OPTIONAL_LOCKS=0`; a regression test shows
  `git status` no longer rewrites a stale index, and does without the setting.
- `ddd90ba` test: a 0.4.12 test asserted a hook reply contains no "S0", but the
  reply names a random Work ID (fails about 1 run in 70).
| P1 discovery | register a worktree that appeared after watching began even when its `claude session` lock names a dead process (a `claude -p` run between ticks), with its session recorded already ended; keep tracking a worktree whose hook registration timed out; the 3 s grace accepted | 674/0/5 | first run on `8d651e9` (P3 + P4 + fix): 688 passed, **1 failed** (`watch_acceptance` `states-live`: the fixture's own `git commit` exited 128, the `index.lock` race fixed in `8cde666`), the scenario alone 4/4; final run on `5aa8633`: 700/0/5 | all passed | `d6a9766` | CONTINUE (symbols, 17 files underneath); merged checks passed | `52a149a` |
