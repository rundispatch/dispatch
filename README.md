<p align="center"><img src="assets/dispatch-logo.svg" width="96" alt="Dispatch"></p>

# Dispatch

**Keep autonomous software work valid while the code moves.**

**Dispatch 0.4.11 — experimental developer preview**

A coding agent works from a snapshot of your source while the real source keeps
changing: you edit files, another run is accepted, a teammate merges. Dispatch
runs the agent in an isolated copy of that snapshot, keeps the patch it produced,
and before applying it checks whether the work still holds against the source as
it is now. The answer is a verdict with reasons, not a merge conflict:

```text
task
  ↓
S0: frozen baseline + isolated candidate workspace
  ↓
one selected coding agent + configured verification
  ↓
Δ: the patch, kept with its evidence
  ↓
validate(facts the work relied on, source now)
  ↓
CONTINUE / REFRESH / STOP, with reasons
  ↓
review → accept or reject
```

Dispatch runs the coding agent you configured in an isolated workspace,
verifies the result when checks are configured, and lets you review and accept
it. Those are how Dispatch carries the work. Coherence is what it adds: the
verdict is always recomputed from the snapshot, the patch and the source now,
and a stale result is never applied.

Everything Dispatch does is local. Provider execution requires the provider’s
service; Dispatch itself needs no account, no network service and no upload.

## Two ways to use Dispatch

- **Let Dispatch launch the agent.** `dispatch run "<task>"` runs your coding
  agent from a frozen snapshot in an isolated workspace. This needs a resource
  set up with `dispatch setup`, or an explicit `--agent`.
- **Protect work you run yourself.** Run `dispatch attach -- <agent command>`
  from your checkout and Dispatch makes the agent its own workspace, so any
  agent CLI works isolated, with or without worktree support of its own.
  `dispatch attach --workspace <dir>` observes an agent already working in a
  worktree, and `dispatch finish` freezes the result. No resource setup is
  needed.

Either way, the result is judged the same way: `dispatch check`, `accept` and
`reject` work on it against the source as it is now.

**Watching a project.** `dispatch start` watches the project in the
background and gives you your shell back. While it runs, Dispatch keeps the
verdict of every Work item it knows about current as the code moves:
- results waiting for your review, native or attached;
- attached work still in progress;
- attached work it may apply.

`dispatch watch` shows that state live. On a terminal it is a table of Work,
named by its worktree folder or its task, with Work that needs you first; the
selected Work's details say why its verdict is what it is and what to do next.
You act on the selected Work without typing its ID: `f` finishes, `a` accepts,
`r` rejects (after asking, naming the Work), `d` reviews. Below 90 columns
each Work is a two-line card. Leaving it does not stop watching. `dispatch stop` ends watching for
this project. No agent needs to be set up
for any of this.

```text
$ dispatch start
✓ Watching ~/src/project
  Dispatch is running in the background. See it: dispatch watch · Stop: dispatch stop
```

