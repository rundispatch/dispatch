# Harness conformance: every agent checked the same way

Dispatch's promise should not depend on which agent wrote the change. This page
records whether it holds for the agents people use: each harness goes through
each path Dispatch offers it, on one shared scenario, and the outcome is judged
against Dispatch's own contract. The record never compares, rates or ranks the
agents or their code.

## The scenario

A small Python project (`scripts/harness-trial/fixture/`): `auth.py` with
`def validate(token)`, `admin.py` with `def promote(token)`, `audit.py`,
`api.py`, `README.md` and `test_app.py`. The trial adds a `dispatch.yml` whose
check is `python3 -B -m unittest -q`. A real agent gets three tasks, one at a time
(`scripts/harness-trial/tasks/`):

- **B** changes `validate` to `validate(ctx, token)`, returning the tenant, and
  updates the tests.
- **A** makes `promote(token)` call `validate(token)` and `audit.record(user)`.
- **C** adds an Auth section to `README.md`, naming no file.

Each task says "change no other file". The expected behavior:

- Before anything lands, Dispatch reports that A and B interact: A uses
  `validate`, whose signature B changes.
- Accepting B applies it to the source.
- `dispatch check` on A is REFRESH with
  `fact_broken: def validate(token): => def validate(ctx, token):`, the right side
  being B's new declaration.
- `dispatch check` on C is CONTINUE.

## The paths

| Path | How the agent runs | How its Work ends |
|---|---|---|
| `attach` | `dispatch attach -- <agent's non-interactive CLI>` from the project: Dispatch makes the workspace | the wrapped attach freezes it when the agent exits |
| `native` | `dispatch run --agent <harness>`; "not offered" when the Dispatch binary has no adapter for the harness (its `--agent` refuses the name) | the run |
| `worktree` | the harness's own worktree mode, in the project, while `dispatch start` watches | once Dispatch shows the session ended, `dispatch finish` |

`dispatch start` watches the project on every path, because interactions are
reported only while a project is watched.

## Reading a cell

- **pass**: every check below held.
- **fail**: Dispatch did not do what its contract says. The cell names the step,
  what was expected and what was observed; the record lists every such defect.
- **not offered**: Dispatch has no such path for this harness.
- **skipped**: the harness could not do the work, so the trial says nothing
  about Dispatch. The cell quotes the harness's own words: no CLI, no
  credentials, a usage limit, a provider error, a timeout, or a change that does
  not fit the task (it touched other files, or C named one of B's files, which
  rightly makes C depend on B).

Checked for each task:

- Work was created.
- S0 is the fixture's commit, with the provenance Dispatch recorded.
- Δ is the agent's change: S0 with Dispatch's patch applied has the same tree
  hash as the agent's workspace read directly with Git.
- The session's start and end are recorded.
- Usage is what Dispatch recorded from the harness's report, or `unknown`.

Then the interaction, the accept of B, and the verdicts on A and C, as above.
The project's check is also run on each agent's tree, independently of
Dispatch, so a failing test is the agent's, not a Dispatch defect.

Hermes removes its worktree when the session ends, before the runner can read
it. On the `hermes worktree` path the runner keeps its own copy of the agent's
tree while the agent runs, taken about every half second while Hermes's lock
stands; a sample counts only if the lock still stood after it was taken. The
record says where each task's tree came from (`agent.tree_source`): `workspace`,
or `last seen by the runner, <time>`, and the table notes the second case. That
copy can miss an edit made in the last moments before Hermes unlocks, so a
difference between S0 + Δ and it is recorded as observed, with its source
named. When the runner saw no tree at all, the task reads "the runner never
saw the agent's tree", not "changed nothing". The trial installs no Hermes hook, so this path
exercises Dispatch's last-seen Δ, not the hook's exact capture.

## Running it

```bash
DISPATCH_BIN=target/release/dispatch TRIAL_OUT=/tmp/harness-trials \
  scripts/harness-trial.sh claude worktree
```

