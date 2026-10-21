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
| `attachment: Option<AttachmentRecord>` | provenance, capabilities, process identities, timestamps, and `source_at_start` (see "The source at the start" below); `None` for every native run |
| `outcome` while active | `lifecycle: Working, work_result: Pending, verification: NotRun, review: NotRequested, phase: Executing` |
| `outcome.closed` | set only when a person closed the Work (`dispatch close`): `{reason, at, replacement?, explanation?}`; see "Closing Work" below |

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
| Claude Code worktree the owner found, which appeared while the project was watched (see "Work that appears without hooks") | the tree of the worktree's merge base with the root, materialized the same way | as above | `BaselineProvenance::Discovered { commit }`, **`AttachConfidence::Full`**: the worktree did not exist when watching began, so everything in it is its session's |
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
Source     the same as its S0 (observed 2026-10-13 03:40 UTC, source HEAD <8 of sha>)
Next: dispatch finish <run-id> when the agent is done
```

or, for a plain-directory snapshot, `S0  snapshot at attach, partial confidence`. The
`Source` line is the start observation (next section). The
*wrapped* form prints nothing at attach (rule: Dispatch is silent in that terminal —
see "Terminal and signals" below), so for wrapped attach the provenance line is not
printed at attach time; `dispatch explain <id>` shows it afterwards for either form.
For an attached run, `explain` prints an **Attached work** section instead of a
selection explanation (there is none): the workspace and root, the S0 line
(`S0: commit <sha> (merge base with the root)`, `S0: commit <sha> (merge base with the
root; found by Dispatch, the worktree appeared while the project was watched)` or
`S0: snapshot of the workspace at attach; earlier edits are not attributed`),
`confidence`, `agent`, `owner`, the
capability flags, and `finished: <reason>` once finished. It is followed by the
ordinary **Coherence** section when the world moved or the verdict is not `CONTINUE`,
and by the one-line `Coherence: CONTINUE — world unchanged` verdict otherwise. The
`serve` view's five fixed columns (below) do not include provenance. Programmatically,
read the run's stored record or its `attach.created` event.

### The source at the start

S0 says what the Work began from, not whether that was the source as it was then. Accept
does not commit, so a worktree made from HEAD lacks accepted, uncommitted changes, and one
made from a remote branch lacks unpushed local commits. A session resumed in a worktree
whose earlier Work was closed or rejected without landing has the reverse: S0 holds changes
the source lacks.

For attached Work that Dispatch did not start from the current source, Dispatch compares the
integration source with S0 once, as close to the start as it can. It uses the same
`world::observe` as the verdict, so both directions come from one comparison, and records
`attachment.source_at_start`:
- **absent:** a workspace Dispatch made, which begins from the current source, or Work
  created before 0.4.12. Nothing is shown.
- **`not_observed`:** set when the Work is built, and left if nobody observes it, or the
  observation fails. Shown as "not observed" and never filled in later.
- **`observed`:** `{at, source_head, differing, paths: [{path, applied_work?}]}`, at most 20
  paths plus the count. `differing: 0` is recorded too, so "the same" and "not observed" stay
  apart.

| Work | Who observes | When |
|---|---|---|
| `attach --workspace` (foreign, or wrapped with an existing workspace) | the `attach` command | right after the Work is published, before the banner or the agent starts |
| registered by a runtime's hooks | the project owner | the first tick that sees the Work, if it was published after this owner started |

- The comparison runs without the run's lock. The lock is taken only to record, and only if
  the run is still `not_observed`, as one `attach.source_observed` event.
- It is never recomputed. Changes after the start are the verdict's business, and a later
  comparison against moved source cannot say what the source held then.
- Registration does no comparison, and the hook reply says nothing about the source.
- An owner never observes Work published before it started, so Work registered while no
  owner, or an older one, watched stays "not observed".
- A differing path is named as another Work's only when it is among the recorded paths of an
  applied Work's patch on this source. Otherwise no author is named. If two applied Work items
  touched the path, only one is named.
- It is advisory: no verdict depends on it.

`status` and `explain` show it under Workspace, `watch` as an "At start" detail (only for a
difference or "not observed"), and the foreign `attach` banner as its `Source` line:

```text
source at start: differed from its S0 at 1 path (observed 2026-10-13 03:40 UTC, source HEAD aab43dbb): a.txt (in applied Work 01M4SNJ9XR31N21H3CT04PN7XR)
  Work started with dispatch attach -- <agent> begins from the current source, including uncommitted changes.
