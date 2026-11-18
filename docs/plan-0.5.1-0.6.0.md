# Dispatch 0.5.1 and 0.6.0: work that orchestrating agents start, kept coherent

## Context

The user works the way many developers now do: they tell their coding agent of choice to spawn subagents or
sessions and orchestrate them. They want Dispatch to be the obvious layer under that orchestrator, as
seamless as a connector (user, 2026-11-16 and 2026-11-18: "Like connectors but for agent terminal sessions";
"I love the integration of my idea for the 'mcp' and subagent orchestration… Let's move fast!").

0.5.0 made Dispatch coordinate Work it knows about: notes, catch-up in place, and one-decision landing. Two
gaps keep it from the orchestrator's path:

1. **Subagents are invisible.** A Claude Code subagent in its own worktree is "Found, not tracked". It gets
   no S0, notes, catch-up or batch landing.
2. **Orchestration is done by hand around Dispatch.** Launching, prompting, polling a screen, answering,
   following up and ending each worker happen outside it.

**Decisions (user, 2026-11-18):**
- **Release shape:** 0.5.1 first, in which Claude Code subagents become Work. Then 0.6.0, the `dispatch mcp`
  connector.
- **Launching:** an orchestrator may launch agent sessions through the connector under a standing consent
  the user gives once per project at a terminal. The consent names which agents may be launched and how
  many may run at once. No retries, and no relaunch on its own.
- **Messaging:** an orchestrator's messages to live sessions are delivered verbatim, labeled as from the
  orchestrating session, and recorded. Dispatch's own asks stay fixed text, once per landing.
- **Landing:** always the user. The connector returns the proposal and the `dispatch accept --expect`
  command. There is no landing tool.
- **Standing:** coordination is not task splitting. The orchestrator splits the task; Dispatch never does.

## Evidence

### Probe (`~/dispatch-dev/p0-subagents/FINDINGS.md`)

Run 2026-11-18, approved, on Dispatch 0.5.0 and Claude Code 2.1.296.

- **Where subagents work.** A subagent started with the Agent tool and `isolation: worktree` works in
  `<root>/.claude/worktrees/agent-<agent_id>`, on branch `worktree-agent-<agent_id>`, from HEAD.
- **Its lock** reads `claude agent agent-<agent_id> (pid <parent pid> start <parent start>)`. It is written
  by `SubagentStart` and removed after `SubagentStop`. 0.5.0 reads only `claude session …`, so all five
  subagent worktrees were "Found, not tracked".
- **What the hooks carry.**
  - `SubagentStart` fires before the subagent's first tool call, with `cwd` = its worktree, `agent_id` and
    `agent_type`.
  - Every tool hook inside a subagent carries the same fields.
  - `SubagentStop` carries `stop_hook_active`, `agent_transcript_path` and `last_assistant_message`.
  - All of them carry the parent's `session_id`, so Work must be keyed by `agent_id`.
- **Both channels reach subagents.**
  - A `PostToolUse` `additionalContext` (notes) reached both subagents.
  - A `SubagentStop` `{"decision":"block"}` (catch-up) kept both working. Each then told its parent the
    request went beyond its task.
- **Background sessions are tracked.** `claude --bg --worktree` sessions are already Work in 0.5.0, with a
  `claude session` lock.
- **Claude Code can be asked who is waiting.** `claude agents --json` lists every session with status and
  `waitingFor` (for example `"permission prompt"`).
- **The orchestrator saw the A/B conflict itself,** and had no way to act on it through Dispatch.
- **MCP:** Claude Code let tool calls of 200 s and 650 s finish normally (209 s and 663 s end to end,
  with no cancel), and it passes a `progressToken`.
- **Hermes:** `delegate_task` children run in the parent's own directory (`TERMINAL_CWD`), with no worktree.
  They are part of the parent's Work, so there is nothing to add for them.

### Baseline (`~/dispatch-dev/notes-0500/orchestration-baseline.md`)

From 0.5.0's build log:
- **Hand steps:** at least 5.5 per landed Work outside Dispatch: 27 launches, 44 prompt deliveries, 12 or
  more typed answers and 27 endings for 20 landed Works. Screen polls were never logged.
- **Hand-orchestration failures:** 8.
  - Q waited 24.6 h on an approval the integrator could not give.
  - R and X lost an hour to a wait that matched a heading R never wrote.
  - Stray keystrokes went into Hermes.
  - Answers the integrator typed were recorded as the user's.
