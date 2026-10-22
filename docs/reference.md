# Dispatch reference

Commands, configuration and current boundaries, versioned with the code. The
[README](../README.md) says what Dispatch is and how to start, and
[rundispatch.sh/docs](https://rundispatch.sh/docs) has the guides.

## Core commands

```text
dispatch setup [codex|claude | --checks [--verify command]...]
dispatch resources
dispatch run "<task>" [--source path] [--agent claude|codex|cursor|hermes]
dispatch run "<task>" --json|--jsonl
dispatch status [run-id] [--json|--jsonl]
dispatch diff [run-id]
dispatch accept [run-id] [--expect <Δ digest>]
dispatch reject [run-id]
dispatch close <run-id> --reason unfinished|workspace-removed|already-present|superseded [--replacement run-id]
dispatch explain [run-id]
dispatch check [run-id] [--json] [--merged [--allow-unsafe-local]]
dispatch refresh [run-id] [--allow-unsafe-local] [--json|--jsonl]
dispatch answer <run-id> <question-id> --revision n --answer text [--json]
dispatch cancel <run-id> <question-id> --revision n [--json]
dispatch attach [--workspace path] [--auto-apply] [-- command...]
dispatch finish <run-id> [--allow-unsafe-local]
dispatch start [--root path]
dispatch watch [--root path] [--json]
dispatch stop [--root path]
dispatch clean [--dry-run] [--yes]
dispatch serve [--root path] [--json]
dispatch history [--limit count]
dispatch version
```

Without a run ID, `status`, `diff`, `explain`, `check`, and `refresh` resolve the latest relevant single-result run for the current source tree; `status`, `diff`, `explain` and `check` print which run they describe. `accept` and `reject` without an ID act only when exactly one result waits for review for the source; with none or several they change nothing and list the explicit commands. `close` and `finish` always need an ID. An explicit ID may be a unique, case-insensitive prefix; an ambiguous one is refused. `--task-file path|-` reads a task from a file or stdin, and `--source path` overrides the current directory.

`run --json` writes one versioned result object to stdout. `run --jsonl` writes each committed, sequenced event followed by a final result object; `status --jsonl` replays that committed journal. Machine modes require non-interactive authorization flags when local execution needs acknowledgement, keeping stdout parseable.

Advanced automation can follow committed events without controlling execution:

```bash
dispatch events <run-id> --after 0 --until attention --timeout 30
dispatch events <run-id> --after <cursor> --until finished --timeout 30
```

Output is JSON Lines with a final committed cursor. `attention` includes human
waiting or a finished outcome; `finished` waits for the core lifecycle to finish
(including a result awaiting review). Timeout exits 124 without answering,
launching, or repairing anything. Omit `--until` to replay through the current
journal. Events now include their committed `payload.outcome`, so a transient
question cannot disappear merely because it was answered before the next poll.
A cursor ahead of the journal is rejected. Historical events written before
Phase 4 remain readable, but cannot reconstruct transient states they did not
record; current state is still available.

One-shot exit codes are:

- `0`: a result is ready for review and verification passed or was not configured;
- `3`: a result is ready for review, but configured verification failed;
- `4`: the goal is waiting on a human (a durable clarification);
- `124`: the goal-wide deadline expired;
- `1`: execution or orchestration failed (including work stopped by the mid-run coherence watcher);
- `2`: command-line usage error.

The result keeps execution, verification, review, and application as separate fields. Attempt records also keep requested, resolved, and harness-observed model/effort values separate; an unknown or mismatched observed identity is not replaced by configuration.

### Resource profiles

`dispatch setup codex` or `dispatch setup claude` writes a profile to the
user-level `$DISPATCH_HOME/resources.yml` (normally `~/.dispatch/resources.yml`);
project configuration cannot enable one. A profile names the exact model, effort
and included funding you confirmed, and the account it was confirmed for:

```yaml
version: 1
allocation_enabled: true
profiles:
  - provider: openai
    funding_source: chatgpt-plus
    harness: codex
    model: your-included-model-id
    effort: low
    service_mode: standard
    runtime: local
    pool: chatgpt-codex
    tier: standard        # written for older versions; nothing selects on it
    included: true
    no_overage_verified: true
    authorization_revision: 1
    codex_account: {account_sha256: "…", checked_at: "…"}   # recorded by setup
```

`no_overage_verified` is an explicit assertion that the account cannot fall
through to paid overage; an included model name or visible credits are not
enough. Profiles without it are shown in `dispatch explain` but are ineligible.

Immediately before each launch, the adapter checks the funding identity it can
observe. Codex: authentication must be ChatGPT, no paid credits available, the
standard service tier, the configured plan, and the account setup recorded; an
identity that cannot be observed is refused. Claude: the subscription evidence
must be current, and the executable, CLI version, account and settings must
match it. A refusal names the reason and is sticky: that
`authorization_revision` stays refused, even if the account switches back,
until `dispatch setup` re-authorizes the profile with a new revision.

Claude assertions expire after at most 24 hours; revalidate the profile with
`dispatch setup`. A bounded live run passed with Claude Code 2.1.274, personal
Pro, `claude-sonnet-5` at medium effort and usage credits disabled; this is
specific to that account state and configuration. See
[provider support](provider-support.md).

### Clarification

A goal is one agent invocation. Codex may ask one essential clarification
question with this exact JSON envelope in its final `agent_message` event,
followed by a successful process exit:

```json
{"dispatch_checkpoint":{"version":1,"question":"Which behavior is required?","choices":["A","B"]}}
```

Dispatch validates the report after the agent's process has been cleaned up,
then persists a question with `lifecycle=waiting` and `waiting_on=human`. It
never pauses a live model process, and it never asks while an agent it launched
may still be running. Malformed, non-final or unsuccessful reports do not create
a question. Answer or cancel the specific question with its current revision
(available in `dispatch status <run-id> --json`):

```bash
dispatch answer <run-id> <question-id> --revision 1 --answer "A" --json
dispatch cancel <run-id> <question-id> --revision 1 --json
```

Answers are authorized by the local run owner's OS identity and accepted once.
The answer command runs one continuation from the original baseline with the
answer; a second question cannot start a third invocation. `--timeout` is one
goal-wide deadline covering baseline checks, the invocations, verification and
human waiting. There is no automatic retry, crash replay or separate `resume`
command, and the background watcher (`dispatch start`) never answers a question
or continues a goal. Pending questions and all attempt evidence survive process
exit and reload, and every launch is recorded durably, so a run is never closed
while an agent it launched may still be alive.

## How Dispatch chooses an agent

Each run uses exactly one agent:

- With profiles in `resources.yml` (written by `dispatch setup`), Dispatch uses
  the first eligible profile in file order. `--agent`, `--model` and `--effort`
  only narrow the choice. If no eligible profile matches, the run is refused
  with each profile's reason; it never falls back to an unchecked agent.
- With no profiles configured (no `resources.yml`, or `allocation_enabled:
  false`), `--agent claude|codex|cursor|hermes` runs that agent with its own
  login and no funding contract is checked.
- With neither, `dispatch run` refuses: run `dispatch setup`, or pass `--agent`.

There is no ranking, benchmark data or automatic fallback to another agent.

## Interactive loop

From a source directory, open Dispatch. Missing resources lead to guided setup
and preserve your goal. You can also configure resources first:

```bash
dispatch setup                 # checks, consent, hooks and watching; then "Agents Dispatch launches…" for provider login and funding consent
dispatch setup --checks        # approve a known project check
dispatch setup --checks --verify 'cargo test'   # or save an exact one, no terminal needed; grants nothing
dispatch                      # direct goal → work → verification → review
```

Enter the outcome, then approve local execution for that goal. Dispatch displays
the chosen resource, work and verification. A durable clarification
appears directly in the session; submitting its answer continues automatically.
At review, enter `d` for the diff, `a` to accept and safely apply, `r` to reject,
`i` for artifact details, or `n` to leave the result pending and start another goal.
These actions retain the displayed run, question, and candidate identities.
Verification and human acceptance remain separate.

The compact inline view preserves scrollback and uses the terminal's background.
Enter submits, Alt+Enter inserts a newline, bracketed paste inserts without
submitting, Ctrl+C cancels, and Ctrl+D exits an empty editor. During work,
Ctrl+C waits for process cleanup; EOF/hangup also requests controlled cancellation.
Input entered while work is active is discarded rather than queued as another goal.

Use `dispatch --plain` for ordinary line input, `--ascii` for ASCII graph marks,
and `--no-color` (or `NO_COLOR`) for native colors. `TERM=dumb` selects plain mode.
Plain input is line-oriented; the integrated editor supports multiline paste.
Bare invocation without terminal input and output prints help and exits 2.

Use `/resources` for Accounts / Resources and `/checks` for project checks.
`dispatch resources` prints cached configuration status without a prompt or model
call. Discovery never authorizes spending. Codex and Claude use their own supported
CLI login; no Dispatch account is required. Confirm the exact model, effort and
included funding. Claude assertions expire after at most 24 hours and have an
explicit refresh path. Changed/rejected funding epochs need fresh owner consent.
No paid fallback, credits or account switch is automatic.
See [setup and review](product-guide.md), [visual system](design-system.md)
and [candidate validation](product-rc-validation.md).

## One-shot CLI

From a repository or plain directory:

```bash
cd my-project
dispatch run "Fix the retry race"
```

Local agents run with your operating-system permissions. On a terminal Dispatch asks first: `[1] This run only  [2] Allow for this project  [Enter] Cancel`. Allowing the project covers later runs and refreshes with exactly the same agent command and checks; any change asks again. A piped `y` allows one run; scripts can also use `--allow-unsafe-local` after accepting that risk.

Review and apply the result. Without an ID, `diff` shows the latest result and
names it, and `accept` acts only when exactly one result waits for review for this
source; with several it changes nothing and lists them with their IDs:

```bash
dispatch diff
dispatch accept
```

Or reject it without changing the source tree:

```bash
dispatch reject
```

The summary at the end of the run prints these commands with the run's ID.

To run a specific agent:

```bash
dispatch run "Fix the retry race" --agent claude
```

`dispatch explain` shows which agent and resource the last task used.

## Configuration and verification

Configuration is optional. Dispatch discovers `dispatch.yml`, `dispatch.yaml`, `.dispatch.yml`, or `.dispatch.yaml` at the source root. `dispatch init` can create a starting file for advanced setup.

```yaml
execution:
  backend: local
  timeout_secs: 1800
  forwarded_env: []

checks:
  baseline: []
  verify:
    - cargo test

harnesses:
  claude:
    model: null
    extra_args: []
  codex:
    model: null
    extra_args: []
  cursor:
    model: null
    extra_args: []
  hermes:
    model: null
    provider: null
    extra_args: []
```

Use the command that actually verifies the project, such as `pytest`, `npm test`, or `go test ./...`. Repository configuration cannot grant itself local-execution or environment-forwarding permission.

Before a native run's agent starts, Dispatch makes sure each program in `checks.baseline` and `checks.verify` can be found, and runs the `checks.baseline` commands, if you listed any, on the run's snapshot (S0). With an empty `checks.baseline` nothing runs before the agent: afterwards, only the `checks.verify` commands that failed are run again on S0, to tell work that broke them from a project that was already broken.

## Safety and source behavior

> **Local execution is not a security sandbox.**

Real agents and project checks run with the permissions of the Dispatch process and receive `HOME` so installed harnesses can use local authentication. Dispatch clears most other child environment variables. Configured extra variables require the separate `--allow-forwarded-env` acknowledgement and their values are redacted from persisted logs.

Dispatch freezes the source into an internal Git baseline and gives the selected agent an independent candidate workspace. It does not run the agent directly in the original tree. `dispatch accept` uses the existing safe apply path. If the source moved since the snapshot, [work coherence](coherence.md) decides whether the patch is still valid; a stale result is refused and the source is left unchanged. With `coherence.accept: strict`, any source drift is refused. `dispatch reject` never applies candidate changes.

If verification is configured, the same commands run against the candidate and their output is retained. Without configured checks, Dispatch reports `Verification: Not configured`. Verification is mechanical evidence, not a universal code-quality judgment. A completed harness invocation is reported as `Ready for review` when its patch still holds against the source now, `Finished, but stale (REFRESH)` or `Finished, already in the source (STOP)` when it does not; failed checks are reported as `Verification failed`, never as completed or verified work. The summary shows the patch, the verdict and what accept would verify; a `CONTINUE` there does not mean merged checks have passed.

The local backend is the supported real-agent path in this candidate. Docker execution is advanced and experimental: users must provide a suitable image containing the agent and project toolchain.

## Local state and artifacts

Normalized state lives in `~/.dispatch/dispatch.db`; `--state-dir` or `DISPATCH_HOME` overrides that location. Per-run evidence lives under `~/.dispatch/runs/<run-id>/`, including the frozen baseline, candidate workspace, bounded stdout/stderr, structured agent output, checks, patch, events, and inspectable metadata.

No network service is required to create, execute, inspect, accept, or reject a task, and Dispatch uploads nothing.

## Current boundaries

- Experimental developer preview, not a stable 1.0 service.
- Real-agent testing centers on Codex CLI and Claude Code (see [provider support](provider-support.md)); Cursor Agent and Hermes Agent remain available as an explicit `--agent cursor` or `--agent hermes` choice without a funding contract. Hermes passed attach and native runs in the 0.4.14 conformance trials; its own worktree mode, which 0.4.14 did not recognize, passed with 0.4.15 on 2026-10-16, in one trial without the hook (the last-seen path). Agent installation, authentication, quotas, and provider availability remain external prerequisites.
- Coherence evidence comes from the fixture matrix and a small number of runs. False-refresh and false-continue rates on real repositories are not measured, no real run has been stopped by the mid-run watcher, and no token, time or cost saving is claimed.
- One agent per run, no automatic retry, no task decomposition, no agent racing, ML, embeddings, LLM judging, background refresh or network service. The background watcher (`dispatch start`) is one local process per project: it keeps verdicts current, finishes Work whose session ended when you allowed the project's checks, and applies only attached work you allowed to integrate; it never launches, refreshes or signals agents. It recognizes only Claude Code's, Codex's, Cursor Agent's and Hermes Agent's worktrees by itself, sees only Claude Code's and Hermes's sessions end, and scans no processes.

## Testing and CI

The tests need only Rust, `git`, `python3` and `sh`; no `docker` CLI.
`scripts/test-in-docker.sh` runs the same validation in `ci/test.Dockerfile`, a
pinned Rust 1.97.1 Linux image, as an ordinary user with your uid. Your checkout
is mounted read-only, `target/` lives in a Docker volume, and the container runs
with `--init`, so orphaned processes are reaped as on a real system. Give it a
command to run that instead, for example `scripts/test-in-docker.sh cargo test
--locked some_test`. `DISPATCH_DOCKER_STRESS=1 scripts/test-in-docker.sh cargo
test --locked` runs the suite on a loaded machine (2 CPUs, 16 busy loops, 8 test
threads), as CI's `tests under load` job does on every pull request. Build first
without it: `scripts/test-in-docker.sh cargo test --locked --no-run`.

CI runs the suite on every pull request and push to `main`, on Linux (in the test
image) and macOS. A release runs the same suite on its tag before it builds any
asset. `stability.yml` runs the suite 20 times a night on both platforms and lists
every failing test with its count. `harness-trials.yml` runs real agents through
`scripts/harness-trial.sh`, only by hand or weekly, never on pull requests, with
keys from a protected environment. Only the fixture reaches the model providers,
sent by the agents; Dispatch uploads nothing.

Dispatch's promise is tested per agent: `scripts/harness-trial.sh <harness>
<path>` runs one shared scenario through each path Dispatch offers an agent and
records pass or fail against Dispatch's own contract. It never compares the
agents. Results: [harness-conformance.md](harness-conformance.md).