```

That remedy comes first, for any agent. Advice specific to a runtime comes from that
runtime's adapter and appears only for workspaces it made. For a Claude Code worktree it
adds: "Claude Code: local commits reach a claude --worktree workspace only with
worktree.baseRef: "head" in its settings; neither that setting nor a commit copies
uncommitted changes." Dispatch never changes settings, commits or pushes in response.

## Δ and "finished"

- **Live Δ**, while a Work is still active: the watcher writes `delta-live.patch` on
  every world movement, exactly like a native allocation run's watcher, for mid-run
  verdicts only. It is never validated and never applied.
- **Finished** means Δ is frozen: `collect_diff(baseline, workspace, delta.patch)`,
  then `checks.verify` runs on what would land (`attach::finish_checks`). The
  `--allow-unsafe-local` acknowledgement (or the one given at attach, or the project's
  check consent) carries the authority for that. The candidate's `checks` and
  `diff_stats` are set, `refresh_outcome` computes `work_result`/`verification`, and the
  run becomes `review: Pending`.
  - **The source has not moved since the Work began** (the workspace's tree and the
    current source with Δ are one tree, or, when that tree cannot be known, accept's
    unmoved rule): the checks run once, **in the workspace itself**, the user's own
    worktree.
  - **The source moved:** they run on the current source with Δ, in the project's check
    workspace, exactly as accept's merged checks do, recorded as `coherence.merged_checks`
    with `by: "finish"`. A pass is the Work's verification, prints `finish: checked on the
    current source with this Δ, as accept checks it`, and lets accept reuse it unless the
    source moves again.
  - **Only if that fails, or Δ no longer applies to the current source,** do the checks
    run again in the workspace (S0 + Δ), to show whose change broke it. The summary then
    gives both: the current source's FAIL lines, and S0 + Δ's result, which is the Work's
    verification.
  - Before any run, the verification memo is consulted ([coherence.md](coherence.md#the-verification-memo)).
    A tree with a recorded pass runs nothing: finish records `checks.reused`, prints
    `finish: checks: passed, reused from <run> at <time>`, and its check lines point at the
    reused logs with 0.0 s.
  - **Wrapped**: finishes automatically the moment the agent process exits, whatever
    its exit code. A non-zero exit still freezes Δ; the candidate becomes `Failed`
    only if Δ itself cannot be collected (an unreadable workspace), otherwise
    `Completed` with the agent's exit code recorded on the candidate.
  - **Foreign**: only an explicit `dispatch finish <id>`. There is no heuristic ("no
    edits for N minutes") that decides an agent is done; a human, or the agent's own
    driving script, must say so.
- **The finish summary** says what holds. Δ is kept even when it no longer applies. The
  summary shows:
  - a heading by verdict: `Ready for review`, `Finished, but stale (REFRESH)`,
    `Finished, already in the source (STOP)` or `Verification failed`;
  - `Work <id>` with its patch path and file count, and the workspace verification;
  - a verdict computed now with the same static evaluation `check` uses (never stored), with
    up to five reasons;
  - `At accept`, the same line `check` prints, saying whether merged checks will run, would
    not run and why, or that accept would refuse it now;
  - the next commands, each with the full ID: accept/reject for CONTINUE; for REFRESH, run
    the agent again and close this Work as superseded, or reject; for STOP, close it as
    already present.

  A patch that no longer applies names its paths and keeps Git's full output in
  `<run dir>/patch-conflict.txt`. Only finish, the end of a native run and accept write that
  file; `check`, `status` and `watch` write nothing. The wrapped form still prints one line.
- After finish, the record behaves like any other Ready result: `check`, `accept`,
  `reject`, `close` and `auto_apply` apply to it unchanged.
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
dispatch close  <run-id> --reason <reason> [--replacement <run-id>]
                [--explanation <text> | --explanation-file <path|->]
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
| `--allow-unsafe-local` | explicitly allows `checks.verify` to run on the host at `finish` time; not needed when the project's check consent covers exactly its checks |
| `--auto-apply` | sets `capabilities.integrate`: apply automatically once the work is ready and coherent |
| `-- <command> [args...]` | selects the **wrapped** form: everything after `--` is the agent command Dispatch spawns and owns for the session |
| `dispatch finish <run-id> [--allow-unsafe-local]` | foreign work only: freeze Δ, run `checks.verify` on what would land (in the workspace when the source has not moved, otherwise on the current source with Δ in the check workspace), become an ordinary Ready result. The flag is recorded on the run (`attach.authorized {by: "human"}`), so accept's merged-tree checks run too. Without the flag, the project's check consent for exactly these checks is enough, and is recorded the same way (`by: "project consent"`) |
| `dispatch close <run-id> --reason <reason>` | retire Work without a review: nothing is applied, committed or deleted (see "Closing Work" below) |
| `dispatch start [--root <path>]` | run the project owner in the background (`serve --background` in its own session) and return |
| `dispatch watch [--root <path>] [--json]` | the project view, live; holds no lock, records nothing |
| `dispatch stop [--root <path>]` | stop exactly this root's owner |
| `dispatch clean [--dry-run] [--yes]` | remove leftover workspaces Dispatch made, the projects' check workspaces and the verification memo, after listing them and asking; list, never remove, worktrees it did not make |
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
5. `checks.verify` configured without the acknowledgement and without the project's
   check consent for exactly those checks — *"finish runs your checks.verify on the host;
   pass --allow-unsafe-local, or give this project standing consent for these checks with
   dispatch setup"*. With that consent, `attach` and `finish` record it on the Work
   (`attach.authorized {by: "project consent"}`), so accept runs its merged checks as it
   does for the flag.
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
  if `--auto-apply` was given, and only then prints — after the agent has fully released
  the terminal. Without `--auto-apply` it prints the end of finish's own summary: the
  verdict heading, `Coherence`, `At accept` and the next commands. With it, it prints the
  auto-apply outcome.

  ```text
  Attached work 01M5FRJKMFTXCP65J9H72R066A finished: Ready for review

  Coherence: CONTINUE — world unchanged

  At accept
    merged checks would not run now — the source has not moved since the Work began, so the merged result is the Work's own tree

  Review
    dispatch diff 01M5FRJKMFTXCP65J9H72R066A

  Then
    dispatch accept 01M5FRJKMFTXCP65J9H72R066A
    dispatch reject 01M5FRJKMFTXCP65J9H72R066A
  ```

  Before 0.4.17 it printed `Attached work <id> finished; next: dispatch check <id>`. The process exit code is the agent's own exit
  code, when the wrapper's own bookkeeping (creation, then finishing) succeeded; a
  bookkeeping failure exits 1 instead (creation failures never start the agent at all).

## The project owner: `start`, `stop`, `serve` and `watch`

The owner loops until Ctrl+C, SIGTERM or SIGHUP, or until its project or Dispatch's state
directory is gone. Nothing is applied or launched on start or exit. `dispatch serve` runs it in the foreground with the view;
`dispatch start` runs it in the background and returns.

- **Identity and lock.** `locks/serve-<sha256(root)>.lock` (`flock`). A second owner
  on the same root refuses to start: *"already serving this root."* The lock is what
  "watched" means: the kernel releases it when the owner exits, crashes or the
  machine reboots, so nothing can claim a project is watched when it is not.
- **The record.** Once it holds the lock, the owner writes
  `watchers/<sha256(root)>.json`: root, `ProcessIdentity` (pid, start time, boot),
  start time, Dispatch version, background or foreground, and, since version 2 of the
  record (0.4.17), `exe`, the canonical path of the binary, and `environment`, the
  `PATH`, `HOME`, `TMPDIR` and `LANG` checks receive. It removes the record when it
  stops. The record says whom `stop` may signal, and what to bring back.
- **Watching comes back** (`background::ensure_current`). A project watched with
  `dispatch start` is watched again by the next Dispatch command or hook there, after a
  reboot, a crash or an upgrade in place, until `dispatch stop`. Until that command or hook
  runs, nothing watches it. A foreground `serve` is never brought back.
  - **A background record left with the lock free** means the owner died without `stop`
    (a reboot, a crash). `status`, `watch`, `attach`, `finish`, `check`, `accept` and
    `start` start it again, with the recorded environment, wait for its first scan, and
    say on stderr `Watching <root> again: its owner had stopped.`. A Claude Code or Hermes
    hook spawns it without waiting and writes that line to the owner's log, never to
    stdout, then goes on as in a watched project. While `dispatch.yml` does not parse,
    nothing is spawned, the record stays, and the line is `Could not watch <root> again:
    …`.
  - **A held lock and another version.** When the record's `exe` is this binary (an
    upgrade in place), or, for a version 1 record without `exe`, when this version is
    newer, the command stops the owner (SIGTERM, then waits for the lock) and starts this
    one with the recorded environment: `Restarted watching <root> with Dispatch <new> (was
    <old>).`. The configuration is read first; if it fails, the old owner keeps watching
    and the line is `Could not restart watching <root> with Dispatch <new>: …`. An older
    binary, or another install path, never replaces an owner: `status` and `watch` keep
    naming the version it runs. Hooks only revive a dead owner; they never replace one.
  - Nothing of this reaches stdout, so `--json` output is unchanged.
- **The owner exits when its project or state directory is gone.** It records the
  `(dev, inode)` of both when it starts, compares them with two `stat` calls per tick, and
  on a difference logs `stopping: the project|state directory <path> no longer exists`,
  removes its record and exits.
- **`dispatch start`** checks `dispatch.yml` and returns "Already watching" if the
  lock is held. Otherwise it starts `dispatch serve --background --root <root>`:
  - in its own session (`setsid`), so closing the terminal does not stop it;
  - with stdin from `/dev/null`;
  - with stdout and stderr in `watchers/<key>.log`, which names each distinct error
    once and is truncated on each start.

  It returns once the record names that child with an `ExactLive` identity and says
  its first discovery scan is done (`discovery_ready`), within 120 s, or prints the
  log's last lines. So a worktree made right after `start` returns is new to the
  watch, not one listed as already there. Of two concurrent starts, one wins the
  lock; the other reports "Already watching" once the winner's first scan is done. No
  agent profile is read.
- **`dispatch stop`** says "Not watching" when the lock is free, deleting any
  leftover record. Otherwise it sends SIGTERM only when the record names the exact
  live process (same pid, start time and boot), then waits for the lock. A record
  from before a crash or a reboot never names a live process, so it is never
  signalled; a lock held by an unidentified process is refused, naming the lock.
- **Two loops.** The owner runs a fast loop on a thread of its own beside the tick. It
  stops before the owner releases the project.
- **Finishes by consent run as tasks** (`serve::Finishes`), one per Work, each with a
  cancellation token, so the tick never waits for a project's checks. Each loop the owner
  reaps the tasks that ended (an error is logged once, and that Work is not tried again by
  this owner), handles reopen notes (cancelling the old Work's finish first; see "Session
  end and resume"), ticks, then spawns a finish for each Work that `finishes_by_consent`
  holds for and that has no task. On every exit, running finishes are cancelled and
  awaited, their checks killed, and the Work stays Working for the next owner.
- **The fast loop** (about once a second: one pass, then a one-second wait; there is
  no setting):
  1. Load the root's runs and run discovery (see "Work that appears without hooks"):
     list, recognize and register, nothing else. The first successful scan sets
     `discovery_ready` on the watcher record, which `start` waits for.
  2. Follow active attached Work whose agent can remove its own worktree
     (`discovery::VANISHING_WORKTREES`: `hermes`), unless Dispatch made its workspace,
     its workspace was already recorded removed, or a live wrapper owns it. Each pass
     checks the worktree against a temporary index kept per run, and only when the tree
     changed writes a new patch and replaces `delta-live.patch` with it. A capture that
     overlaps the worktree's removal is dropped, so the last complete patch stays.
  3. When such a worktree is gone, record it with the freshest complete patch as the
     last-seen Δ (`note_workspace_gone`; see "Workspace removal").

  Measured on one machine, medians of ten passes: an unchanged pass took about 15 ms and
  a changed one 47 ms on Dispatch's own repository (340 tracked files); P0 measured the
  worktree listing at 11 ms, and 21 ms with 60 linked worktrees.
- **Every tick** (`coherence.poll_secs`, minimum 1 second):
  1. Read `world::signal(root)` and compare it with the previous tick's. Load the
     root's runs once (other projects' runs are not read). Adopt any orphaned Work
     (next bullet). Then observe the source at the start, once, for runtime-registered
     Work that is still `not_observed` and was published after this owner started (see
     "The source at the start"); each such Work costs one events query per owner
     lifetime. Then read the session locks of the runtime-made Work it follows for
     liveness (see "Liveness from the session lock"), and clear the "waiting on you" mark
     of Work whose session moved (see "Waiting on you"): one `stat` per waiting Work.
  2. Follow every active attached run whose live owner state is not `Live` (a live
     wrapper is that wrapper's own business). For Work the fast loop follows, the
     tick still evaluates from its own scratch patch, but leaves `delta-live.patch` and
     a vanished worktree to that loop. Its signal is a hash of its patch so
     far, taken with the trusted baseline repository into the owner's scratch
     directory, with an index kept per run so only changed files are re-hashed. The
     patch is taken again only when a status against that index sees the worktree
     changed, and the patch the tick followed is the one it evaluates and writes as
     `delta-live.patch`. Follows run side by side, at most one per core, and hold no
     lock. When the world or that work moved:
     - observe the world against that run's own baseline and evaluate, side by side
       and without the run's lock. The owner looks at the source once per change of its
       signal and shares that look with every Work: a file's hash is kept while its
       size, times, inode and mode are unchanged, and taken again when the file changed
       within 2 s of the last look; baseline trees are kept per commit; facts are
       derived once per S0 commit and Δ. All of it lives in memory only;
     - drop `analysis_uncertain`-only reasons, the same way the mid-run watcher does
       (a person may be mid-edit);
     - then take the run's lock only to record: read the run again, and record only if
       it is still unfinished and live or removed.
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
  6. Count the files in each followed run's patch so far (the scratch patch of step 2;
     no new snapshot) and keep the counts in `watchers/<key>.live.json`, rewritten only
     when they change. `status` and `watch` read them as files changed now. Readers
     ignore the file unless an owner holds the project and its record names the pid
     that wrote it, so a count left by a killed owner reads as unknown. The owner
     removes it when it stops.
  7. In the foreground, render the project view (below) every tick, because a run
     attached, finished or applied by *another* process changes the view without any
     verdict or apply of the owner's own.

  With `-v` the owner logs where each non-idle tick's time went (`-vv`: every tick):
  signal, loading runs, following work, evaluations, re-checks, auto-apply,
  footprints and pairs, ending with the tick's `total`:

  ```text
  tick: signal 18 ms · 1 runs loaded in 2 ms · 1 followed in 9 ms · 1 evaluated in 37 ms · 0 rechecked in 0 ms · auto-apply 0 ms · 0 footprinted in 0 ms · pairs in 0 ms · total 99 ms
  ```
- **`dispatch watch`** renders the same view from canonical state under a header
  naming who watches (`watched in the background since 10:02 (pid 812)`, or `not
  watched · dispatch start`, plus a note when the owner runs another Dispatch
  version that this binary does not replace).
  - It checks every second for a new event id or a change of watcher, and redraws
    at least every 30 s so finished Work ages out.
  - It holds no lock and records nothing of its own, so leaving it (`q` or
    Ctrl+C) never stops watching.
  - On a terminal, the rows can be selected (↑/↓ or j/k). A key runs exactly the
    command a person would type, as them, under the same locks and authority:
    - `f` is `dispatch finish`; without the project's check consent it asks
      before running checks, which is the same as `--allow-unsafe-local`;
    - `a` is `dispatch accept`. A refusal is shown as accept prints it, and an accept
      names what became of the merged checks;
    - `r` is `dispatch reject`, after a confirmation with focus on Cancel. On unfinished
      Work it refuses before asking and names `dispatch close <id>`;
    - `c` is `dispatch close`. It asks for the reason (those the Work allows, except
      `superseded`, which needs a replacement ID and stays a typed command), then for a
      confirmation. Cancel is the default of both, so Enter alone, Esc or Ctrl+C records
      nothing;
    - `d` or Enter opens the review screen.

    `--json`, `--plain` and output that is not a terminal stay passive.
  - `--json` adds a `{"type":"watcher", "watcher"}` object whenever the header
    changes. When worktrees are found, not tracked, it also carries `inventory`:
    `[{path, first_seen, base?, lock?, attach}]`, re-emitted when the list changes.
  - **Work in progress** (`working` or `idle`) has a Progress detail on a terminal:
    `files: 3 changed · tracked for 12m · agent running (pid 4242)`. Files read
    `unknown` before the owner has counted them, or when no owner watches the
    project, and `none changed yet` for a count of zero. The tracked time is from the
    attachment's start. "Agent running" appears only
    while the recorded agent process is alive with the same start (`ExactLive`).
    Piped rows and `--json` work records carry no count.
  - **Found, not tracked.** Worktrees discovery lists (see "Work that appears without
    hooks") are shown under the Work, one line each with `dispatch attach --workspace
    <path>`, also when there is no Work. They are not selectable. Piped, each is a
    line `found, not tracked · dispatch attach --workspace <path>`.
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
  - `dispatch status` ends with the same line (`Project: …`), followed by a "Found, not
    tracked" section when there is something to list. In a project with no Work it prints
    `No Work in <root> yet.` and that line, and exits 0; `status --json` prints `null`,
    and `status --jsonl` prints nothing. For attached Work that is not
    finished, its Candidate block reads `Runtime unknown`, `Tracked for 12m` and
    `Files changed <n> now`, or `unknown` as above. There is no Diff line until the
    Work is finished, and its Workspace section adds `agent running (pid N)` while the
    agent runs.
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
  attached work, and `merge-base <commit> (full; found by Dispatch)` for Work the owner
  found without a hook.

  The origin is:
  - `native` for work Dispatch ran;
  - `isolated` for work in a workspace Dispatch made;
  - `discovered` for work a runtime's session registered, or that the owner found;
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
  - `lost` (its workspace vanished unannounced: you can finish the changes last seen,
    or close it; with nothing kept, only close it);
  - `ready`, `blocked`, `applied` (`applied by auto-apply` when policy applied it),
    `closed` (a person closed it; its review is shown as none) or `finished`.

  A run is shown while it is still active, or for up to an hour after it finished.
  On a TTY the block is redrawn in place; otherwise lines are appended. It **renders
  every tick but prints only what changed** since the last redraw — the whole block is
  only ever redrawn if at least one line in it differs from what is already on screen.
  `--json` instead emits one `{"type":"work", "run_id", "agent", "verdict", "state",
  "reason", "origin", "s0", "verification", "review", "applied_by", "needs_you"}` object
  per run whose displayed fields changed (`origin` to `applied_by` since 0.4.3,
  `needs_you` since 0.4.17), and one
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
  path), after a close, after a crash, or when removal fails. `dispatch status <id>` shows where it
  is and what became of it.
- **Clean:** nothing is deleted on a timer. `dispatch clean` lists the workspaces
  Dispatch made whose Work is over (rejected, closed, including closed results, or
  applied but not released) and that still exist, with path, branch and full Work ID.
  - A workspace Dispatch made is listed only when no Work needs its path: no
    unfinished Work, and no Work waiting for review, may be attached to it. So after
    a workspace is repaired and attached again as new Work, closing or rejecting the
    old Work does not make it cleanable while the new Work uses it.
  - On confirmation (focus on Cancel; without a terminal, `--yes`) it removes each
    one and its branch, under the run's lock, and only if it is still cleanable.
    It records `workspace.released {reason: "cleaned"}`.
  - `--dry-run` only lists them.
  - Worktrees you or a runtime made are never touched. Those whose Work is over are
    listed under "Kept, because Dispatch did not make them", one line per path with
    who made it and each Work ID and how it ended, followed by how to inspect one
    (`git -C <path> status`) and remove it (`git worktree remove <path>`, or through
    the runtime that made it), with a caution that a locked worktree may still belong
    to a running agent session.
  - "Nothing to clean" adds that Dispatch removes only the workspaces it made.
  - The run's record and Δ stay.
  - **Check workspaces** (`<state>/check-workspaces/<sha256 of the source path>`, one per
    project, see [coherence.md](coherence.md#integration-checks-l2)) are listed after
    them, one line each: `<path> · internal check workspace`. The confirmation then says
    that check workspaces hold temporary verification and build files and that Work
    records are kept separately. Each is removed under its project's source lock; one in
    use refuses with `the project's checks are in use: …`. The next merged check builds it
    again from nothing.
  - **The verification memo** is listed last, `<state>/memo · recorded passes (N)`, when it
    holds any pass, so "Nothing to clean" is never said while one is recorded. It is
    removed only after confirmation, after the workspaces, and `Removed N recorded
    pass(es).` says so; `--dry-run`, Cancel and a refusal leave it. Nothing but reuse is
    lost: the next check of each tree runs again.