- **Arguments:** `<harness>` is `claude`, `codex`, `cursor` or `hermes`;
  `<path>` is `attach`, `native` or `worktree`.
- **Environment:**
  - `DISPATCH_BIN`: the Dispatch binary, `dispatch` on `PATH` by default.
  - `TRIAL_OUT`: the output root, `./harness-trials` by default. Keep it
    outside any other project: agents read instruction files in parent
    directories.
  - `TRIAL_TIMEOUT_SECS`: the limit for each agent invocation, 600 by default.
    It is also the fixture's `execution.timeout_secs` for native runs.
  - `TRIAL_MODEL_CLAUDE`, `TRIAL_MODEL_CODEX`, `TRIAL_MODEL_CURSOR`,
    `TRIAL_MODEL_HERMES`: an optional model, passed to the CLI, or put in the
    fixture's `dispatch.yml` for native runs.
- **Credentials** come from the environment and are never written to the record:
  `ANTHROPIC_API_KEY` or `CLAUDE_CODE_OAUTH_TOKEN` for Claude Code;
  `OPENAI_API_KEY` for Codex, fed to `codex login --with-api-key` in a fresh
  `CODEX_HOME`; `CURSOR_API_KEY` for Cursor; the provider key Hermes's config
  names. On a developer machine an existing login counts.
- **Output:** one summary line, and `$TRIAL_OUT/<harness>-<path>/` with:
  - `record.json`;
  - `steps/`: each command's line, stdout, stderr and exit code;
  - `tasks/<b|a|c>/`: the agent's stdout and stderr (its stream-JSON
    transcript, where it has one), exit status, workspace, trees, patch and
    check, and for `hermes worktree` the runner's `last-seen/` copies and
    `agent-tree-source.txt`;
  - `state/`: the trial's own `DISPATCH_HOME`, with each run's events and
    metadata;
  - the fixture repository.
- **Exit status:** 0 pass, 1 fail, 2 not offered, 3 skipped.

Each trial starts from a fresh copy of the fixture with a unique directory name
(Cursor names its worktree directory after the project's) and its own
`DISPATCH_HOME`. It never touches `~/.dispatch`. Claude Code runs without user
settings (`--setting-sources ""`, hooks disabled), so hooks installed for
another Dispatch state never fire. At the end the trial stops watching and
removes every worktree it made, its harness's included (`~/.codex/worktrees`,
`~/.cursor/worktrees`), through the fixture repository's `git worktree remove`.

The harnesses' own session stores (`~/.claude/projects`, `~/.codex/sessions`)
are not copied: finding a session there means walking another tool's home
directory.

A record can be rebuilt from its evidence with
`python3 scripts/harness-trial/trial.py record <trial dir>`. The table below
is written from records only:

```bash
python3 scripts/harness-trial/trial.py table --write docs/harness-conformance.md "$TRIAL_OUT"
```

## Results

<!-- conformance-table:begin -->
Evidence on the machine that ran the trials.

