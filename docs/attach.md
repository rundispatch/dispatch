# Attached work and `serve`: technical reference

This page describes what `src/orchestrator/attach.rs` and `src/orchestrator/serve.rs`
do: `dispatch attach`, `dispatch finish` and `dispatch serve`, the three commands that
let Dispatch observe, judge and — only if you ask — apply work done by an agent it did
not launch. For the short version and the commands, see the
[README](../README.md#attach-your-own-agent) and the
[product guide](product-guide.md#using-your-own-agent). Coherence itself (the model,
the verdicts, the fixture matrix) is documented in [coherence.md](coherence.md); this
page is about how attached work is fed into that same model. Attach and `serve` are
part of the experimental developer preview.

## The owner-loop model

The unit that keeps native Dispatch work coherent is an **owner loop**: one foreground
process that holds the run's operation lock, runs the watcher, persists verdicts, and
applies through `auto_apply`. Attached work gets the same owner loop, not a different
mechanism:

- **`dispatch attach -- <agent command>`** (wrapped attach) *is* the owner loop of the
  Work it creates: it starts the agent under the terminal, watches the integration
  world while the agent runs, and on exit freezes the patch, verifies it, and applies
  it if authorized. Claude, Codex, Cursor or anything else stays exactly itself;
  Dispatch prints nothing to that terminal while it runs.
- **The project owner** (`dispatch start` in the background, or `dispatch serve` in
  the foreground) is the owner loop for Work that has no live owner: an already
  running agent attached with `--workspace` and no command, or wrapped Work whose
  wrapper process died. It also keeps the verdict of every Ready result awaiting
  review current, native or attached. One owner per integration root. `dispatch
  watch` renders the project view from what the owner recorded, and owns nothing.

No owner is required for a standalone run, wrapped attach, or the TUI. The owner is
needed to keep an already-running foreign agent observed, to apply its work once it
is ready, and to keep verdicts current while you are not asking. Native runs and attached Work coexist by sharing state (SQLite, run
directories) and locks (the per-source apply lock), not by routing native runs through
`serve`: both paths call the same `evaluate`, `gate` and `auto_apply`.

## Repository and integration-root identity

- **Integration root**: the checkout `attach`/`serve` watch and apply into. It becomes
  the attached run's `source_path`, so the world, `world::observe` and the per-source
  apply lock are the ones native runs on that source already use. `--root` defaults to
  the repository's main worktree (the first entry of `git worktree list --porcelain`)
  when the workspace is a linked Git worktree; it is required for a plain directory.
  `serve` uses its current directory or `--root`.
- **Repository key**: `sha256(canonical git-common-dir)`, recorded on the attachment
  as `repo_key`. `attach` refuses a workspace whose common dir differs from the root's:
  *"workspace belongs to a different repository than the integration root."* Two main
  worktrees of the same repository are, by design, two separate worlds; the attachment
  names the one it targets.
- **Same-checkout attach** (workspace equals the integration root): the checkout is
  never the workspace. Δ attribution and self-application are undefined when the
  workspace and the root are the same tree.
  - **Wrapped form:** Dispatch makes a workspace instead (see *Workspaces Dispatch
    makes* below).
  - **Foreign form:** refused with *"attach needs a separate worktree; run git worktree
    add, or wrap the agent (dispatch attach -- <agent>) and Dispatch makes one"*.
- **Plain directories**: only the wrapped form can attach a plain (non-Git) directory;
  S0 is a snapshot taken at attach time (below). Foreign attach of a plain directory
  always requires Git and is refused otherwise.

## The record: an ordinary `RunRecord`

Attached Work is a `RunRecord` with `mode: RunMode::Attached` (`"attached"` in JSON)
and exactly one candidate. This is the whole trick that keeps coherence from forking
into two implementations: `check`, `explain`, `status`, `accept`, `reject`, `apply`,
`auto_apply`, the watcher, `gate` and `apply_validated` all take a `RunRecord` and a
candidate label, and none of them need to know whether Dispatch launched the agent.

| `RunRecord` field | Attached value |
|---|---|
| `source_path`, `source_kind`, `source_git_head`, `source_fingerprint` | the integration root at attach time |
| `baseline_path`, `baseline_commit` | S0, materialized under `runs/<id>/baseline` (below) |
| `candidates[0]` | `workspace_path` = the external worktree or directory; `diff_path` = `runs/<id>/delta.patch`; `harness_id` = `attachment.agent` or `"external"`; `label` = `"A"`; `checks` filled at finish |
| `attempts[0]` | one record, `role: "attached"`, same `harness_id`, `resource: None`, every model field `None` (never guessed) |
| `environment` | `execution_backend: "local"`; `unsafe_local` = the `--allow-unsafe-local` acknowledgement; the rest copied from the root's `dispatch.yml` |
| `execution` | `None`: no invocation budget, no questions |
| `task` | `--task` text, or `"attached work in <workspace basename>"`; `exact_prompt` equals `task` |
| `attachment: Option<AttachmentRecord>` | provenance, capabilities, process identities, timestamps (below); `None` for every native run |
| `outcome` while active | `lifecycle: Working, work_result: Pending, verification: NotRun, review: NotRequested, phase: Executing` |

`RunMode::Attached` needs **migration 21**, which rebuilds `runs` (see "Migration 21"
below). A binary built before this exists refuses a state directory that has already
been migrated to schema 21.

What attached Work cannot do: `dispatch refresh` (refused — see "Review of attached
work" below).

## S0 and confidence

S0 must be "the integration world as the agent saw it when its work began," because
every fact compares S0 against the world now, and Δ is workspace minus S0.

| Attach form | S0 | Δ | Provenance / confidence |
|---|---|---|---|
| Git worktree, wrapped or foreign | the tree of `merge_base(root HEAD, workspace HEAD)`, exported and turned into a private baseline repository under `runs/<id>/baseline` | everything in the workspace that differs from S0 (committed and uncommitted, untracked included, ignore rules honored) | `BaselineProvenance::GitMergeBase { commit }`, **`AttachConfidence::Full`**: edits made before attach are in Δ because S0 is a real commit |
| Git worktree, no merge base with the root | refused: *"no common history between the workspace and the integration root."* | — | — |
| Plain directory, wrapped only | a snapshot of the workspace taken at attach (`source::create_snapshot`) | workspace minus that snapshot | `BaselineProvenance::SnapshotAtAttach`, **`AttachConfidence::Partial`**: edits made *before* attach are invisible to Δ, and this is what the record says |
| Plain directory, foreign | refused: *"a plain directory can be attached only by wrapping the agent command."* | — | — |

The baseline is materialized on disk, not referenced by pointer, so it survives a
later rebase or garbage collection in the user's own repository, and `world::observe`,
`facts`, `gate` and `apply_validated` see exactly the `baseline_path`/`baseline_commit`
shape they already handle for native runs.

**Where this is visible today.** `attachment.provenance` and `attachment.confidence`
are stored on every attached run (`runs/<id>/metadata.json`) and carried in the
`attach.created` event payload. The *foreign* form of `attach` prints them once, at
creation:

```text
ATTACHED <run-id>
Workspace  <path>
Root       <path>
S0         commit <sha> (merge base), full confidence
Next: dispatch finish <run-id> when the agent is done
```

or, for a plain-directory snapshot, `S0  snapshot at attach, partial confidence`. The
*wrapped* form prints nothing at attach (rule: Dispatch is silent in that terminal —
see "Terminal and signals" below), so for wrapped attach the provenance line is not
printed at attach time; `dispatch explain <id>` shows it afterwards for either form.
For an attached run, `explain` prints an **Attached work** section instead of a
selection explanation (there is none): the workspace and root, the S0 line
(`S0: commit <sha> (merge base with the root)` or `S0: snapshot of the workspace at
attach; earlier edits are not attributed`), `confidence`, `agent`, `owner`, the
capability flags, and `finished: <reason>` once finished. It is followed by the
ordinary **Coherence** section when the world moved or the verdict is not `CONTINUE`,
and by the one-line `Coherence: CONTINUE — world unchanged` verdict otherwise. The
`serve` view's five fixed columns (below) do not include provenance. Programmatically,
read the run's stored record or its `attach.created` event.

## Δ and "finished"

- **Live Δ**, while a Work is still active: the watcher writes `delta-live.patch` on
  every world movement, exactly like a native allocation run's watcher, for mid-run
  verdicts only. It is never validated and never applied.
- **Finished** means Δ is frozen: `collect_diff(baseline, workspace, delta.patch)`,
  then `checks.verify` runs **in the workspace itself** — it is the user's own
  worktree, and the `--allow-unsafe-local` acknowledgement (or the one given at attach)
  carries the authority for that. The candidate's `checks` and `diff_stats` are set,
  `refresh_outcome` computes `work_result`/`verification`, and the run becomes
  `review: Pending`.
  - **Wrapped**: finishes automatically the moment the agent process exits, whatever
    its exit code. A non-zero exit still freezes Δ; the candidate becomes `Failed`
    only if Δ itself cannot be collected (an unreadable workspace), otherwise
    `Completed` with the agent's exit code recorded on the candidate.
  - **Foreign**: only an explicit `dispatch finish <id>`. There is no heuristic ("no
    edits for N minutes") that decides an agent is done; a human, or the agent's own
    driving script, must say so.
- After finish, the record behaves like any other Ready result: `check`, `accept`,
  `reject`, `auto_apply` apply to it unchanged.
- Δ attribution is per workspace — there is no per-process attribution inside one
  workspace — which is why same-checkout attach is refused and why one workspace holds
  at most one active attached Work at a time (`attach --workspace` on an
  already-attached workspace refuses with the existing id: *"workspace already
  attached as `<id>`."*).

## Capabilities: observe, signal, control, integrate

Four booleans, recorded on `attachment.capabilities` and enforced by the owner loop
that holds the Work:

| Capability | Wrapped attach | Foreign attach | Native run |
|---|---|---|---|
| **OBSERVE** (watch the world, evaluate, record verdicts) | yes | yes, by `serve` | yes |
| **SIGNAL** (surface stale/invalid state: events, `status`, the `serve` view) | yes | yes | yes |
| **CONTROL** (stop/cancel the process) | the wrapper's own child only, on the wrapper's own SIGTERM/SIGHUP | never | the existing supervisor |
| **INTEGRATE** (apply automatically when eligible) | only with `--auto-apply` at attach | only with `--auto-apply` at attach, executed by `serve` | session or invocation policy |

`observe` and `signal` are always `true`; `control` is `true` only for the wrapped
form; `integrate` mirrors `--auto-apply`.

**No foreign process is ever signaled or killed.** `--pid` on the foreign form and the
wrapper's own child PID are recorded purely for liveness (`identity_state`: PID plus
start time plus boot id) — never as a target for `kill`. `mid_run: stop` (the
coherence configuration key that lets Dispatch cancel a *native* attempt on an invalid
verdict) does not apply to attached work at all: neither the wrapped owner loop nor
`serve` implement it. A verdict on attached work is recorded, never enforced against
the agent. The agent never needs to know Dispatch exists; Dispatch writes no marker
files into its workspace.

## The three commands

```text
dispatch attach [--root <path>] [--workspace <path>] [--task <text>] [--agent <name>]
                [--allow-unsafe-local] [--auto-apply] -- <command> [args...]
dispatch attach --workspace <path> [--root <path>] [--pid <n>] [--task <text>]
                [--agent <name>] [--allow-unsafe-local] [--auto-apply]
dispatch finish <run-id> [--allow-unsafe-local]
dispatch start  [--root <path>]
dispatch watch  [--root <path>] [--json]
dispatch stop   [--root <path>]
dispatch clean  [--dry-run] [--yes]
dispatch serve  [--root <path>] [--json]
```

| Flag | Meaning |
|---|---|
| `--workspace <path>` | the worktree/directory to observe; default the current directory |
| `--root <path>` | the integration root; default the repository's main worktree for a linked Git worktree, required for a plain directory |
| `--task <text>` | describes the attached work; never sent to the agent |
| `--agent <name>` | free-text label (`claude`, `codex`, `cursor`, anything else); never guessed — `None` becomes `"external"` |
| `--pid <n>` | the foreign agent's process ID, for liveness only, never signaled |
| `--allow-unsafe-local` | explicitly allows `checks.verify` to run on the host at `finish` time |
| `--auto-apply` | sets `capabilities.integrate`: apply automatically once the work is ready and coherent |
| `-- <command> [args...]` | selects the **wrapped** form: everything after `--` is the agent command Dispatch spawns and owns for the session |
| `dispatch finish <run-id> [--allow-unsafe-local]` | foreign work only: freeze Δ, run `checks.verify` in the workspace, become an ordinary Ready result. The flag is recorded on the run (`attach.authorized {by: "human"}`), so accept's merged-tree checks run too |
| `dispatch start [--root <path>]` | run the project owner in the background (`serve --background` in its own session) and return |
| `dispatch watch [--root <path>] [--json]` | the project view, live; holds no lock, records nothing |
| `dispatch stop [--root <path>]` | stop exactly this root's owner |
| `dispatch clean [--dry-run] [--yes]` | remove leftover workspaces Dispatch made, after listing them and asking |
| `dispatch serve [--root <path>] [--json]` | the project owner in the foreground, with the view |

Whether `attach` is wrapped or foreign is decided by whether a command follows `--`:
with one, it is `run_wrapped`; without one, it is the plain `create` (foreign) form.

**Refusals, in the order they are checked**, with the exact messages `attach` prints
on `stderr` and exits non-zero:

1. workspace equals root, foreign form only — *"attach needs a separate worktree; run
   git worktree add, or wrap the agent (dispatch attach -- <agent>) and Dispatch makes
   one"*
2. different repository — *"workspace belongs to a different repository than the
   integration root"*
3. Git workspace with no merge base — *"no common history between the workspace and
   the integration root"*
4. plain-directory foreign attach — *"a plain directory can be attached only by
   wrapping the agent command"*
5. `checks.verify` configured without the acknowledgement — *"finish runs your
   checks.verify on the host; pass --allow-unsafe-local"*
6. the workspace is already attached — *"workspace already attached as `<id>`"*

`dispatch finish` on a run that is not an active, single-candidate attachment refuses
with *"attached work `<id>` is not active"* or *"attached work `<id>` does not have
exactly one candidate."*

## Terminal and signals (the wrapped form)

The wrapped owner loop is deliberately not the `Executor` path that Dispatch uses for
runs it launches itself: no piped stdout/stderr, no token accounting, no timeout —
Dispatch does not own this agent's execution, only its observation.

- The agent inherits the wrapper's stdio and stays in the wrapper's own process group
  (`std::process::Command` is used without `process_group(0)`), so terminal job
  control and Ctrl+C behave exactly as if the shell had started the agent directly.
  Nothing is written to the terminal while the child is alive.
- Before the agent is spawned, the wrapper installs `tokio` signal handlers for
  **SIGTERM** and **SIGHUP** that forward the same signal to the child (`libc::kill`,
  best-effort — a race with the child's own exit is a no-op).
- Also before spawning, the wrapper sets **SIGINT** and **SIGQUIT** to `SIG_IGN` for
  its own lifetime, the same thing `time(1)` does, so a Ctrl+C during the run reaches
  the interruptible child and not the wrapper. Because `SIG_IGN` is inherited across
  `exec`, the child resets both to `SIG_DFL` in `pre_exec` — after `fork`, before
  `exec` — so it execs interruptible with the default disposition, exactly as if the
  shell had started it. Both dispositions are restored to default in the wrapper once
  the agent has exited.
- The SIGTERM/SIGHUP handlers are installed before the agent is spawned specifically
  to close a race: a signal arriving after `spawn` but before the handler was
  installed would otherwise kill the wrapper by its default disposition and orphan the
  agent instead of forwarding the signal to it.
- On exit, the wrapper finishes the Work (freezes Δ, runs `checks.verify`), applies it
  if `--auto-apply` was given, and only then prints its own single line — after the
  agent has fully released the terminal. The process exit code is the agent's own exit
  code, when the wrapper's own bookkeeping (creation, then finishing) succeeded; a
  bookkeeping failure exits 1 instead (creation failures never start the agent at all).

## The project owner: `start`, `stop`, `serve` and `watch`

The owner loops until Ctrl+C, SIGTERM or SIGHUP. Nothing is applied or launched on
start or exit. `dispatch serve` runs it in the foreground with the view;
`dispatch start` runs it in the background and returns.

- **Identity and lock.** `locks/serve-<sha256(root)>.lock` (`flock`). A second owner
  on the same root refuses to start: *"already serving this root."* The lock is what
  "watched" means: the kernel releases it when the owner exits, crashes or the
  machine reboots, so nothing can claim a project is watched when it is not.
- **The record.** Once it holds the lock, the owner writes
  `watchers/<sha256(root)>.json`: root, `ProcessIdentity` (pid, start time, boot),
  start time, Dispatch version, background or foreground. It removes the record
  when it stops. The record only says whom `stop` may signal.
- **`dispatch start`** checks `dispatch.yml` and returns "Already watching" if the
  lock is held. Otherwise it starts `dispatch serve --background --root <root>`:
  - in its own session (`setsid`), so closing the terminal does not stop it;
  - with stdin from `/dev/null`;
  - with stdout and stderr in `watchers/<key>.log`, which names each distinct error
    once and is truncated on each start.

  It returns once the record names that child with an `ExactLive` identity (within
  10 s), or prints the log's last lines. Of two concurrent starts, one wins the lock;
  the other reports "Already watching". No agent profile is read.
- **`dispatch stop`** says "Not watching" when the lock is free, deleting any
  leftover record. Otherwise it sends SIGTERM only when the record names the exact
  live process (same pid, start time and boot), then waits for the lock. A record
  from before a crash or a reboot never names a live process, so it is never
  signalled; a lock held by an unidentified process is refused, naming the lock.
- **Every tick** (`coherence.poll_secs`, minimum 1 second):
  1. Read `world::signal(root)` and compare it with the previous tick's. Load the
     root's runs once (other projects' runs are not read). Adopt any orphaned Work
     (next bullet).
  2. Follow every active attached run whose live owner state is not `Live` (a live
     wrapper is that wrapper's own business). Its signal is a hash of its patch so
     far, taken with the trusted baseline repository into the owner's scratch
     directory, with an index kept per run so only changed files are re-hashed. No
     lock is held for this. When the world or that work moved:
     - take the run's lock;
     - observe the world against that run's own baseline and evaluate;
     - drop `analysis_uncertain`-only reasons, the same way the mid-run watcher does
       (a person may be mid-edit).
  3. Re-check every Ready, unapplied, review-pending result, native or attached. It
     is checked when it becomes Ready and whenever the world signal moves, with
     `coherence::live_validity`, which is exactly what `check` shows: a refusal by
     the merged-tree checks stands while the world has not moved. The evaluation
     takes no lock. The run's lock is taken only to record, and only if the run is
     unchanged since it was read.
  4. A verdict is recorded (`apply::persist_verdict`) when it differs from the
     verdict stored on the run in its decision, or is not `CONTINUE` and describes
     another world. The owner's first check of a Work item is always recorded, so
     the view says `unmoved` or `CONTINUE` rather than "not checked". The comparison is with
     the stored verdict, never with the owner's memory, so a restarted owner
     records what changed while it was away.
  5. For every attached run that is now `Finished`, `Ready`, unreviewed, unapplied and
     has `capabilities.integrate`, call `auto_apply` (serialized by the same
     per-source lock every applier uses). After its own successful apply, the owner
     re-observes immediately instead of waiting for the next tick — the apply is its
     own doorbell.
  6. In the foreground, render the project view (below) every tick, because a run
     attached, finished or applied by *another* process changes the view without any
     verdict or apply of the owner's own.

  With `-v` the owner logs where each non-idle tick's time went (`-vv`: every tick):
  signal, loading runs, following work, evaluations, re-checks and auto-apply.
- **`dispatch watch`** renders the same view from canonical state under a header
  naming who watches (`watched in the background since 10:02 (pid 812)`, or `not
  watched · dispatch start`, plus a note when the owner runs another Dispatch
  version).
  - It checks every second for a new event id or a change of watcher, and redraws
    at least every 30 s so finished Work ages out.
  - It holds no lock and records nothing of its own, so leaving it (`q` or
    Ctrl+C) never stops watching.
  - On a terminal, the rows can be selected (↑/↓ or j/k). A key runs exactly the
    command a person would type, as them, under the same locks and authority:
    - `f` is `dispatch finish`; without the project's check consent it asks
      before running checks, which is the same as `--allow-unsafe-local`;
    - `a` is `dispatch accept`, and a refusal is shown as accept prints it;
    - `r` is `dispatch reject`, after a confirmation with focus on Cancel;
    - `d` or Enter opens the review screen.

    `--json`, `--plain` and output that is not a terminal stay passive.
  - `--json` adds a `{"type":"watcher", "watcher"}` object whenever the header
    changes.
  - **Interactions between Work.** Each tick the owner also compares every piece
    of Work not yet integrated. How is in
    [coherence.md](coherence.md#work-against-work-interactions-interactionsrs).
    - A row ends with `· interacts with <id>`.
    - The interactive view shows the selected row's `Concurrent` lines.
    - `dispatch status` has a `Concurrent` section, and "not known" when the
      project is not watched.
    - A `--json` work object carries `interactions`: `null` while the project
      is not watched, else a list of `{with, direction (this_affects_theirs |
      theirs_affects_this | both), rule (same_declaration | uses | file |
      textual_overlap), path, symbol, change, evidence (symbol | file | text),
      lines}`. `status --json` carries the same list while watched.
    - Where another Work is named, its ID is shortened only as far as it stays
      distinct.
    - The owner keeps its view in `watchers/<key>.interactions.json`, written
      atomically and removed when it exits. Readers ignore it unless an owner
      holds the project.
  - `dispatch status` ends with the same line (`Project: …`).
- **Adoption.** An active attached run whose stored `owner_state` is `Live` but whose
  owner process (`attachment.owner`, the wrapper) is now gone (`identity_state`:
  `Gone`/`Reused`) is adopted: `serve` takes over its observation, commits
  `attach.adopted {owner_state: "adopted"}`, and sets `owner_state: Adopted`. A run
  whose operation lock is held by someone else is skipped, not forced — adoption never
  overrides a live owner. Nothing is finished, applied or relaunched by adoption
  itself; only observation resumes.
- **The view.** One line per run in this root — attached and native together, sorted
  by id — read from the stored DB projections, never recomputed for display beyond
  what the tick above already evaluated:

  ```text
  <first 8 of id> · <origin> <agent> · S0 <what it began against> · <verdict>[: <first reason>] · <state> · <verification>[ · review <review>]
  01M37J7H · native claude · S0 snapshot at eec41cc7 · unmoved · ready · checks passed · review pending
  01M37K2A · attached codex · S0 merge-base 1c9e0a47 (full) · REFRESH: fact_broken: pub fn validate… · blocked · checks passed · review pending
  ```

  The agent is the one that did the work, for native runs too. S0 is `snapshot at
  <project commit>` for native work on a Git source (the working tree as it was at
  that commit), `directory snapshot` for a plain directory, and `merge-base <commit>
  (full)`, `snapshot at attach (partial)` or `workspace at start <commit>` for
  attached work.

  The origin is:
  - `native` for work Dispatch ran;
  - `isolated` for work in a workspace Dispatch made;
  - `discovered` for work a runtime's session registered;
  - `attached` for any other attached work.

  The verdict is
  `CONTINUE`, `REFRESH` or `STOP` from the stored validity; `unmoved` when the source
  has not changed (including a result applied to an unmoved source); `overridden`
  when a human applied it over a REFRESH; or `not checked` when nothing has been
  evaluated yet.

  The state is one of:
  - `working`;
  - `question` (the run waits for your `dispatch answer`);
  - `idle` (discovered work with no open session);
  - `removed` (its workspace is gone and its exact changes are kept, waiting for
    you, or for its checks when the project's check consent holds);
  - `lost` (its workspace vanished unannounced; it cannot be finished);
  - `ready`, `blocked`, `applied` (`applied by auto-apply` when policy applied it) or
    `finished`.

  A run is shown while it is still active, or for up to an hour after it finished.
  On a TTY the block is redrawn in place; otherwise lines are appended. It **renders
  every tick but prints only what changed** since the last redraw — the whole block is
  only ever redrawn if at least one line in it differs from what is already on screen.
  `--json` instead emits one `{"type":"work", "run_id", "agent", "verdict", "state",
  "reason", "origin", "s0", "verification", "review", "applied_by"}` object per run
  whose displayed fields changed (the last five since 0.4.3), and one
  `{"type":"world", "digest"}` object whenever the observed world moved.

## Workspaces Dispatch makes

`dispatch attach -- <agent>` run from the checkout itself makes the workspace for
the agent under `<state>/workspaces/<run-id>`, never inside the checkout.
- **Git checkout:** S0 is the checkout's exact world at that moment, including
  uncommitted and untracked files but not ignored ones. It is recorded as a commit
  without copying anything (`source::world_commit`, like `git stash create`) and
  checked out as a linked worktree on its own `dispatch/<run-id>` branch.
- **Plain directory:** a snapshot and a private copy of it.
- The provenance is `workspace_at_start`, with full confidence. The agent needs no
  worktree support of its own.
- Ignored files, such as installed dependencies or build output, are not carried
  over, just as with `claude --worktree`.
- **Release:** the workspace is removed only once its work is applied, because its Δ
  is then in the checkout. It is kept after a reject (whose message prints its
  path), after a crash, or when removal fails. `dispatch status <id>` shows where it
  is and what became of it.
- **Clean:** nothing is deleted on a timer. `dispatch clean` lists the workspaces
  Dispatch made whose Work is over (rejected, closed, or applied but not
  released) and that still exist, with path, branch and Work.
  - On confirmation (focus on Cancel; without a terminal, `--yes`) it removes each
    one and its branch, under the run's lock, and only if it is still cleanable.
    It records `workspace.released {reason: "cleaned"}`.
  - `--dry-run` only lists them.
  - Work in progress or waiting for review is never listed. A runtime's or your
    own workspace is never touched. The run's record and Δ stay.

## Work a runtime registers

With Claude Code's hooks installed (`dispatch setup` → Runtime integrations), the
runtime tells Dispatch about its sessions (`dispatch hook claude`, which reads the
runtime's event on stdin):
- **Only in a watched project.** Only a project watched with `dispatch start` is
  followed. The integration root is always derived from the workspace's own Git
  repository, never taken from the event. Hook input is validated and bounded, and
  anything malformed is refused whole.
- **A session starting in a separate worktree** registers Work the first time:
  - S0 is the worktree's exact world then, before the session's first turn;
  - registration finishes within a 40 s budget, counted from the hook process
    starting, well inside the 60 s timeout `dispatch setup` installs. It is
    assembled in `<state>/registrations/<id>/`, and becomes Work only by a
    database insert and a rename into `runs/<id>/`, so Work is never half-made.
    A single decision file, created exclusively, settles each registration:
    - **registered:** published before the hook returned;
    - **pending:** S0 is durable (`registration.json`) but not yet published.
      The watching owner, or the next hook for that worktree, publishes it.
    - **untracked:** S0 was not durable by the deadline, or the worktree kept
      changing while it was captured. The session is told so, and nothing
      publishes it afterwards.

    The owner removes the leftovers of failed registrations once their budget
    has passed, and logs each one. `attach.created` records `registration_ms`
    and `via` (`hook` or `owner`). The plan with the full rules is
    `docs/plan-0.4.10.md` §3.
  - later sessions in that worktree (resume, clear, compact, a fork into it, a
    replayed event) are recorded on the same Work;
  - an ended session never ends the Work;
  - a resume into a worktree Dispatch has not seen is partial, since earlier edits
    may predate S0.
- **A fresh session in the checkout itself** is told that Dispatch cannot tell its
  edits from yours. Nothing is tracked. A resumed session gets no notice, because
  Claude Code reports the checkout before re-entering the session's worktree.
- **Verification:** runtime-registered Work carries no authority to run checks of
  its own. It gets that authority in one of two ways:
  - **A person's `dispatch finish <id> --allow-unsafe-local`**, recorded on the
    run.
  - **The project's check consent**, granted in `dispatch setup --checks`.
    - It is stored at `<state>/projects/<sha256(root)>.json` and names the root
      and the exact effective `checks.verify` commands. It is never stored in the
      repository, so a repository cannot grant itself host execution.
    - Work registered while it holds is created with local authority
      (`attach.authorized {by: "project consent"}`).
    - When `checks.verify` changes, it no longer holds until the person approves
      the new commands.
    - It authorizes checks only; applying still needs a person's review.
- **Workspace removal.** Claude Code deletes a worktree in two ways: at session exit,
  which runs `WorktreeRemove`, and through its `ExitWorktree` tool with `action:
  remove`, which does not. For that second path Dispatch hooks `PreToolUse`. Before
  either deletion, Dispatch keeps the Work's exact final Δ: the patch is written and
  synced into the run, and the removal committed, before the hook returns.
  - If that fails, or takes longer than 240 s, the hook fails and Claude Code keeps
    the worktree (for `ExitWorktree`, the tool call is refused).
  - Removal is an observation, not an ending. The Work waits for `finish`, which
    verifies it in a workspace rebuilt from S0 and the kept Δ, or for `reject`,
    which closes it and keeps the Δ in the run.
  - With the project's check consent still holding, for the same commands the
    Work was authorized with, the owner does that `finish` on its next tick
    (`finish_reason: by_consent`). The Work becomes Ready, or shows its failed
    checks, and waits for review. This happens after the hook returns, so the
    hook's budget is unchanged.
  - Only an empty Δ closes the Work.
  - If a followed workspace vanishes without the hook, the owner keeps the last Δ it
    followed (`delta-last-seen.patch`). That Work cannot be finished; rejecting it
    closes it. If that last Δ was empty, the Work closes by itself with no
    changes.

## The gate rule for attached runs

`coherence::gate` has one rule that exists only for attached work: **it never takes
the unmoved-fingerprint shortcut.** For a run Dispatch launched, the whole-tree
fingerprint recorded at creation was taken from the same tree S0 was copied from, so
an equal fingerprint today reliably means an unmoved world and the gate can skip
straight to `Legacy` (the existing all-or-nothing apply). An attached run's S0 is a
merge-base *commit*, while its `source_fingerprint` is the root's *working tree* at
attach time — the two can already differ even though nothing has moved since, so the
shortcut would silently skip evaluation and the integration checks. Attached runs
(`run.mode == RunMode::Attached`) therefore always evaluate through `evaluate_run` and,
when it says `Continue`, through L2 as usual; only `coherence.accept: strict` still
takes them straight to `Legacy`, exactly as it does for native runs.

This was found and fixed during a 0.4.0 real-agent trial (see "Evidence" in the [claims
table](coherence-validation.md)): the first real-agent attach applied without ever
computing a verdict because of this shortcut, and `dispatch explain` showed no
coherence section for an applied run — the regression test lives in
`tests/attach_cli.rs`.

## `empty_delta`

`auto_apply`'s eligibility stage (see [coherence.md](coherence.md#stage-1-eligibility))
skips a finished, otherwise-eligible run whose frozen `delta.patch` is empty, with
reason `empty_delta`, instead of applying it as "0 files changed." This matters
specifically for attached work: a foreign or wrapped agent that made no edits, or an
agent that only touched files the source's ignore rules exclude, still produces a
Ready result with nothing to apply. The run stays reviewable; nothing is recorded as
applied. This was also found during a 0.4.0 real-agent trial, fixed alongside the gate rule
above.

## Review of attached work

`dispatch accept`/`dispatch reject`, and the review menu's equivalents, record a
review of attached work exactly as they do for native work (since 0.4.1): one
`goal_feedback_revisions` row with the reasons and the verbatim explanation,
`outcome.review`, and a `review.accepted`/`review.rejected` event. The review and, on
accept, `apply_locked(..., ApplyAuthority::Human)` run under one hold of the run's
operation lock.

- `load_latest_unresolved_single` (the target of a bare `dispatch accept`/`dispatch
  reject` with no id) also returns an attached run whose review is still `Pending`.
- **The delivered-result guard.** A review is a judgment of a delivered result; an
  attached run that has not been finished yet has no Δ to judge. Reviewing an active
  attachment refuses: *"review requires a delivered result; finish attached work
  `<id>` first."*
- `dispatch refresh` has no meaning for attached work — there is no Dispatch task to
  relaunch — and is refused: *"attached work has no Dispatch task to refresh; finish
  or reject it."*

## Events

In addition to the existing coherence, application and review events (see
[coherence.md](coherence.md#events)), attached work commits:

| Event | When | Payload |
|---|---|---|
| `attach.created` | `attach` creates the Work (both forms) | `{"attachment": <AttachmentRecord>}` |
| `attach.started` | the wrapped form's agent process has been spawned | `{"agent_process": <ProcessIdentity>}` |
| `attach.finished` | `finish` (either explicit or on agent exit) freezes Δ | `{"reason": <FinishReason>}` |
| `attach.adopted` | `serve` takes over observation of an orphaned run | `{"owner_state": "adopted"}` |
| `attach.authorized` | Work gains local authority: from the project's check consent, or from a person's `finish --allow-unsafe-local` | `{"unsafe_local": true, "by": "project consent" \| "human"}` |
| `workspace.removed` | the workspace is gone: kept exactly by the hook, or seen missing by the owner | `{"exact": true, "files_changed"}` or `{"exact": false, "no_changes"}` |
| `workspace.released` | Dispatch removed a workspace it made | `{"workspace", "reason": "applied" \| "cleaned"}` |
| `work.closed` | a person rejected unfinished Work whose workspace is gone | `{"reason": "workspace_removed", "by": "human"}` |
| `interaction.landed` | after `result.applied`: what the owner's view said about this Work and the others in progress | `measure::Landing` (see [coherence.md](coherence.md#measuring-interactions-measurers)) |
| `interaction.outcome` | the owner's first verdict on one of those others after this Work landed | `measure::Outcome` |

An attached run also commits the ordinary `run.created`/`run.finished` events, and
`coherence.checked`/`coherence.invalidated`, `result.applied`/`application.failed`,
and `auto_apply.skipped`/`auto_apply.blocked` exactly as a native run does, all through
the shared `apply::persist_verdict` (see below).

## Shared verdict persistence

`orchestrator::apply::persist_verdict(state, db, run, validity)` is the one function
that turns a `Validity` into a stored verdict: it calls `remember_validity` (stamping
`first_invalid_at` the first time a run becomes invalid) and commits
`coherence.checked` (for `Continue`) or `coherence.invalidated` (otherwise) through the
same transition path every other event uses. The allocation-run mid-run watcher
(`native::apply_watch`), the wrapped attach owner loop, and `serve` all call it
directly; none of them re-implement `mid_run: stop` — `apply_watch` is the only caller
that still does, and only for native allocation runs.

## Durable versus recomputed

- **Durable**: the Work record (`runs/<id>/metadata.json`, including the
  `AttachmentRecord`), the materialized S0 baseline, the frozen `delta.patch`, every
  event, and `attachment.owner_state`/`finished_at`/`finish_reason`.
- **Recomputed on every use**: the world, symbol tables, facts and the verdict shown by
  `check`, `status` and `explain` — exactly as for
  native runs. `serve`'s view line reads the *last stored* verdict; it does not
  recompute one for display beyond what its own tick already evaluated and persisted.

## Restart and crash behavior

- **`serve` restart.** Active attached Work is read back from the database
  (`mode = attached`, `lifecycle != Finished`). The wrapper's stored `ProcessIdentity`
  (and the agent's, if known) is re-checked with `identity_state`: `ExactLive` is left
  alone (a live wrapper owns it); a stored `Live` owner that is now `Gone`/`Reused` is
  adopted (above); a foreign attachment with no owner, or an owner whose liveness
  cannot be told (`Unknown`), is observed but not marked adopted. Nothing is finished,
  applied or relaunched by a restart.
- **A wrapper crash** leaves its Work `Working` with a stale `owner_state: Live` until
  `serve` next observes that root and adopts it (or until a human runs `dispatch
  finish` directly), whichever happens first.
- **Reattaching** a process is not a separate operation: `attach --workspace` on a
  workspace that already has an active attached run refuses with that run's id
  (refusal 6 above) rather than creating a second Work for the same workspace.
- **SQLite unavailable or busy.** The wrapper keeps the agent running regardless and
  retries persistence on the next watcher tick, logging to `stderr` only after the
  child has exited; `serve` reports at most one error per tick and retries the next
  one. Neither ever kills or pauses an agent because of a persistence failure.
- **Crash consistency** is the same as for native runs: WAL plus `synchronous = FULL`;
  apply is not crash-atomic (a crash between `git apply` and the database update can
  leave a patched source with an unapplied-looking run — pre-existing, not specific to
  attach).
- **Concurrent projection writers.** An owner loop, `serve`, and every unlocked
  `load_run` reader that repairs a stale projection can all rewrite the same
  `metadata.json`. `state::write_atomically` stages each write through a **uniquely
  named** temporary file before an atomic rename, so two writers can never consume
  each other's temporary file mid-write; whichever rename lands last leaves a
  complete, current projection. (Found by the S6 simultaneous-integration scenario,
  which failed 30-50% of runs with a shared temporary name before this fix.)

## Coexistence with standalone runs

- `dispatch run`, `dispatch`, `dispatch accept` are unchanged when no attachment
  exists on a source.
- With attachments present, native runs and attached Work observe the same world,
  take the same per-source lock, and produce the same kinds of events. A native
  session's own watcher notices a landing by `serve` on its next poll, and vice versa.
- Nothing in the standalone path reads the `serve` lock or requires `serve` to be
  running.
- Attached Work appears in `status --json` like any other run, with
  `mode: "attached"`. Attach is always a foreground, human-typed command.

## Migration 21

`RunMode::Attached` requires schema 21 (`(21, "attached_work_mode", ...)` in
`src/db.rs`), which rebuilds `runs` the way migration 13 did: rename to `runs_v20`,
recreate `runs` with the same 28 columns plus a widened
`CHECK (run_mode IN ('legacy', 'routed', 'allocation', 'comparison', 'attached'))`,
copy every row across, drop `runs_v20`, then recreate migration 19's
`private_runs_source_window` index and `private_decision_immutable` trigger, which the
rename leaves bound to the old table name. `migrate()` turns `PRAGMA foreign_keys` off
for the duration of this migration (as it already does for 13), because otherwise
`DROP TABLE runs_v20` fails on any database with a row in `attempts`, `control_runs` or
`planned_goals` that references a run. `schema_version` becomes 21; opening a
historical-schema database creates a `dispatch.schema-20-*.db` backup first, as for
every other historical-schema upgrade. An older binary refuses a schema-21 state
directory, exactly as it already refuses any newer schema.

## Known limits

- No socket and no daemon: coordination is SQLite (WAL, revision-fenced projections)
  plus `flock` files; `serve` discovers new or changed Work on its next tick (at most
  `poll_secs`, default 10 s). This is adequate for work measured in minutes to hours,
  not for sub-second push updates.
- Interactions between Work are advisory, exist only while an owner watches the
  project, and never change a verdict or block anything.
- `serve` never launches, kills or refreshes anything. It finishes Work only when
  the project's check consent holds and a runtime removed the Work's workspace
  with its exact Δ kept. Otherwise wrapped attach and a human (`dispatch finish`)
  are the only things that finish a Work.
- No foreign process is ever signaled. `--pid` and the wrapper's own child PID are
  liveness-only.
- `mid_run: stop` does not apply to attached work in this version; a `Stop` verdict on
  attached work is recorded, not enforced.
- One workspace holds exactly one active attached Work; there is no per-process
  attribution inside a workspace shared by more than one agent.
- A plain directory cannot honor `.gitignore` (same limit as native runs on a plain
  directory); its S0 confidence is `Partial` regardless.
- `validate_candidate_tree`'s existing size limits apply to `collect_diff` on an
  attached workspace exactly as they do to a native candidate; a very large worktree
  with build output can make `finish` slow. Measured on this repository's own
  1.7 GB `target/` worktree: about 25 ms for 3,968 files, which is why the limits are
  unchanged for this release.
- Attached results never become routing or allocation evidence; they carry no
  observed-model or resource data (`resource: None`, every model field `None`).
- The `serve` view does not print S0 provenance or confidence; `dispatch explain`
  does (see "Where this is visible today" above), and the stored record and the
  `attach.created` event carry them for programs.