- **Packet C trials:** about 13.5 steps per trial. Codex needed 15 launches for 6 working sessions.

### Code (`~/dispatch-dev/integration-0500` = v0.5.0; two read-only maps and one design review, 2026-11-18)

- **Keying.**
  - Work is keyed by its canonical worktree path. The note inbox and catch-up files are keyed by
    sha256(workspace) (`inbox.rs:62`, `catchup.rs:632-662`).
  - So once a subagent's worktree is Work, notes and asks already reach the right subagent.
- **Why a subagent registers nothing today.**
  - `parse` (`runtime/claude.rs:64-127`) ignores `Subagent*`.
  - `PostToolUse` returns before `parse` (169-171).
  - `parse_lock` (691) reads only `claude session `, so discovery files the agent lock as `Lock::Other`
    and lists the worktree.
- **What a shared parent `session_id` breaks.**
  - `find_session_attachment` (`attach.rs:1570`) returns the newest active Work with a session id, so a
    parent's SessionEnd could end a subagent's Work.
  - `publish_session_first` (`registration.rs:662`), `waiters/<session_id>` (`claude.rs:298-325`) and
    `NeedsYou` matching (`attach.rs:1418`) all key by session id.
  - All subagents' locks name one parent pid.
- **For the connector.**
  - No crate is needed: `serde_json` and `tokio` are already in `Cargo.toml`.
  - Every printing path must stay off stdout: `publish_event` in Jsonl mode, `print_proposal`, the
    consent prompt. `RunOutputMode::Silent` exists.
  - There is no one "all Work" function. The rows are assembled in `presenter/watch.rs:920-963`.
  - `status_json` (`orchestrator.rs:1918`) covers one Work.
  - The batch `offer` (`batch.rs:413`) already builds the structured payload it records as
    `batch.proposed`.
  - Native runs are one-shot per harness (`harness.rs:345`) and supervised in the foreground.
    `background::command` (`background.rs:406`) is the detached-spawn pattern.
  - Native consent is exact-match and granted only at a terminal (`consent.rs:184-228`,
    `orchestrator.rs:454-533`).
  - Notes are written by `inbox::write` (`inbox.rs:103`) and delivered only through hooks.

## 0.5.1 "Subagents are Work" (Claude Code)

### Packets

**P0b, open questions (integrator, approved with this plan, on the same scratch project, under 30
minutes).** These confirm on 2.1.296 what the hooks docs say:
- **Worktree removal.** Is an unchanged subagent worktree removed with no `WorktreeRemove`? When is a
  changed one removed?
- **`SubagentStart`.** Does its `additionalContext` reach the subagent?
- **Repeats.** Do `SubagentStop` and `SubagentStart` repeat when the parent sends a running subagent
  another message?
- **Without isolation.** What does a subagent without isolation send (`cwd` = the parent's directory,
  with `agent_id`)?
- **Its base.** Does a subagent without `worktree.baseRef: head` branch from the default branch?
- **`claude stop`.** What does stopping a background session leave in `dispatch status`? The first probe
  stopped its owner too early to say.

The answers become fixtures and tests for A. If one contradicts the docs, A follows what was observed.

**A, subagents become Work (one Claude Code worker, Opus 5.5).** The design was reviewed against the code;
the review's eight findings are folded in, and the line references below are into
`~/dispatch-dev/integration-0500`.
1. **Hook input.** `Input` gains `agent_id` and `agent_type`, with a leading `agent-` stripped from
   `agent_id`.
   - `SubagentStart` is parsed as a `Start` with source `subagent`. `resumed` is set to false
     explicitly (the SessionStart rule at `claude.rs:76` would mark it Partial), and the start is kept
     out of the shared-checkout notice.
   - S0 is the subagent worktree's own HEAD at `SubagentStart`. An isolated subagent may branch from the
     default branch rather than the root's HEAD.
   - `SubagentStart` also delivers pending notes as `additionalContext`, as SessionStart does.
2. **Session identity.** A subagent's session id is `<parent session_id>_agent-<agent_id>`.
   `runtime::session_id` (`runtime.rs:151-157`) admits only letters, digits, `-` and `_`.
   - `RuntimeSession` gains serde-default `parent_session`, `agent_id` and `agent_type`. Each subagent's
     publication, needs-you and session end stay its own.
   - A `Notification` that carries `agent_id` maps to the same composite id.
   - The parent's own Stop and SessionEnd find no subagent Work (`inbox.rs:291-301`), which is
     correct.
   - **The resume advice for subagent Work:** "start Claude Code in <worktree>", never `claude --resume`
     (`orchestrator.rs:1055-1090` and ~3297).