| Harness | Path | Result | Dispatch | Harness | Date | Evidence |
|---|---|---|---|---|---|---|
| claude | attach | pass | dispatch 0.4.14 | 2.1.296 (Claude Code) | 2026-10-15 | `~/dispatch-dev/conformance-0414/trials/claude-attach` |
| claude | native | pass | dispatch 0.4.14 | 2.1.296 (Claude Code) | 2026-10-15 | `~/dispatch-dev/conformance-0414/trials/claude-native` |
| claude | worktree | pass | dispatch 0.4.14 | 2.1.296 (Claude Code) | 2026-10-15 | `~/dispatch-dev/conformance-0414/trials/claude-worktree` |
| codex | attach | pass | dispatch 0.4.14 | codex-cli 0.155.1, model gpt-6-sol | 2026-10-15 | `~/dispatch-dev/conformance-0414/trials/codex-attach` |
| codex | native | pass | dispatch 0.4.14 | codex-cli 0.155.1, model gpt-6-sol | 2026-10-15 | `~/dispatch-dev/conformance-0414/trials/codex-native` |
| codex | worktree | pass | dispatch 0.4.14 | codex-cli 0.155.1, model gpt-6-sol | 2026-10-15 | `~/dispatch-dev/conformance-0414/trials/codex-worktree` |
| cursor | attach | pass | dispatch 0.4.14 | 2026.10.14-0f9edde | 2026-10-15 | `~/dispatch-dev/conformance-0414/trials/cursor-attach` |
| cursor | native | pass | dispatch 0.4.14 | 2026.10.14-0f9edde | 2026-10-15 | `~/dispatch-dev/conformance-0414/trials/cursor-native` |
| cursor | worktree | pass | dispatch 0.4.14 | 2026.10.14-0f9edde | 2026-10-15 | `~/dispatch-dev/conformance-0414/trials/cursor-worktree` |
| hermes | attach | pass | dispatch 0.4.14 | Hermes Agent v0.18.2 (2026.7.7.2) · upstream df5700eb | 2026-10-15 | `~/dispatch-dev/conformance-0414/trials/hermes-attach` |
| hermes | native | pass | dispatch 0.4.14 | Hermes Agent v0.18.2 (2026.7.7.2) · upstream df5700eb | 2026-10-15 | `~/dispatch-dev/conformance-0414/trials/hermes-native` |
| hermes | worktree | pass (agent's tree: B last seen by the runner, 2026-10-16T01:20:50Z; A last seen by the runner, 2026-10-16T01:22:05Z; C last seen by the runner, 2026-10-16T01:22:31Z) | dispatch 0.4.15 | Hermes Agent v0.18.2 (2026.7.7.2) · upstream df5700eb | 2026-10-16 | `~/dispatch-dev/conformance-0415/trials/hermes-worktree` |
<!-- conformance-table:end -->

## Coordination cells (0.5.0)

0.5.0 adds four cells for Work that coordinates: notes, catch-up, S0 following
the agent's merge, and batch landing. They come from the same fixture and tasks,
with three live sessions at once, one per task, each in the harness's own
worktree mode while `dispatch start` watches the project. These trials were run
by hand, not by `scripts/harness-trial.sh`, which runs one task at a time and
has no such cells. The table is written by hand from each trial's `record.md`;
`trial.py table` does not write it.

- **Setup.** A fresh copy of the fixture with its own state directory. For
  Claude Code, the hooks are in the project's `.claude/settings.local.json`,
  pointing at the release build and that state, and Claude Code runs with
  `--setting-sources local`. For Hermes, the person's `~/.hermes/config.yaml`
  held the release build's three hooks for that state during the trial, with the
  person's approval, and was restored byte for byte after it.
- **Steps.**
  1. All three sessions get their task. While A and B are live, each should hear
     through its hooks that they interact.
  2. B and C end their sessions and are finished. `dispatch accept` with no ID
     proposes B and C, and `accept --expect <batch digest>` lands both.
  3. A, live and idle, should be asked once to merge B's landing. Codex and
     Cursor Agent have no hook to carry an ask, so the trial tells A to merge,
     as the person would.
  4. A commits and merges the recorded commit. Its S0 should move, and the next
     verdict be taken against the new S0.
  5. A ends, is finished and accepted.

| Cell | Pass when |
|---|---|
| notes | Claude Code, Hermes: a note about A and B delivered through a hook. Codex, Cursor Agent: "none, no hook" |
| catch-up | Claude Code, Hermes: one `catchup.written` for A naming B's landing, one `catchup.delivered` with its `via`, and no second ask. Codex, Cursor Agent: "none, no hook" |
| S0 follows the merge | `attach.s0_moved` for A after its merge, then a verdict on the new S0 |
| batch landing | one proposal of B and C, one `--expect`, one merged-check run, one `refs/dispatch/source` commit |

"none, no hook" is not a fail: Dispatch offers that harness no hook, and its
contract asks nothing there. The four trials below ran on the release build of
`96fd98c` (`dispatch 0.5.0`), macOS arm64, on 2026-11-15.

| Harness | Notes | Catch-up | S0 follows the merge | Batch landing | Harness version | Evidence |
|---|---|---|---|---|---|---|
| claude | pass (`PostToolUse`) | pass (A idle, woken 0.7 s after the ask was written) | pass | pass | 2.1.296 (Claude Code) | `~/dispatch-dev/conformance-0500/claude-2` |
| codex | none, no hook | none, no hook | pass | pass | codex-cli 0.155.1, model gpt-6-sol | `~/dispatch-dev/conformance-0500/codex-2` |
| cursor | none, no hook | none, no hook | pass | pass | 2026.11.13-eacade1 | `~/dispatch-dev/conformance-0500/cursor-2` |
| hermes | pass (`pre_llm_call`) | pass (A idle, the ask with its next turn, `via: turn`; `pre_verify` not exercised) | pass | pass | Hermes Agent v0.18.2 (2026.7.7.2) | `~/dispatch-dev/conformance-0500/hermes-2` |

**The first run** was on the release build of `e8067c3`, on 2026-11-13
(`~/dispatch-dev/conformance-0500/{claude,codex,cursor,hermes}`). Every cell
passed or read "none, no hook" there too, except one:

- **Codex, S0 follows the merge: fail.** A committed, merged B's landing and
  fixed its call, and no `attach.s0_moved` was recorded. A's finish was REFRESH,
  and its Δ held B's and C's landed changes. Every Codex worktree is detached,
  and S0 took no proof from a detached HEAD. Since packet Y, Dispatch records
  whether a workspace's HEAD was detached when its Work began, and in one that
  was, a detached HEAD moves S0 as a branch's does ([attach.md](attach.md),
  "When S0 moves"). On the rerun, S0 moved 25 s after A was told to merge, and
  A's Δ held only its own 2 files.