## Work a runtime registers

A runtime's session in its own worktree becomes Work in a watched project in two
ways: through Claude Code's hooks, described here, or without any hook, when the
owner finds the worktree ("Work that appears without hooks", below). Either way
there is one Work per worktree. Hermes's hook registers nothing; it keeps the
exact Δ of Work discovery found ("Hermes Agent", below).

With Claude Code's hooks installed, the runtime tells Dispatch about its sessions
(`dispatch hook claude`, which reads the runtime's event on stdin). `dispatch setup`
offers them when `claude` is on `PATH`, showing the exact additions, with focus on Skip.
There are six, each a `command` hook running `'<dispatch>' --state-dir '<state>' hook
claude`:

| Event | Matcher | Timeout | Since |
|---|---|---|---|
| `SessionStart` | | 60 s | 0.4.6 |
| `SessionEnd` | | 5 s | 0.4.6 |
| `WorktreeRemove` | | 300 s | 0.4.6 |
| `PreToolUse` | `ExitWorktree` | 300 s | 0.4.6 |
| `Notification` | `permission_prompt\|idle_prompt\|agent_needs_input` | 5 s | 0.4.17 |
| `Stop` | | 5 s | 0.4.17 |

A settings file holding exactly the four an earlier Dispatch installed is offered as an
Update, showing the hooks it becomes, for the same executable (`Hook command, unchanged:
…`) or another. Hooks of Dispatch's edited by hand are left alone and reported as `changed
by hand`; the "Agents Dispatch launches…" → Runtime integrations menu removes them (`Only
Dispatch's own hooks are removed, hook by hook`), then installs them again. A person's own
hooks, in the same file or the same entry, are kept on install, update and removal.

- **Only in a watched project.** Only a project watched with `dispatch start` is
  followed. The integration root is always derived from the workspace's own Git
  repository, never taken from the event. Hook input is validated and bounded, and
  anything malformed is refused whole. In a project nobody watches, a session start or
  end costs no Git and no database: Dispatch reads the main worktree from the `.git`
  files themselves and checks only that project's watch lock. Anything it cannot read
  for certain (a symlinked `.git`, a separate Git directory or submodule, a config with
  `worktree…`, `bare = true` or `[include…`) is left to Git. A workspace's removal is
  never decided this way, because its Δ is kept whether or not the project is watched.
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
      changing while it was captured. The session is told so, and no hook
      publishes it afterwards. Discovery may still track the worktree when it
      finds it later, from its merge base (below).

    The owner removes the leftovers of failed registrations once their budget
    has passed, and logs each one. `attach.created` records `registration_ms`
    and `via` (`hook`, `owner`, or `discovery` for Work found without a hook,
    whose `registration_ms` is counted from the session's process start). The
    plan with the full rules is `docs/plan-0.4.10.md` §3.
  - later sessions in that worktree (resume, clear, compact, a fork into it, a
    replayed event) are recorded on the same Work. A `SessionStart` that finds its
    Work held (the owner recording a verdict, a typed `dispatch finish` running its
    checks) waits for it within the hook's 40 s budget; if the Work finished
    meanwhile, the session goes on from that result;
  - the watching owner observes the source at the start once, after the Work is
    published (see "The source at the start"); registration and the hook reply say
    nothing about it;
  - an ended session does not end the Work by itself. Without the project's check
    consent, `status` and `watch` say the session ended: finish to freeze its changes
    (`dispatch finish <id>`), or resume it; or close it (`dispatch close <id> --reason
    unfinished`). With it, the owner finishes it once the session's process is gone
    (see "Session end and resume");
  - Claude Code reports a worktree session's end (`SessionEnd`) from the main
    checkout, not the worktree. When an end names no Work by its directory,
    Dispatch records it on the unfinished Work of that project holding the same
    open session, publishing a pending registration of that session first. An end
    that matches neither changes nothing;
  - `claude -p` sessions report no end at all. Their end is recorded only from the
    worktree's lock ("Liveness from the session lock", below), once the owner has
    seen the session running. Otherwise finish or close their Work explicitly;
  - a resume into a worktree Dispatch has not seen is partial, since earlier edits
    may predate S0.
- **A fresh session in the checkout itself** is told that Dispatch cannot tell its
  edits from yours. Nothing is tracked. A resumed session gets no notice, because
  Claude Code reports the checkout before re-entering the session's worktree.
- **Verification:** runtime-registered Work carries no authority to run checks of
  its own. It gets that authority in one of two ways:
  - **A person's `dispatch finish <id> --allow-unsafe-local`**, recorded on the
    run.
  - **The project's check consent**, granted in `dispatch setup` (or `dispatch setup
    --checks`) on a terminal.
    - It is stored at `<state>/projects/<sha256(root)>.json` and names the root
      and the exact effective `checks.verify` commands. It is never stored in the
      repository, so a repository cannot grant itself host execution.
    - Version 2 (0.4.17) also covers typed `attach` and `finish`, and Work the owner
      finishes when its session ends. A version 1 consent no longer holds (`check
      consent no longer holds: it did not cover typed attach, finish, or session end`)
      until `dispatch setup` asks again.
    - Work registered or found while it holds is created with local authority
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
    verifies it in a workspace rebuilt from S0 and the kept Δ, or for
    `dispatch close <id> --reason workspace-removed`, which closes it and keeps the
    Δ in the run. `reject` refuses it, because there is no result to judge.
  - With the project's check consent still holding, for the same commands the
    Work was authorized with, the owner does that `finish` as a task of its own,
    from its next tick (`finish_reason: by_consent`). The Work becomes Ready, or
    shows its failed checks, and waits for review. This happens after the hook
    returns, so the hook's budget is unchanged.
  - Only an empty Δ closes the Work.
  - If a followed workspace vanishes without the hook, the owner keeps the last Δ it
    followed (`delta-last-seen.patch`), and records when that patch was written as
    `workspace_removed.last_seen_at`. For Work whose agent can remove its own worktree
    the fast loop keeps that patch fresh; for other Work it is the tick's (see "The
    project owner").
    - `status`, `watch`, `finish`, `diff`, `check` and the review screen label it with
      that time:

      ```text
      removed 2026-10-15 23:27 UTC: only the changes last seen were kept; finish them yourself (dispatch finish <id>) or close it (dispatch close <id> --reason workspace-removed)
      Changes last seen 2026-10-15T23:27:17.198627622Z. Changes made after that time are not included; Dispatch cannot say whether there were any.
      ```

      A record from before 0.4.15 has no such time and says it is unknown. `diff
      --name-only` writes the label on stderr, so stdout stays file names.
    - A person can finish it: `dispatch finish <id>`, or `f` in `watch`. Finish copies
      the last-seen Δ to `delta.patch`, rebuilds a workspace from S0 and it, and runs
      the checks there, as for an exact capture. The result waits for review; accept
      runs its merged checks when the source moved, and `accept --expect` binds it to
      the reviewed Δ. The project's check consent never finishes it.
    - With no last-seen Δ kept, `finish` refuses: *"the workspace of <id> was removed
      and nothing was kept; it cannot be finished. Close this work: dispatch close
      <id> --reason workspace-removed"*.
    - `dispatch close <id> --reason workspace-removed` closes it and keeps that Δ.
      While an owner watches the project but has not yet recorded the vanished
      workspace, `close` refuses and asks you to try again in a few seconds. If that
      persists, the owner may be stuck: `dispatch stop` ends it, and close then goes
      ahead without those changes. If that last Δ was empty, the Work closes by itself
      with no changes.

### Session end and resume

With the person's standing consent for the project's checks, the owner finishes Work a
runtime registered (Claude Code or Hermes, by hook or discovery) once all of these hold
(`attach::finishes_by_consent`):
- the Work is Working, registered with the project's consent, and its checks are exactly the
  ones consent names now;
- every session it has ended, by the runtime's `SessionEnd` (or Hermes's hook) or by
  liveness;
- the session's process is gone, judged by the worktree's lock
  (`liveness::session_process_gone`): a lock naming a pid that no longer runs with the lock's
  start time, as a closed terminal leaves it, or no lock at all, as `/exit` leaves it.

`/clear` and `/resume` end a session in a process that keeps running, so they finish
nothing. `SessionEnd` itself only records the end, within its 5 s timeout. The finish is the
one above ("Δ and finished": Δ frozen, the memo first, checks in place when the source is
unmoved, in the check workspace when it moved), recorded as `finish_reason: by_consent`
with `attach.finished`'s `workspace_removed: null`. It runs as a task of its own, so the
owner's ticks and other Work's verdicts go on while its checks run. Nothing is applied. Codex
and Cursor send no end, and a Claude session in a worktree Claude Code did not make has no
lock to judge, so that Work is finished by a person.

While the owner finishes Work, it keeps `runs/<id>/finishing-by-consent` with its pid. A typed
`dispatch finish` or `dispatch close` on that Work waits a moment, then says `Work <id> is
being finished by its project's owner, as you allowed (checks running); wait for it, or close
it once it is ready`.

**Resume.** A session that starts again in a worktree whose latest Work from that runtime
awaits review (Ready, review pending, not applied, not closed), or is being finished by consent, does not
join it:
- the new Work begins from that Work's S0 and provenance, so its Δ holds both sessions'
  changes, and records `work.reopened {reopens: <old>, provenance}`;
- the earlier Work closes as superseded by it, its Δ kept, its review `not_requested`, with
  `work.closed {reason: "superseded", by: "reopen", replacement}`. A finish still running is
  cancelled first and its checks killed;
- the note `<state>/reopen/<old id>`, written before the new Work is published, makes this
  survive a crash: the owner closes the earlier Work within a tick when the hook could not.

Either the `SessionStart` hook (`source: resume`) or discovery (a new running lock on the
worktree) may see the session first; the outcome is the same, and the hook's reply names the
new Work: `Dispatch is tracking this worktree as Work <id>; see it with dispatch watch.`. A
session resumed while the owner merely holds the Work's lock for a moment (a tick recording a
verdict) joins that Work, as `/clear` does. Attribution follows the worktree, never the
session ID, since `/resume` can carry a conversation into another worktree.

When the earlier Work's S0 can no longer be read (Git prunes unreferenced objects in time),
the session starts Work of its own from the worktree as it is, the earlier Work stays for
its own review, and the new Work records `work.not_reopened {reopens, reason}`.

```sql
SELECT run_id, json_extract(payload_json, '$.reopens') FROM events WHERE event_type = 'work.reopened';
SELECT run_id FROM events WHERE event_type = 'work.closed' AND json_extract(payload_json, '$.by') = 'reopen';
SELECT run_id, json_extract(payload_json, '$.reopens'), json_extract(payload_json, '$.reason')
  FROM events WHERE event_type = 'work.not_reopened';
```

### Waiting on you

Claude Code's `Notification` hook (matched to `permission_prompt`, `idle_prompt` and
`agent_needs_input`) and `Stop` hook mark the worktree's Work in a watched project, and never
reply, so they never keep Claude working:
- `permission_prompt` is `permission`; `idle_prompt` and `agent_needs_input` are `input`;
  `Stop` is `turn ended`, "waiting for your next prompt". Any other notification changes
  nothing. A newer mark replaces the older one.
- The mark is recorded on the Work (`attachment.needs_you`) and as `runtime.needs_you {kind,
  message, at, transcript, session_id}`.
- `dispatch status` prints `waiting on you: permission · Claude needs your permission · for
  2m`. `dispatch watch` lists the Work under "needs you", with state `waiting` and the next
  step `Its session asks for your permission: answer it there.` or `Its session waits for
  your next prompt: continue it there, or f finish when it is done.`; a piped row ends with `·
  waiting on you: <kind> · <message>`. `watch --json` work lines carry `needs_you: {kind,
  message, since}` or `null`, and `state` stays `working`.
- The owner clears it when the session moves again, and records `runtime.needs_you_cleared
  {by}`: `transcript` when Claude wrote its transcript after the mark (for `turn ended`, more
  than 10 s after it, since Claude records the end of a turn itself after the hook);
  `session_end` when the session ended; `work_finished` when the Work is no longer Working.
  A session end hides the mark at once, before the owner clears it.

```sql
SELECT run_id, json_extract(payload_json, '$.kind'), timestamp FROM events WHERE event_type = 'runtime.needs_you';
SELECT run_id, json_extract(payload_json, '$.by'), timestamp FROM events WHERE event_type = 'runtime.needs_you_cleared';
```

### Work that appears without hooks

`orchestrator::discovery`, on every pass of the owner's fast loop (about once a
second), with no hook installed:
- **What is looked at.** `git worktree list --porcelain` for the root. No directory is
  walked: per worktree, Dispatch reads its `.git` file and the two files Claude Code
  keeps in `.git/worktrees/<name>/`. A plain-directory root has no worktrees.
- **What counts as Claude's.** A linked worktree under `<root>/.claude/worktrees/`
  whose Git directory holds `CLAUDE_BASE`. Recognizing it belongs to the Claude
  adapter (`runtime::claude::made_by_claude`). Worktrees a person made (`git worktree
  add`) are never registered, listed or logged. Missing and prunable entries are
  skipped. Codex's, Cursor Agent's and Hermes Agent's worktrees are recognized too, by
  their own signs (below). Other agents' layouts are not recognized; `dispatch attach -- <agent>` gives
  any agent a workspace of its own.
- **The first scan of a watch** puts every Claude worktree without Work into the
  inventory, `watchers/<key>.inventory.json` (path, first seen, `CLAUDE_BASE`, the lock
  reason verbatim, and which watch took it). Nothing is registered on that scan: what
  those worktrees held before is unknown. The inventory survives an owner restarted after
  a crash and is removed when watching stops (`dispatch stop`, Ctrl+C on `serve`), so the
  next `dispatch start` takes a new one.
- **Later scans.** A worktree that appears after the first scan:
  - locked `claude session <name> (pid <pid> start <time>)` by a process that runs with
    that start: registered;
  - locked that way by a process that is gone, as a `claude -p` run between two passes
    leaves it: registered, with its session recorded as ended (`end_reason:
    "process_gone"`), so it reads `idle` at once;
  - locked that way, but the process's start cannot be read: looked at again next pass;
  - any other lock, or none (a subagent's or background session's worktree, whose lock
    has not been observed): listed in the inventory after a 3 s grace, since Claude Code
    writes `CLAUDE_BASE` a moment before it locks a new worktree. Each is noted once in
    the owner's log.
- **Registration** takes the workspace lock without waiting, publishes a hook's pending
  registration first, and registers nothing if Work exists by then. It uses the hook's
  assembly and publication path:
  - S0 is the merge base of the worktree and the root, provenance `Discovered { commit
    }`, confidence full, owner the runtime;
  - the one session is `lock:<name>@<pid>`, `source: "discovered"`, started at the
    lock's start;
  - `attach.created` records `via: "discovery"`;
  - authority to run checks comes from the project's check consent, as for a hook.

  `status` and `explain` show it as found by Dispatch. A failed registration is noted
  once per owner process and retried every pass.
- **One Work per worktree.** A worktree with Work is neither listed nor registered. A
  hook's `SessionStart` before or after joins the same Work. A listed worktree that is
  removed leaves the list, and one made again at that path is new. Work whose worktree
  Dispatch saw removed leaves the path free once that Work is finished. Work that
  finished while its worktree stayed keeps it: a later session there is not turned into
  Work without a hook, since Dispatch could not know where it began.
- **Removal without hooks.** With no `WorktreeRemove` hook, a worktree Claude Code
  removes is seen missing by the owner, as in "Workspace removal" above.

**Codex and Cursor Agent** (`discovery::UNOBSERVED`, `runtime::codex`,
`runtime::cursor`). After Claude's test, each worktree from the same list is tried
against one sign per runtime:
- **Codex** (`codex exec --worktree`): `codex-thread.json` in the worktree's Git
  directory, found through its `.git` file without running Git, with a non-empty
  string `ownerThreadId`. The marker alone decides, wherever the worktree is; Codex
  puts its own under `~/.codex/worktrees/<4 hex>/<project folder>`, detached at HEAD.
  The session is the thread ID.
- **Cursor Agent** (`cursor-agent --worktree <name>`): the path Git lists is exactly
  `canonicalize(~/.cursor/worktrees)/<project folder, lowercased>/<name>`, and its
  branch is exactly `refs/heads/<name>`. Cursor lowercases the folder name, so the
  folder as written is not accepted, on macOS and Linux alike. One path is tested and
  `~/.cursor` is never walked. The session is the worktree's name. Because the sign is
  a place and a branch, a worktree a person makes by hand at that path on that branch
  counts as Cursor's.

Such a worktree follows Claude's rules for the first scan (listed, not tracked) and
for one Work per worktree. One that appears later is registered as soon as its sign
is there, with no grace period: S0 is its merge base with the root, `workspace_owner:
runtime`, and the agent `codex` or `cursor`. Its one session has `source:
"discovered"` and `started_at` from the mtime of the worktree's `.git` file, when Git
made it. The owner's log says `found a Codex session in <path>; tracking it as Work
<id>; its end is not observable: …`.

Neither runtime records anything that names its running process, so their liveness is
`Unknown` for good: no `agent_process`, no end, never `agent running`, never `idle` by
itself. Cursor's lingering `worker-server` process is not the agent, and nothing is
scanned for one. `status` and `explain` say so and name the next commands:

```text
the end of a Codex session is not observable — when it is done, finish to freeze its changes (dispatch finish <id>); or close it: dispatch close <id> --reason unfinished
```

`watch` shows its origin as `codex session in its own worktree, found by Dispatch; its
end is not observable`. Hooks for these runtimes are not installed or read.

**Hermes Agent** (`runtime::hermes`), observed with Hermes Agent 0.18.2 and read from
its source:
- **The sign.** `hermes chat --worktree` makes `<root>/.worktrees/hermes-<8 hex>` on a
  new branch `refs/heads/hermes/hermes-<same 8 hex>` (lowercase hex), and locks it at
  once with `hermes pid=<pid>`, its own process, with no start time. Both the path Git
  lists and the branch must match. `hermes -z` ignores `--worktree`. Hermes also adds
  `.worktrees/` to the checkout's `.gitignore`.
- **Registration** follows Claude's rules, with that lock in place of `claude session`:
  a worktree there on the first scan is listed, not tracked; a later one locked by a
  running process is registered with that process, one whose process is gone is
  registered with its session ended (`process_gone`), and one without the lock is
  listed after the 3 s grace. Hermes locks its worktree a moment after making it; a
  worktree listed before its lock appeared is registered once the lock names the
  process that made it. S0 is its merge base, the agent `hermes`, and its one
  session is the worktree's name, `hermes-<8 hex>`, with `source: "discovered"` and
  `started_at` from the mtime of the worktree's `.git` file. Hermes's own session ID
  never reaches discovery and is not recorded. The owner's log says `discovery: found a
  Hermes session in <path>`.
- **Liveness** ("Liveness from the session lock", below): the pid alone could be
  recycled, so the process counts as running only if it started no later than the
  worktree's `.git` file was written (plus 1 s for `ps`'s resolution). One that started
  later is gone; an unreadable time is `Unknown`. Hermes removes its worktree as its
  process exits, and Dispatch usually notes the removal first, by the hook or the fast
  loop; the process it recorded is then followed by its exact identity until it is gone,
  and the session ends `process_gone`, with or without the hook.
- **It vanishes.** When the session ends, Hermes runs its `on_session_finalize` hooks,
  then unlocks the worktree, runs `git worktree remove --force` and `git branch -D`,
  uncommitted changes and all. It keeps the worktree only when it holds commits on no
  remote-tracking ref; a repository with no remote-tracking refs counts as having none,
  so even committed work goes. Hermes is the one entry of `VANISHING_WORKTREES`, so the
  fast loop keeps its Work's `delta-live.patch` fresh, and a removal Dispatch did not
  hear about leaves the last-seen Δ ("Workspace removal", above).
- **The hook.** `dispatch setup` → Runtime integrations → `Install the Hermes hook`
  appends one block to `~/.hermes/config.yaml` (`$HERMES_HOME/config.yaml` when that is
  set):

  ```yaml
  # Added by dispatch setup, which removes it unless it is edited.
  hooks:
    on_session_finalize:
      - command: "'<dispatch>' --state-dir '<state>' hook hermes"
        timeout: 300
  ```

  - The consent screen shows that block and the path, with focus on Cancel. The file is
    only read (to find a top-level `hooks:` key and to check the block reads back as
    written), never rewritten: the block is appended after a backup beside the file
    (`config.yaml.dispatch-backup-<time>`), and every other byte is kept. A config that
    already has `hooks:` is left alone, and setup shows the lines to add under it.
  - `Remove the Hermes hook` takes out exactly that block, found by its marker line and
    regenerated for comparison, and gives the file its bytes back. It refuses when the
    block was edited, added by hand, or rewritten: *"Dispatch's hook in <path> is not as
    Dispatch added it; it was edited or added by hand, so remove it there yourself"*.
    `hermes config set` rewrites the file with PyYAML, which drops the marker, so after it
    the hook must be removed by hand ("Upgrading to 0.4.15" in
    [release-install.md](release-install.md)).
  - Setup's status line is `Hermes hook: installed`, `not installed`, or `unknown, its
    config.yaml is unreadable`; a hook added by hand counts as installed.
  - Hermes asks the person to allow the hook the first time it runs it, and a run without
    a terminal needs `hermes --accept-hooks`. Dispatch never writes Hermes's allowlist or
    sets `hooks_auto_accept`. The allowlist is keyed by the command, so if the Dispatch
    binary moves, Hermes asks again.
  - The timeout is Hermes's most, 300 s, which covers the minute keeping a removed
    worktree's Δ may wait for its registration and the minute it may wait for the Work's
    lock. Hermes waits for the hook before it removes the worktree.
- **What the hook does** (`dispatch hook hermes`, reading Hermes's payload on stdin and
  the worktree from `TERMINAL_CWD`, since the payload's `cwd` is the main checkout):
  - It acts only on `on_session_finalize` with `reason: "shutdown"`, which is the CLI's
    process exit, right before the removal. `/new` fires it with `session_boundary`, and
    the TUI with no reason, while the worktree lives on; `on_session_end` ends every turn.
    All of those are ignored, so that Work falls back to the last-seen Δ.
  - `TERMINAL_CWD` must be the top of a Hermes-shaped worktree, by path and branch, of the
    same repository as the payload's `cwd`, and the worktree's lock must name the hook's
    parent, the process about to remove it. Anything else is silent.
  - If Hermes will remove the worktree (by its own rule above), the hook keeps the exact Δ
    through the same path as Claude Code's `WorktreeRemove`: `workspace.removed` with
    `exact: true`, and the Work waits for `finish` or `close`, or is finished by the
    project's check consent. If Hermes will keep it, the hook records only the session's
    end (`runtime.session_ended`, reason `shutdown`).
  - It registers nothing. A worktree discovery never saw, in a project that is not watched
    or removed before a scan, is not kept.
  - It prints nothing. On an error it exits 1 with the reason on stderr; Hermes only logs
    it and removes the worktree anyway, and the last-seen Δ is what remains.

### Liveness from the session lock

`orchestrator::liveness`, on every owner tick, for attached Work whose workspace the
runtime made (`workspace_owner: runtime`) and is still a Claude or Hermes worktree, while
it is unfinished or, once finished, while it still records its agent process. A Hermes
lock has no start, so its process is judged by when the worktree was made (above), and
when another pid takes a Hermes lock no session is ended for it:
- **Running.** When the lock names a process that runs with the lock's start, the owner
  records its exact identity on the attachment once (`agent_process`,
  `runtime.agent_running`). `status`, `explain` and `watch` show `agent running (pid N)`.
  Work discovery registers already carries that process from registration, since
  discovery has just seen it run, so a `claude -p` session that ends before the next tick
  still has its end recorded (found in the 0.4.13 real-agent trial).
- **Gone.** After that, the open sessions are ended as a hook's `SessionEnd` would end
  them (`runtime.session_ended`), and the identity is cleared (`runtime.agent_gone`):
  - `process_gone` when the lock still names that pid and the process is gone, or a
    process with another start now has the pid;
  - `lock_released` when the lock was removed, or another session's lock replaced it.
    Sessions that began before the new lock's start are ended; the new process is
    recorded if it runs.

  The Work then reads `idle`, with "session ended — finish, resume or close".
- **After `finish`.** Finishing Work does not stop following its agent. When `finish`
  comes before the session ends, or before the next tick sees it gone, the end is still
  recorded the same way once that exact process is gone or its lock is released, and
  the agent is cleared. Finished Work never takes on a new agent: a later lock ends the
  earlier session (`lock_released`) and records no process. In 0.4.13, Work finished a
  few seconds after its agent exited kept its session open for good (found by the
  0.4.14 conformance trials). The finish, its verdict and its Δ are not touched.
- **After removal.** A removed workspace has no lock left. While the Work still records
  its agent process, that process is followed by its exact identity alone: once it is
  gone, or its pid was reused, the open sessions end (`process_gone`) and the process is
  let go, as above. Until then nothing happens. This is not Hermes's alone: a Claude
  session whose worktree was removed while it ran ends the same way.
- **Nothing else.** A lock never seen running, or a process whose start cannot be read,
  proves nothing, and nothing is recorded. A session a hook already ended stays as it
  was, so there is one end. Liveness itself finishes, applies, closes and signals nothing;
  an end it records can let the owner finish the Work by consent ("Session end and
  resume"). A
  person's attach (`workspace_owner: user`), with or without `--pid`, is never touched.
- **Cost.** One `ps` per followed lock per tick, plus one identity lookup when a new
  process is recorded. The identity is written under the run's lock only when it
  changes; a busy run is left for the next tick.

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

- **A decision without an ID needs exactly one candidate.** `dispatch accept` or
  `dispatch reject` with no ID (`sole_awaiting_review`) acts only when exactly one
  result for the source waits for review: finished, ready, review pending, not
  applied and not closed, native or attached. With none or several it changes
  nothing, exits 1, and lists each waiting result as `dispatch <accept|reject>
  <full id>  (task)`, and Work still in progress as "not counted". An explicit ID
  keeps the unique-prefix rule; the full ID is used for locks, writes and events.
  Every finish and result summary prints its commands with the full ID.
- **The delivered-result guard.** A review is a judgment of a delivered result; an
  attached run that has not been finished yet has no Δ to judge. `reject` on
  unfinished attached Work refuses and names `dispatch close <id> --reason unfinished`
  (or `workspace-removed` when its workspace is gone). Accepting an active
  attachment refuses: *"run `<id>` has no delivered result to review; finish attached
  work or answer the pending question first."* Closed Work cannot be accepted, rejected
  or finished.
- `dispatch refresh` has no meaning for attached work — there is no Dispatch task to
  relaunch — and is refused: *"attached work has no Dispatch task to refresh; finish,
  reject or close it."* For a stale result, run the agent again on the current source,
  and once that Work is finished, close the old one as superseded by it.

### Closing Work

`dispatch close <id> --reason <reason>` retires Work without a quality judgment. It
records one `work.closed` event (`{reason, by: "human", world_digest?, replacement?}`)
and sets `outcome.closed`. It writes no `review.*` event and no feedback row, and
leaves earlier reviews as they are. It never applies, commits, signals an agent or
deletes a workspace. It takes the run's operation lock, waiting up to 5 s as `finish`
does, so Work a live wrapper owns is refused.

| Reason | For | Requires |
|---|---|---|
| `unfinished` | unfinished Work whose workspace is there | its changes are frozen into `delta.patch` first, without checks |
| `workspace-removed` | unfinished Work whose workspace is gone | the exact Δ the hook kept, or the last-seen Δ, stays |
| `already-present` | a result waiting for review | the whole retained patch is already in the source (`git apply --check --reverse`); the world digest is recorded. Part of it is not enough |
| `superseded` | unfinished Work or a result waiting for review | `--replacement <id>`: different, finished, ready Work on the same source, neither closed nor rejected, with a non-empty retained patch |

- Closing unfinished Work sets `lifecycle: finished`, `work_result: cancelled`,
  `review: not_requested`. Closing a result keeps its Δ, `work_result: ready` and
  `application`, and sets `review: not_requested`. Either way the Work leaves pending
  review and ID-less decisions, and reads as `closed: <reason>` in `status`,
  `history`, `watch` and `check`.
- Reviewed or applied Work cannot be closed, and closed Work cannot be closed again.
- `--explanation` or `--explanation-file` stores the person's words verbatim.
- Repairing a workspace is explicit: attach the repaired workspace as new Work, finish
  it, then close the old Work with `--reason superseded --replacement <new-id>`. Both
  patches are kept. A source-only repair needs no close: `check` the same Work again.

## Events

In addition to the existing coherence, application and review events (see
[coherence.md](coherence.md#events)), attached work commits:

| Event | When | Payload |
|---|---|---|
| `attach.created` | `attach` creates the Work (both forms) | `{"attachment": <AttachmentRecord>}` |
| `attach.source_observed` | the one observation of the source at the start (see "The source at the start") | `StartObservation`: `{"at", "source_head", "differing", "paths": [{"path", "applied_work"?}]}` |
| `attach.started` | the wrapped form's agent process has been spawned | `{"agent_process": <ProcessIdentity>}` |
| `attach.finished` | `finish` (either explicit or on agent exit) freezes Δ | `{"reason": <FinishReason>, "workspace_removed": <WorkspaceRemoval> \| null}`; `workspace_removed` since 0.4.15, `{at, exact, last_seen_at}` |
| `attach.adopted` | `serve` takes over observation of an orphaned run | `{"owner_state": "adopted"}` |
| `attach.authorized` | Work gains local authority: from the project's check consent (at registration, or a typed `attach` or `finish` that ran on it), or from a person's `finish --allow-unsafe-local` | `{"unsafe_local": true, "by": "project consent" \| "human"}` |
| `runtime.session_started` | a hook records a new session on existing Work | `{"session": <RuntimeSession>}` |
| `runtime.session_ended` | a hook's `SessionEnd`, Hermes's hook when Hermes keeps its worktree, or the owner from the session lock ("Liveness from the session lock") | `{"provider", "session_id", "reason"}`; Hermes's hook gives `shutdown`, the owner's reasons are `process_gone` and `lock_released` |
| `runtime.agent_running` | the owner records the process a session lock names, once it runs | `{"agent_process": <ProcessIdentity>, "previous"}` |
| `runtime.agent_gone` | the owner clears it, once that process is gone or its lock released | `{"agent_process": null, "previous": <ProcessIdentity>}` |
| `runtime.needs_you` | Claude Code's `Notification` or `Stop` hook marks the Work (0.4.17) | `{"kind": "permission" \| "input" \| "turn_ended", "message", "at", "transcript", "session_id"}` |
| `runtime.needs_you_cleared` | the owner clears that mark (0.4.17) | `{"by": "transcript" \| "session_end" \| "work_finished"}` |
| `work.reopened` | a resumed session's Work begins from an earlier Work's S0 (0.4.17) | `{"reopens": <old id>, "provenance"}` |
| `work.not_reopened` | it could not, because that S0 can no longer be read (0.4.17) | `{"reopens": <old id>, "reason"}` |
| `workspace.removed` | the workspace is gone: kept exactly by a hook (Claude Code's or Hermes's), or seen missing by the owner | `{"exact": true, "files_changed"}` or `{"exact": false, "no_changes", "last_seen_at"}`; `last_seen_at` since 0.4.15 |
| `workspace.released` | Dispatch removed a workspace it made | `{"workspace", "reason": "applied" \| "cleaned"}` |
| `work.closed` | a person closed Work (`dispatch close`, or `c` in `watch`), or a resumed session superseded it (`by: "reopen"`, 0.4.17). Before 0.4.12, a person rejected unfinished Work whose workspace was gone | `{"reason": "unfinished" \| "workspace_removed" \| "already_present" \| "superseded", "by": "human" \| "reopen", "world_digest"?, "replacement"?}` |
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
  event, `attachment.owner_state`/`finished_at`/`finish_reason`,
  `attachment.source_at_start` (recorded once, never recomputed) and `outcome.closed`.
- **Recomputed on every use**: the world, symbol tables, facts and the verdict shown by
  `check`, `status`, `explain` and the finish summary — exactly as for
  native runs. `serve`'s view line reads the *last stored* verdict; it does not
  recompute one for display beyond what its own tick already evaluated and persisted.

## Restart and crash behavior

- **`serve` restart.** Active attached Work is read back from the database
  (`mode = attached`, `lifecycle != Finished`). The wrapper's stored `ProcessIdentity`
  (and the agent's, if known) is re-checked with `identity_state`: `ExactLive` is left
  alone (a live wrapper owns it); a stored `Live` owner that is now `Gone`/`Reused` is
  adopted (above); a foreign attachment with no owner, or an owner whose liveness
  cannot be told (`Unknown`), is observed but not marked adopted. Nothing is finished,
  applied or relaunched by a restart. A restarted owner neither repeats nor recomputes a
  start observation, and runtime-registered Work published while no owner ran stays "not
  observed".
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

- No socket and no push: `dispatch start` runs one local background process per
  project (`serve --background`), and it coordinates with every other command through
  SQLite (WAL, revision-fenced projections) plus `flock` files. `serve` finds new
  worktrees within about a second, and re-evaluates changed Work on its next tick (at
  most `poll_secs`, default 10 s). This is adequate for work measured in minutes to
  hours, not for sub-second push updates. Polling is a cadence, not a guarantee: a
  worktree that lives and vanishes between two passes is missed.
- Interactions between Work are advisory, exist only while an owner watches the
  project, and never change a verdict or block anything.
- `serve` never launches or refreshes anything, and kills only the checks of its own
  finishes when it cancels them. It finishes Work only when the project's check consent
  holds and either a runtime removed the Work's workspace with its exact Δ kept, or a
  Claude Code or Hermes session ended with its process gone; Work kept only as last
  seen waits for a person. Otherwise wrapped attach and a human (`dispatch finish`)
  are the only things that finish a Work.
- A finish by consent that fails (an error, not failing checks) is not tried again by
  the same owner; finish it yourself, or it is tried again when the owner restarts.
- A worktree removed while its finish by consent runs: the runtime's removal waits up
  to 60 s for the finish, then fails, and the worktree stays. Its changes were already
  kept when the finish began.
- "Waiting on you" comes only from Claude Code's hooks. A `turn ended` mark clears at
  the first transcript write more than 10 s after it, so a prompt typed within those
  10 s clears it only at the agent's next write; a person's own `Stop` hook slower
  than 10 s clears it early.
- Owners come back only when a Dispatch command or hook runs in the project; there is
  no launchd or systemd agent.
- No foreign process is ever signaled. `--pid` and the wrapper's own child PID are
  liveness-only.
- Discovery recognizes only Claude Code's, Codex's, Cursor Agent's and Hermes Agent's
  worktrees, and liveness reads only a `claude session …` or `hermes pid=` lock.
  Claude's was observed with Claude Code 2.1.296 on macOS; the lock of a subagent or
  background-session worktree, and the format on Linux, were not. Codex's marker and
  Cursor's location were observed with Codex 0.155.1 and Cursor Agent 2026.10.14 on
  macOS, and Hermes's place, branch, lock and hook with Hermes Agent 0.18.2 on macOS,
  in one confirming session. The end of a Codex or Cursor session is not observable.
  There is no process scanning or association by working directory.
- Hermes's exact capture covers only the CLI's shutdown. Its TUI (`--tui`) and `/new`
  fall back to the last-seen Δ, which misses changes made after the last pass. Without
  the hook, a tick that falls between Hermes's `git worktree unlock` and its removal
  ends the session as `lock_released` while Hermes still runs; it is still an end,
  and the process is let go.
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