**Work that appears by itself.** With Claude Code's hooks installed (`dispatch
setup` → Runtime integrations, shown and approved first), a Claude Code session
in its own worktree of a watched project (`claude --worktree`) becomes Work by
itself.
- S0 is the worktree as the session found it, before its first edit.
- Later sessions in the same worktree join the same Work.
- When Claude Code removes the worktree, Dispatch first keeps its exact
  changes, then waits for you to `finish` or `reject` them.
- If you have let this project's checks run by themselves (`dispatch setup
  --checks`), Dispatch runs them on the kept changes instead, and the Work waits
  for your review. That permission names the exact commands: when
  `checks.verify` changes, it no longer holds until you approve the new ones. It
  never lets anything apply.
- A session running directly in your checkout is told that Dispatch cannot
  tell its edits from yours, and nothing is tracked.
- No session in its own worktree is lost silently, even on a busy machine. It
  ends one of three ways, and the session is told which:
  - **tracked:** its Work is created before the hook returns;
  - **pending:** its starting state is saved, and the watching owner finishes
    creating the Work moments later;
  - **not tracked:** its starting state could not be captured in time, or the
    worktree kept changing while it was being captured.

  Work is never left half-created.

To build Dispatch with Dispatch, as its own releases are built, see
[docs/self-hosting.md](docs/self-hosting.md).

**Work that notices Work.** While a project is watched, Dispatch also compares
the Work that is not yet integrated, and says where two pieces already touch:
- both change the same declaration or whole file, or edit overlapping lines;
- one changes the signature of, or removes, a declaration the other uses;
- one changes or deletes a file the other relies on.

A row in `dispatch watch` then names the other Work under TOUCHES (piped, it
ends with `interacts with <id>`). The selected Work's details, and `dispatch
status`, say how, apart from the verdict: Work can be
`CONTINUE` and still interact. This is exact to the declaration for Rust and
Python, and to the file elsewhere. It is advisory: nothing is blocked,
reordered or refreshed, and once one piece lands the other is judged by the
coherence check as before.

**How often interactions are right is measured.** When Work lands, Dispatch
records the interactions it had with the other Work in progress
(`interaction.landed`). The watching owner then records its next verdict on
each of those other pieces (`interaction.outcome`), whether or not an
interaction was reported. Documented queries in
`docs/queries/interactions-measured.sql` read the precision and misses from
your own state directory. Nothing leaves your machine. "Invalidated" there
means Dispatch's own later verdict, not a confirmed conflict.

Watching covers Work Dispatch launched, that you attached, and sessions whose
runtime reports them. It does not scan for agent processes. After a reboot, run
`dispatch start` again. `dispatch serve` does the same watching in the
foreground, and `check` and `status` answer on demand.

## Work coherence: keeping results valid while the code moves

Before 0.2.0 any difference anywhere in the tree refused `dispatch accept`, even
an edited README, and Dispatch could not tell a harmless change from one that
invalidates the work.

Dispatch treats a run like an optimistic transaction. **S0** is the snapshot
the run started from and **Δ** is the patch the agent produced. From S0 and Δ,
Dispatch derives the *facts the work relied on*: the declarations the patch edits,
the declarations its new code uses, and, for files it cannot parse, the files
themselves. When you accept, Dispatch observes the current source (ignoring what
`.gitignore` excludes), checks that Δ still applies, and checks that those facts
still hold. The answer is a verdict, always recomputed from S0, Δ and the source as
it is now. Nothing about it is trusted from an earlier moment.

| Verdict | Meaning | At accept time | Mid-run |
|---|---|---|---|
| `CONTINUE` | The source did not move, or nothing the work relied on changed. | Applies against the current source, after your `checks.verify` pass on the merged result if the source moved. | Nothing, unless it follows an earlier invalid verdict (`coherence.checked`). |
| `REFRESH` | Δ no longer applies, a relied-on fact changed, or a check failed on the merged tree. | Nothing is applied and the source is left unchanged; the message names `dispatch refresh`. | `coherence.invalidated` is recorded and the agent continues. Only with `mid_run: stop` and `stop_on_refresh: true` is it stopped. |
| `STOP` | Δ is already present in the source. | Nothing is applied; the message points to `dispatch reject`. | `coherence.invalidated` is recorded. With `mid_run: stop` the agent is stopped. |

### Accept-time flow

- `dispatch check [run]` evaluates the finished result now and prints the verdict,
  analysis level, number of changed files, reasons and the next command. It is
  read-only, stores nothing, and exits 0 for any verdict (1 if it cannot evaluate).
  It runs the file, patch and symbol layers only; integration checks run at accept.
- `dispatch accept [run]` applies the result and then records your acceptance. If
  the tree is byte-identical to the snapshot, behavior is unchanged. If it moved,
  the verdict gates the apply. A `REFRESH` or `STOP` leaves the source untouched,
  shows the application as blocked by source drift, and records no acceptance: the
  result stays pending until you refresh or reject it.
- `dispatch accept [run] --despite-refresh --explanation "<why>"` applies a `REFRESH`
  that comes only from the file and symbol analysis, when you have checked that the
  work still holds. Your checks must still run and pass on the merged tree, and the
  overridden verdict is recorded with your explanation. It never overrides `STOP`, a
  patch that no longer applies, or a failing check, and auto-apply never uses it.
- `dispatch refresh [run]` starts a **new** run of the same task against the current
  source. The task gets a fixed note naming the earlier run and up to ten reasons it
  went stale. The old run is not modified. It needs the same explicit flags as `run`
  (for example `--allow-unsafe-local`) and repeats the original launch choices,
  such as a fixed agent, model or effort. Dispatch never relaunches automatically.

Worked example. A run adds a call to `auth::validate(&token)` in `src/handler.rs`.
While it waited for review:

```text
# You edit README.md and an unrelated function in src/util.rs.
$ dispatch check
Run <id>
Coherence: CONTINUE
Analysis: files_only
World changed files: 2
Next: dispatch accept <id>
$ dispatch accept          # applies onto the edited tree

# Instead, someone changes validate's signature in src/auth.rs.
$ dispatch check
Run <id>
Coherence: REFRESH
Analysis: symbols
World changed files: 1
  fact_broken: pub fn validate(token: &Token) -> Result<User, AuthError> => pub fn validate(ctx: &AuthContext, token: &Token) -> Result<User, AuthError>