- **Claude Code, an observation, not a cell.** The ask named B by 8
  characters, which all three Works of the trial shared. Since packet Y, an ask
  names each Work by as much of its ID as tells it apart from every Work of the
  project. The rerun's ask named B by 11.

## Subagent cells (0.5.1)

0.5.1 makes a Claude Code subagent started with `isolation: worktree` Work of its
own ([attach.md](attach.md), "Subagents"), so these cells are Claude Code's
only. Hermes's `delegate_task` children work in their parent's own directory and
are part of the parent's Work (observed with the probe, 2026-11-18). The trial
uses the same fixture and tasks: one orchestrating Claude Code session in the
checkout starts B, A and C at once as worktree subagents with the Agent tool,
while `dispatch start` watches the project, on the release build. It is run by
hand, not by `scripts/harness-trial.sh`.

- **Setup.** As for the coordination cells: a fresh copy of the fixture with its
  own state directory, the release build's ten Claude Code hooks in the project's
  `.claude/settings.local.json` and no `worktree.baseRef`, and Claude Code run
  with `--setting-sources local`. Folder trust is accepted first in a session with
  hooks disabled (`claude --setting-sources '' --settings
  '{"disableAllHooks":true}'`), which makes no state. The person gives the
  project's check consent at a terminal, and `dispatch start` watches the project.
- **The prompt** is a file, `.claude/orchestrator-prompt.txt`, which the
  orchestrator is told to read: a long line typed into the terminal can arrive
  cut short. The file is one line: this opening, then `Task A:`, `Task B:` and
  `Task C:`, each followed by its task from `scripts/harness-trial/tasks/`, word
  for word.

  ```text
  Start three subagents at once with the Agent tool, each with isolation: "worktree" and subagent_type "general-purpose". Give each one its task below, word for word. Do not edit any file yourself, and do not merge their branches. When all three have finished, tell me each one's result in two lines.
  ```

  Task A goes on after its task, so that A still runs when B lands, and knows in
  advance what to do with an ask:

  ```text
  When your edit is done and python3 -B -m unittest -q passes, run sleep 100 three times, one after the other, before you finish: this trial needs you running a while. If a Dispatch hook asks you to catch up with a landing, do what it says: commit, merge the commit it names, then change promote to take (ctx, token) and pass ctx to validate, update test_app.py to match, and run the tests.
  ```

