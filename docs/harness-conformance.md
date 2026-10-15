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
    check;
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
Evidence under `~/dispatch-dev/conformance-0414/trials/`, on the machine that ran the trials.

| Harness | Path | Result | Dispatch | Harness | Date | Evidence |
|---|---|---|---|---|---|---|
| claude | attach | pass | dispatch 0.4.14 | 2.1.296 (Claude Code) | 2026-10-15 | `claude-attach/` |
| claude | native | pass | dispatch 0.4.14 | 2.1.296 (Claude Code) | 2026-10-15 | `claude-native/` |
| claude | worktree | pass | dispatch 0.4.14 | 2.1.296 (Claude Code) | 2026-10-15 | `claude-worktree/` |
| codex | attach | pass | dispatch 0.4.14 | codex-cli 0.155.1, model gpt-6-sol | 2026-10-15 | `codex-attach/` |
| codex | native | pass | dispatch 0.4.14 | codex-cli 0.155.1, model gpt-6-sol | 2026-10-15 | `codex-native/` |
| codex | worktree | pass | dispatch 0.4.14 | codex-cli 0.155.1, model gpt-6-sol | 2026-10-15 | `codex-worktree/` |
| cursor | attach | pass | dispatch 0.4.14 | 2026.10.14-0f9edde | 2026-10-15 | `cursor-attach/` |
| cursor | native | pass | dispatch 0.4.14 | 2026.10.14-0f9edde | 2026-10-15 | `cursor-native/` |
| cursor | worktree | pass | dispatch 0.4.14 | 2026.10.14-0f9edde | 2026-10-15 | `cursor-worktree/` |
| hermes | attach | pass | dispatch 0.4.14 | Hermes Agent v0.18.2 (2026.7.7.2) · upstream df5700eb | 2026-10-15 | `hermes-attach/` |
| hermes | native | pass | dispatch 0.4.14 | Hermes Agent v0.18.2 (2026.7.7.2) · upstream df5700eb | 2026-10-15 | `hermes-native/` |
| hermes | worktree | fail: B: Work: expected Work created; observed no Work within the wait; the harness's worktrees: none | dispatch 0.4.14 | Hermes Agent v0.18.2 (2026.7.7.2) · upstream df5700eb | 2026-10-15 | `hermes-worktree/` |
<!-- conformance-table:end -->