3. **Isolation guard, in every path including the head-only Stop fast path.** A payload with `agent_id`
   registers, takes notes or claims an ask only when its workspace is
   `.claude/worktrees/agent-<agent_id>`.
   - Without that guard, a subagent without isolation, a teammate, or Claude Code's internal agents
     (prompt suggestions, `/btw`), working in the parent's directory, could take the parent's notes or
     ask.
   - The guard also stops a leak that has been there since 0.4.18.
4. **The agent lock.** `Lock::Agent { id, pid, start }` is read in `parse_lock`, discovery and liveness.
   - **The marker:** Claude Code writes `CLAUDE_BASE` for agent worktrees (checked on the probe
     project), so `made_by_claude` holds.
   - **Discovery** registers a missed subagent worktree as `Discovered` (S0 = its merge-base).
   - **The process:** never record `agent_process` from an agent lock, since it names the shared parent.
     That covers discovery's `register(…, Some(pid))` (`discovery.rs:290`) and liveness's `running()`
     (`liveness.rs:111`).
   - **Liveness:** an agent branch in `observe_run` ends every open session of that Work when the agent
     lock is gone, or when its pid is gone (the parent exited). That includes discovery's `lock:agent-…`
     session.
5. **SubagentStop runs once per response, not once per subagent,** so it never ends the session.
   - **The ask:** if `stop_hook_active` is false and an ask is pending for its own workspace (the guard
     in 3), it blocks with the ask (`catchup.delivered {via: subagent_stop}`).
   - **Otherwise it freezes the exact Δ,** as `freeze_removed_workspace` does, with the timeout raised
     to about 60 s. Claude Code removes an unchanged subagent worktree with no `WorktreeRemove` (the
     docs say so). Without a frozen Δ, a short subagent would leave a "lost" Work, and a removal racing
     liveness could capture a half-deleted tree (`attach.rs:1782-1785`).
   - **The session ends** only by the agent branch in 4.
   - **No waiter for subagents.**