- **Steps.**
  1. The orchestrator (`claude --setting-sources local --permission-mode
     acceptEdits`, in the checkout) reads the file and starts A, B and C. Each
     should be Work before its first edit, and no subagent worktree "Found, not
     tracked".
  2. While A and B run, a subagent should hear through a hook that they interact.
  3. B and C end. The owner should finish each by consent once its session ends.
     `dispatch accept` with no ID proposes both, and `accept --expect <batch
     digest>` lands them.
  4. A's sleeps run as background shells, and Claude Code resumes A after each
     one. Still running when B lands, A should be asked once, at its next
     `SubagentStop`, to merge B's landing, and its S0 should follow the merge.
  5. Once the orchestrator reports all three done, it exits (`/exit`). A's
     session ends then (finding 1, below), and the owner should finish it by
     consent. `dispatch accept <id> --expect <Δ digest>` lands it on its own.

| Cell | Pass when |
|---|---|
| Work at spawn | one Work per subagent, registered by its `SubagentStart` or by discovery before its first edit, with S0 the tree its worktree was made from; 0 subagent worktrees "Found, not tracked" |
| notes | a note about A and B delivered to a subagent through a hook (`PostToolUse` or `SubagentStart`) |
| catch-up | one `catchup.written` for A naming B's landing, one `catchup.delivered {via: subagent_stop}`, and no second ask |
| finish by consent | each subagent's Work finished by the owner (`finish_reason: by_consent`) once its session ended, with no `dispatch finish` typed |
| batch landing | one proposal of B and C, one `--expect`, one merged-check run, one `refs/dispatch/source` commit |

The trial below ran on a release build of `f35e1d6` (`dispatch 0.5.1`), macOS
arm64, on 2026-11-20, before packet N's fix. Under `src/`, the release differs
from that build only by N, which fixed finding 2, the late note, after the trial.

| Harness | Work at spawn | Notes | Catch-up | Finish by consent | Batch landing | Harness version | Evidence |
|---|---|---|---|---|---|---|---|
| claude | pass (3 Works, 0 "Found, not tracked"; B's made by discovery from its lock 0.46 s before its `SubagentStart`, which joined it) | pass (`PostToolUse`, to A) | pass (A running, the ask at its next `SubagentStop`, 68 s after it was written) | pass (B and C when their locks went, A when the parent exited) | pass (B and C; A later on its own) | 2.1.296 (Claude Code) | `~/dispatch-dev/conformance-0510/claude` |

The trial also checked the parent's end and that no Work was lost. Both passed:
the parent's `SessionEnd` ended no subagent Work, A's session ended by liveness
(`process_gone`) when the parent exited, no waiter ran for the trial's state, and
all three Works finished and landed. Its findings, from its `record.md`:

1. **A's session stayed open until the parent exited,** 18 minutes after A's
   last stop and through one parent turn, because A kept its lock: Claude Code
   had resumed A after each background shell. B and C, never resumed, lost their
   locks 6 to 14 s after their stops. A's finish by consent waited for the
   parent. Nothing was lost: Δ was kept at each stop. This is how Claude Code
   2.1.296 behaves, and a known limit ([attach.md](attach.md), "Known limits").
2. **A late note.** B's landing item waited for A's ten-minute note window and
   was written to A 8 minutes after A's S0 already held the landing. It was never
   delivered, since A was done. Packet N fixed it after the trial: a landing makes
   a note item only while it is newer than the Work's S0 ([attach.md](attach.md),
   "Notes to live agents").
3. **Discovery won the race with `SubagentStart`** for B, by 0.46 s. There was
   one Work, as designed, and its task has no agent type.
4. **The ask waited 68 s** for A's next `SubagentStop`, which came only when its
   background shell ended. A subagent that has stopped is not woken.

Stand-in subagents, sending the probe's recorded payloads, go through each of
these steps in `tests/subagents.rs`. That tests Dispatch, not a harness, and is
not a conformance result.