Next: dispatch refresh <id>  (or dispatch reject <id>)
```

All keys are optional and live in `dispatch.yml`; the values shown are the defaults.
The block is read from the configuration frozen with each run.

```yaml
coherence:
  accept: validate        # validate: check the moved tree; strict: refuse on any drift (0.1.x behavior)
  mid_run: observe        # observe: record verdicts while the agent works; stop: cancel it on STOP
  stop_on_refresh: false  # with mid_run: stop, also cancel on REFRESH
  poll_secs: 10           # how often the mid-run watcher looks for source changes (must be > 0)
  integration_checks: true  # when the source moved, run checks.verify on the merged tree before applying
```

### What it checks and what it does not

- Symbol facts exist for **Rust and Python** only. Other languages use file-level
  facts: any change to a file the patch edits, or mentions by path, is a `REFRESH`.
- A referenced symbol is bound by *unique name* in the baseline, not by full name
  resolution. Ambiguous or very common names are skipped, so some breakage is missed.
- A Python function that only gains optional parameters (defaults, `*args`,
  `**kwargs`) keeps code that calls it valid. Any other signature change to a
  function the work calls is a `REFRESH`; Rust signatures compare exactly.
- Integration checks run your configured `checks.verify` on the merged tree in a
  scratch copy of the non-ignored files, with no build cache. They hold the apply
  locks while running. On the local backend they are skipped for a run that was not
  itself approved for local execution. A check that fails, times out or cannot run
  produces `REFRESH`.
- Two edits to the same symbol are reported as `REFRESH`. Python appends at the very
  end of a function are not seen as same-symbol edits by the symbol layer; the patch
  check and your checks still apply.
- Transitive behavior changes (a callee's callee) are caught only if your checks
  cover them.
- Nested repositories and submodules are not analysed.
- Mid-run verdicts are advisory by default.

### Auto-apply

A session mode (Shift+Tab, or `/auto-apply on`) and a CLI flag (`dispatch run
--auto-apply`, `dispatch refresh --auto-apply`) apply an eligible result
automatically instead of waiting for review. Auto-apply is never auto-accept: it
applies only a verdict whose evidence is complete for the exact world it names — an
unmoved world with passed verification, or a moved world whose merged tree passed
your own checks — and it never records human acceptance, so review stays `pending`.
See the [product guide](docs/product-guide.md#auto-apply) for the mode and
[coherence reference](docs/coherence.md#automatic-application-auto-apply) for the
full eligibility and authorization rules.

### Attach your own agent

`dispatch attach` puts work from an agent Dispatch did not launch — Claude Code, Codex, Cursor, a script — under the same coherence checking, in its own worktree:

```sh
dispatch attach --auto-apply -- claude -p "add input validation"   # from your checkout: Dispatch makes the workspace
dispatch attach --workspace ../scratch --agent codex --auto-apply  # or observe one already running
dispatch finish <run-id>                                           # you say when it's done
dispatch start                                                     # keeps a foreign attachment observed and applies it
```

Dispatch is honest about what it saw: full confidence when S0 is the workspace as the work began (a workspace Dispatch made, or a session's start) or a real Git merge-base commit; partial confidence when it had to snapshot a directory at attach time, or when a session resumed in a worktree it had not seen, since earlier edits are then invisible to the patch. A workspace Dispatch made is removed once its work is applied, and kept otherwise until you run `dispatch clean`, which lists what it would remove and asks first. See [attach.md](docs/attach.md).

See the [coherence reference](docs/coherence.md) for the model, rules, events and
the fixture matrix, and [coherence validation](docs/coherence-validation.md) for
what is claimed today, what is measured during real use, and what would show the
thesis to be wrong.

## Interactive loop

From a source directory, open Dispatch. Missing resources lead to guided setup
and preserve your goal. You can also configure resources first:

```bash
dispatch setup                 # provider login guidance and explicit funding consent
dispatch setup --checks        # approve a known project check
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
See [setup and review](docs/product-guide.md), [visual system](docs/design-system.md)
and [candidate validation](docs/product-rc-validation.md).

## One-shot CLI

From a repository or plain directory:

```bash
cd my-project
dispatch run "Fix the retry race"
```

Local agents run with your operating-system permissions. Dispatch asks for confirmation before execution; scripts can use `--allow-unsafe-local` after accepting that risk.

Review and apply the result without copying a run ID:

```bash
dispatch diff
dispatch accept
```

Or reject it without changing the source tree:

```bash
dispatch reject
```

To run a specific agent:

```bash
dispatch run "Fix the retry race" --agent claude
```

`dispatch explain` shows which agent and resource the last task used.

## How Dispatch chooses an agent

Each run uses exactly one agent:

- With profiles in `resources.yml` (written by `dispatch setup`), Dispatch uses
  the first eligible profile in file order. `--agent`, `--model` and `--effort`
  only narrow the choice. If no eligible profile matches, the run is refused
  with each profile's reason; it never falls back to an unchecked agent.
- With no profiles configured (no `resources.yml`, or `allocation_enabled:
  false`), `--agent claude|codex|cursor` runs that agent with its own login and
  no funding contract is checked.
- With neither, `dispatch run` refuses: run `dispatch setup`, or pass `--agent`.

There is no ranking, benchmark data or automatic fallback to another agent.

## Install

Download the archive for your platform and `SHA256SUMS` from the
[latest GitHub release](https://github.com/rundispatch/dispatch/releases), verify
the checksum, and run the executable by its explicit path; see
[install / upgrade / uninstall](docs/release-install.md).

Interactive testing for this release was on macOS arm64. Linux x86_64 is built and
smoke-tested by CI but not exercised interactively. The macOS package is unsigned and
not notarized.
Source builds use `cargo build --release --locked`; Git and the project’s actual
check tools must be installed. No provider tool or font is installed by Dispatch.

## Safety and source behavior

> **Local execution is not a security sandbox.**

Real agents and project checks run with the permissions of the Dispatch process and receive `HOME` so installed harnesses can use local authentication. Dispatch clears most other child environment variables. Configured extra variables require the separate `--allow-forwarded-env` acknowledgement and their values are redacted from persisted logs.

Dispatch freezes the source into an internal Git baseline and gives the selected agent an independent candidate workspace. It does not run the agent directly in the original tree. `dispatch accept` uses the existing safe apply path. If the source moved since the snapshot, [work coherence](#work-coherence-keeping-results-valid-while-the-code-moves) decides whether the patch is still valid; a stale result is refused and the source is left unchanged. With `coherence.accept: strict`, any source drift is refused. `dispatch reject` never applies candidate changes.

If verification is configured, the same commands run against the candidate and their output is retained. Without configured checks, Dispatch reports `Verification: Not configured`. Verification is mechanical evidence, not a universal code-quality judgment. A completed harness invocation is reported as `Ready for review`; failed checks are reported as `Verification failed`, never as completed or verified work.

The local backend is the supported real-agent path in this candidate. Docker execution is advanced and experimental: users must provide a suitable image containing the agent and project toolchain.

## Core commands

```text
dispatch setup [codex|claude | --checks]
dispatch resources
dispatch run "<task>" [--source path] [--agent claude|codex|cursor]
dispatch run "<task>" --json|--jsonl
dispatch status [run-id] [--json|--jsonl]
dispatch diff [run-id]
dispatch accept [run-id]
dispatch reject [run-id]
dispatch explain [run-id]
dispatch check [run-id] [--json]
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

Without a run ID, `status`, `diff`, `accept`, `reject`, `explain`, `check`, and `refresh` resolve the latest relevant single-result run for the current source tree. Explicit run IDs remain available for history and debugging. `--task-file path|-` reads a task from a file or stdin, and `--source path` overrides the current directory.

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
[provider support](docs/provider-support.md).

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
```

Use the command that actually verifies the project, such as `pytest`, `npm test`, or `go test ./...`. Repository configuration cannot grant itself local-execution or environment-forwarding permission.

## Local state and artifacts

Normalized state lives in `~/.dispatch/dispatch.db`; `--state-dir` or `DISPATCH_HOME` overrides that location. Per-run evidence lives under `~/.dispatch/runs/<run-id>/`, including the frozen baseline, candidate workspace, bounded stdout/stderr, structured agent output, checks, patch, events, and inspectable metadata.

No network service is required to create, execute, inspect, accept, or reject a task, and Dispatch uploads nothing.

## Current boundaries

- Experimental developer preview, not a stable 1.0 service.
- Real-agent testing centers on Codex CLI and Claude Code (see [provider support](docs/provider-support.md)); Cursor Agent remains available as an explicit `--agent cursor` choice without a funding contract. Agent installation, authentication, quotas, and provider availability remain external prerequisites.
- Coherence evidence comes from the fixture matrix and a small number of runs. False-refresh and false-continue rates on real repositories are not measured, no real run has been stopped by the mid-run watcher, and no token, time or cost saving is claimed.
- One agent per run, no automatic retry, no task decomposition, no agent racing, ML, embeddings, LLM judging, background refresh or network service. The background watcher (`dispatch start`) is one local process per project: it keeps verdicts current and applies only attached work you allowed to integrate; it never launches, refreshes or discovers agents.

## Development

Rust owns execution, persistence, coherence and review. There is no server component.

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Detailed contributor guardrails live in [AGENTS.md](AGENTS.md).