6. **Finish and land.** When the session ends, finish by consent works from the frozen Δ
   (`finish_locked` already rebuilds from one, `attach.rs:1766-1781`).
   - An empty Δ closes the Work as `no_changes`.
   - Otherwise the result joins `dispatch accept`'s batch like any Work. Its task reads `claude subagent
     <agent_type> in agent-<id prefix>`.
   - **When the parent merged the subagent's branch** into its own Work, the batch leaves the subagent
     Work out as already applied. It never lands the change twice. Tested.
7. **Hooks.**
   - `INSTALLED` gains `SubagentStart` (60 s) and `SubagentStop` (60 s), and 0.5.0's set moves into
     `EARLIER` (`[_; 4]`).
   - Setup offers the Update once, with one added sentence: subagents in their own worktrees become Work.
8. **Tests,** with stand-in payloads in the probe's exact key orders:
   - **Identity:**
     - three subagents under one parent session id;
     - an `agent_id` with and without the `agent-` prefix;
     - the parent's SessionEnd and Stop leave subagent Work alone;
     - a subagent's Notification clears only its own needs-you.
   - **Channels:**
     - a note at `SubagentStart` and at `PostToolUse`;
     - two `SubagentStop`s in a row: an ask blocks once, and both freeze Δ while the session stays
       open;
     - a `SubagentStop` over 1 MB;
     - a non-isolated subagent and an internal agent in a Work's worktree take nothing.
   - **Ends:**
     - an unchanged worktree removed by Git closes the Work as `no_changes`;
     - a changed one removed after its lock is released finishes from the frozen Δ;
     - the parent exiting ends its subagents' sessions.
   - **Discovery** of an agent lock records no process.
   - **Hook-set round trips.**
   - **End to end:** changer and user subagents, then one batch proposal, the changer first and the user
     left out as REFRESH. A subagent whose branch the parent merged is left out as already applied.

**R, docs, version 0.5.1, release notes with `Site impact:` (Claude Code worker, last).**
- `attach.md` and the Claude Code guide: subagents, the lock, the two hooks, and what is not tracked
  (subagents without isolation).
- `harness-conformance.md`: a "subagents" cell.

**C, conformance (integrator, real trial, approved with this plan).**
- An orchestrator Claude Code session spawns the B/A/C tasks as worktree subagents, on the release build.
- **Expected:**
  - 3 Works with S0 at spawn and 0 isolated subagents "Found, not tracked";
  - a note to a subagent;
  - finish by consent;
  - one batch proposal with the changer first;
  - a `SubagentStop` catch-up when a landing happens while a subagent still runs.

### Exit criteria (0.5.1)

- No isolated subagent left "Found, not tracked" in C. S0 at spawn for each.
- A note and a catch-up reached a subagent in C, each once.
- Subagent results landed through one batch approval.
- The parent's end ended no subagent Work, and no waiter process belongs to a subagent.
- **Hook timing:**
  - `SubagentStart` p95 at most 15 ms over 1,000 calls;
  - `SubagentStop` with an ask p95 at most 15 ms;
  - `SubagentStop` freezing Δ, measured and reported against its 60 s timeout.
- No subagent Work left "lost": unchanged ones close as `no_changes`, and changed ones finish from their
  frozen Δ.
- CI green on all three jobs. Production LOC reported (estimate +350 to +550).

## 0.6.0 "Your orchestrator, connected": `dispatch mcp`

A local stdio MCP server (JSON-RPC 2.0, one message per line) in the library crate (`src/mcp.rs`), for the
project at its working directory.
- **It changes nothing on its own:** it never uploads, lands, refreshes, retries or splits a task.
- **Stdout is the protocol only.** Every path it calls runs `Silent`, and a test fuzzes every tool and
  fails on any other stdout line.

### Tools

| Tool | What it does | Built from |
|---|---|---|
| `work` | All Work in the project, one row each: id, harness and origin (session, subagent, native, attached), state, verdict and reason, needs-you (kind, message, since), asked to catch up, interactions, files changed, ready for review | one `work_rows` lifted from `presenter/watch.rs:920-963`, which watch then uses too (one path) |
| `status` | One Work in full | `status_json` (`orchestrator.rs:1918`) |
| `wait` | Returns when any named Work (or any) finishes, needs input, or changes verdict, or at `timeout_s` (default 300, max 900). Sends MCP progress when given a `progressToken` | stat of runs' `metadata.json` and `events.jsonl` and of `marks/`; no SQLite in the loop, idle CPU about 0 |
| `start` | Launches a native run of a named agent (Claude Code, Codex, Cursor, Hermes) on a task, detached, and returns its id at once | `run_dispatch` through `background::command`; the id from `run.created` |
| `send` | Relays the orchestrator's message, verbatim, to a live hooked session or subagent | a new note kind `message` through `inbox::write` |
| `propose_landing` | The batch proposal as data: members in order with why, those left out with reasons, the digest and the exact `dispatch accept --expect <digest>` for the person | a returning variant of `batch::offer` |

There is no landing tool and no refresh tool. A test asserts that no tool applies, accepts, rejects,
closes or refreshes.

### Rules

- **Launch consent.** `dispatch setup` (at a terminal only) offers a standing launch consent per
  project: the agents allowed and `max_running` (default 3), stored beside the native consent
  (`projects/launch-<sha>.json`).
  - `start` refuses an agent that isn't allowed, or one above the cap.
  - Every launch is recorded as `launch.requested {by: "mcp", client}`.
  - No retries. A failed launch is reported, never relaunched.
- **Messages.**
  - **Format:** delivered verbatim with one label line, `Message from the orchestrating session
    (<client>):`.
  - **Delivery:** at the session's next `PostToolUse`, and to an idle Claude Code session by its waiter,
    as an ask is. Hermes gets it at its next turn.
  - **Recorded:** `message.written` and `message.delivered`.
  - **Refused** for Codex, Cursor and native runs, with "no hook carries messages to <harness>".
  - Dispatch's own catch-up asks stay fixed text, once per landing.
- **Registration.** With consent, `dispatch setup` registers the server in each detected harness's MCP
  config. It is a thin adapter per harness, owning only the config location and format, and setup's
  uninstall removes it.
  - Claude Code: `claude mcp add --scope user`.
  - Codex: `[mcp_servers.dispatch]` in `~/.codex/config.toml`.
  - Cursor: `~/.cursor/mcp.json`.
  - Hermes: `hermes mcp add`.
- **AGENTS.md** (in R2):
  - The connector is allowed as above. It amends "premature APIs" and "speaks to live agents only …
    with fixed text": Dispatch's own words stay fixed, and an orchestrator's messages are relayed
    verbatim and labeled.
  - Landing, refresh, retries and task splitting stay out.

### Packets

- **M0, probe (integrator):** MCP tool-call limits and progress handling in Codex 0.155.1, Cursor Agent and
  Hermes 0.18.2, with the probe server in `~/dispatch-dev/p0-subagents/mcp/`. The `wait` default comes from
  the shortest limit.
- **M1, the server and its reads (Claude Code worker; lands first):** the loop, `work_rows` shared with
  watch, and `work`, `status`, `wait` and `propose_landing`, plus the stdout and no-landing tests.
- **M2, launch (Claude Code worker, after M1):** launch consent in setup, then `start` with its cap and
  events.
- **M3, messages (Claude Code worker, after M1, in parallel with M2):** `send`, the `message` note kind and
  waiter delivery.
- **M4, registration in each harness (Claude Code worker):** setup's install, upgrade and uninstall round
  trips for all four.
- **R2:** docs (`reference.md`, a new "Orchestrating agents" guide, `attach.md`), AGENTS.md, version 0.6.0,
  release notes with `Site impact:`.
- **C2, conformance:**
  - an orchestrator in each harness that has MCP drives two workers through `dispatch mcp`, from task to
    proposal;
  - Claude Code also through worktree subagents.

### Exit criteria (0.6.0)

1. **Proven on itself.** 0.6.0's second wave is orchestrated through the candidate's `dispatch mcp` by
   one orchestrator Claude Code session, with no herdr keystrokes to workers.
   - Hand steps per landed Work, against 0.5.0's 5.5.
   - Hand-orchestration failures, against 8.
   - Screen polls: 0.
2. **`wait` is prompt and cheap.** It returns within 2 s of a finish or a needs-input, and an idle `wait`
   uses no measurable CPU.
3. **Launch rules hold.** The cap and the allowed agents are enforced, consent is given only at a
   terminal, and every launch is recorded.
4. **Messages are delivered once and verbatim,** and refused for harnesses without hooks.
5. **No tool can land.** Tested.
6. **Coordination still holds.** The coordination cells of 0.5.0 still pass, and the MCP cells pass for
   each harness with MCP.
7. **Size and CI.** Production LOC reported (estimate +900 to +1,300). CI green.

## Not built

- Task splitting. Landing, refresh or retry by an agent.
- Messages to Codex, Cursor or native runs mid-run (no hook carries them).
- Tracking Hermes's `delegate_task` children separately: they share the parent's directory.
- Waking subagents: they run or are done.
- Claude Code `--bg` sessions as `start`'s launcher. Native runs are the one path for all four harnesses.
  The question can be revisited with evidence.
- A daemon, a socket or a network listener. The connector is a stdio child of the harness.

## Order, workers, landing

As in 0.5.0:
- Claude Code workers (`claude --worktree <name> --model claude-opus-5-5`, hook-registered) are launched
  from a clone.
- Each landing is `dispatch accept --expect`, after the integrator's line-by-line review, CI and 10
  stability runs on its branch, and the user's approval.
- CI is green after every landing.

**0.5.1:**
- Supervisor: the pinned 0.5.0 (`~/dispatch-dev/dispatch-0.5.0/`). State `state-0510`.
- Clone `integration-0510` on `release-0.5.1` from `v0.5.0`, with this plan as `docs/plan-0.5.1-0.6.0.md`.
- Order: P0b, then A, then R, then C on the release build. Draft PR, merge by the user, tag on approval.

**0.6.0:**
- Supervisor: the 0.5.1 release. Clone `integration-0600`.
- Wave 1: M0, then M1 under the hand workflow, then M2, M3 and M4.
- Switch: a candidate release build of the head after M1 to M3 land.
- Wave 2 (the remaining fixes, R2): run by one orchestrator session through the candidate's `dispatch
  mcp`, with my own steps counted.
- C2 last, on the release build.

## Verification

- **Each packet:** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, then
  finish and accept. CI (Linux, macOS, stress) and stability (10 runs) on its branch.
- **0.5.1 end to end,** on a release build in a temp HOME with stand-in subagent payloads (as
  `rel-0418/e2e.py`): three subagents under one parent, then notes, ends, finish by consent, and a batch
  proposal; then a real trial (C).
- **0.6.0 end to end:**
  - a scripted MCP client (JSON lines over a pipe) drives `work`, `wait`, `start`, `send` and
    `propose_landing` against stand-in Work;
  - stdout purity under fuzzing;
  - a real orchestrator session in Claude Code (C2), then wave 2 measured.
- **Hook timing:** `SubagentStart` and `SubagentStop` p95 over 1,000 calls. **`wait`:** latency and idle
  CPU.
