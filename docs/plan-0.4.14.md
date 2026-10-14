# Plan: Dispatch 0.4.14, every agent checked the same way

**Draft, 2026-10-14.** Base: `4573303` (Dispatch 0.4.13 plus the README explainer and the `Site impact` rule in AGENTS.md). Packets run in
parallel through a pinned Dispatch 0.4.13, as in `docs/plan-0.4.13.md`.

## 1. Objective

Dispatch's promise does not depend on which agent wrote the change. 0.4.14
tests that claim against the agents people use, fixes what the tests find, and
moves testing off the developer's Mac:

- **Harness conformance.** Claude Code, Codex, Cursor and Hermes each go
  through every path Dispatch offers them, on one shared scenario, and the
  outcome is recorded per harness and path. The record is pass/fail against
  Dispatch's own contract. It never rates or ranks the agents.
- **Fixes from the trials.** Adapters are brought up to what each CLI does
  today. Another harness's worktrees are recognized only where they leave a
  reliable marker, as Claude's do.
- **Trust for the integrator.** Close the gaps that made the 0.4.13 integrator
  write `verify.sh` and `land.sh`, so that Dispatch's own commands are enough to
  land a packet.
- **Tests in containers, locally and on GitHub Actions.** The same test image
  runs on a laptop and in CI. CI runs the suite on both shipped platforms, and
  the stability and real-agent trials run there instead of on one Mac.

## 2. Evidence

### Harnesses on the dev Mac (2026-10-14)

| Harness | Version | Non-interactive | Own worktree mode | Dispatch today |
|---|---|---|---|---|
| Claude Code | 2.1.296 | `-p`, `--output-format stream-json` | `--worktree`: `.claude/worktrees/<name>`, `CLAUDE_BASE` marker, lock naming the session's pid and start time | Native adapter; hooks; discovered without hooks (0.4.13) |
| Codex CLI | 0.155.1 | `codex exec --json` | `codex exec --worktree`, "a new managed Git worktree"; location and markers not yet observed | Native adapter (`exec --ephemeral --sandbox … --json`); attach |
| Cursor Agent | 2026.09.28 | `-p --output-format stream-json` | `-w/--worktree [name]` at `~/.cursor/worktrees/<repo>/<name>`, outside the project; `--worktree-base <ref>` | Native adapter `cursor`, unchanged since v0.1.0, generic JSONL parsing; wrapped attach trialed in 0.4.6–0.4.9; no native run in any plan log |
| Hermes Agent | 0.18.2 | `-z <prompt>`, `chat -q … -Q`, `--usage-file <path>` | `--worktree`; location and markers not yet observed | No adapter; attach only |

Every harness can already be used through `dispatch attach -- <cli>`, which
gives it a Dispatch-made workspace. Only Claude's own worktrees are recognized.

### Integrator trust gaps (0.4.13 build)

Each was observed while landing 0.4.13 through Dispatch:

1. **No merged checks before accept.** `verify.sh` applied a worker's live diff,
   untracked files included, to the clone's HEAD and ran the full suite before
   the human approved. Dispatch runs merged checks only inside `accept`.
2. **Nothing ties the review to what lands.** `land.sh` compared the frozen Δ's
   tree with the tree that was reviewed. The workspace can change between review
   and `finish`.
3. **Check results are hard to read.** Per-command status came from
   `runs/<id>/metadata.json` through `jq`.
4. **Merged checks are slow.** A full suite ran on every accept, and one flaky
   test refused an accept. There are no automatic retries, by design.
5. **Symbol facts were not exercised.** Packets owned different files, so every
   verdict was CONTINUE.

### Testing infrastructure

- CI runs `fmt`, `test` and `clippy` on `ubuntu-22.04` only. macOS arm64 ships,
  and is where Dispatch is developed, but its tests run only on the dev Mac. The
  release workflow builds and smoke-tests the binary without running the suite.
- The dev Mac's real-time scanner makes suite timing swing more than sixfold.
  This is why the twenty-clean-runs stability bar is still deferred.
- Docker 29.6.1 is installed locally. Dispatch already has a Docker execution
  backend (`execution.backend: docker`). The containers in this plan are for
  testing; they add no backend.

### What P0 must answer, per harness

- Where the harness's own worktree goes, and what marks it as made by that
  harness: a file in `.git/worktrees/<name>/`, a lock reason, a branch name or a
  path.
- Whether anything names the running process (a lock with pid and start time,
  or a session file), and what is left when the process exits.
- The exact non-interactive output: the final message, usage, errors and
  version. Captured as transcripts for the adapter tests.
- How it authenticates in a Linux container: API-key environment, a mounted
  login directory, or a CI token.
- Whether hooks exist that report session start and end, and their payloads.

## 3. Contracts

### A. Shared scenario and conformance record (P0, integrator)

The scenario is the explainer's, reproduced in
`~/dispatch-dev/explainer/provenance/`. A Python fixture contains `auth.py`
(`def validate(token)`), `admin.py` (`def promote(token)`), `audit.py`,
`api.py` and `README.md`. There are three tasks, given to real agents as
prompts:

- **B:** scope `validate` to a tenant (`validate(ctx, token)`);
- **A:** audit `promote` (adds a call to `validate(token)`);
- **C:** document auth in `README.md`.

**Expected behavior:**

- While A and B are in flight, the interaction is reported.
- Accepting B applies it.
- A is then REFRESH with `fact_broken: def validate(token): => def validate(ctx, token):`.
- C stays CONTINUE.

**Record** (one JSON per harness × path, plus a table in
`docs/harness-conformance.md`):

- whether Work was created;
- the S0 and its provenance;
- Δ equal to the agent's change (tree hash);
- session start and end recorded;
- usage as reported, or `unknown`;
- the interaction seen;
- each verdict, expected and observed;
- the harness version;
- the evidence path.

**Paths per harness:**

1. wrapped attach;
2. a native run, where an adapter exists;
3. the harness's own worktree mode while Dispatch watches.

A cell is `pass`, `fail` (with the defect) or `not offered`. No cell
compares agents' code quality.

### B. Conformance runner (P1)

`scripts/harness-trial.sh <harness> <path>`:

- creates the fixture in a fresh directory with its own `DISPATCH_HOME`;
- runs the three tasks for that harness and path;
- writes the record and an evidence directory: events, run metadata, harness
  stdout/stderr, and any transcript the harness keeps.

The same script runs on the host, in the harness test image and in GitHub
Actions. A real agent is invoked only when the harness's credentials are
present; otherwise the cell reads `skipped: no credentials`.

### C. Other harnesses' worktrees (P2, gated by P0's evidence)

- **Recognition where the signals are reliable.** For each harness whose
  worktree mode leaves a reliable marker, recognition goes in
  `src/runtime/<harness>.rs` beside `claude.rs`. It uses the same contract:
  made-by test, `Liveness::{Running, Gone, Unknown}`, and registration through
  `registration::discovered`. Session end comes from that harness's lock or
  session record.
- **No liveness signal.** Liveness stays `Unknown`, and the hint says the
  session end is not observable.
- **No reliable marker.** The worktree stays "found, not tracked", with
  `dispatch attach --workspace <path>`, as today.
- **Worktrees outside the project.** Cursor's, for example: discovery already
  lists every linked worktree with `git worktree list`, wherever it is.
  Recognition must not walk `~/.cursor`.
- **No guessing.** No process scanning, and no inferring an agent from file
  contents.

### D. Native adapters (P3)

- **Cursor:** parse the current `stream-json` for the final result, usage, model
  and errors, against P0's transcripts.
- **Codex:** check the adapter's flags against 0.155.1 and fix what changed.
- **Hermes (decision D2):** a thin adapter. `-z <prompt>` runs non-interactively,
  `--usage-file` provides usage, and the model and provider come from config. It
  joins `SUPPORTED_HARNESSES`, `setup` and `--agent`.
- **Every adapter** stays within the adapter discipline: discovery, command,
  output parsing and failure semantics. Each gets a transcript-based unit test.

### E. Trust (P4)

1. **`dispatch check <id> --merged`.**
   - Runs `checks.verify` on the current source with the Work's Δ as of now, in
     a scratch copy.
   - For unfinished Work, Δ is the live workspace change, untracked files
     included, exactly as `finish` would capture it.
   - It applies nothing and changes no verdict.
   - The result is recorded as an event with the source state and the Δ digest.
   - `accept` still runs its own merged checks, because the source can move
     after.
2. **Δ digest.**
   - `diff`, `check` and `finish` print `Δ <digest>`: a SHA-256 of the patch
     bytes, 12 characters shown.
   - `dispatch accept <id> --expect <digest>` refuses before anything else if
     the frozen Δ differs.
   - The digest is recorded with the review.
3. **Per-command check results.** `finish`, `check --merged` and `accept` show
   each command with pass/fail, exit code, duration and log path. Test-runner
   counts are not parsed, so they stay unknown.
4. **Merged-check cost, measured only.** Record each merged-check run's duration
   if it is not already an event. There is no caching in 0.4.14; decide with the
   numbers.

### F. Containers and GitHub Actions (P5)

1. **Test image.** `ci/test.Dockerfile` contains the pinned Rust toolchain,
   `git`, `python3` and `sqlite3`. `ci/harness.Dockerfile` adds pinned harness
   CLIs on top. `scripts/test-in-docker.sh` runs fmt, clippy and test with the
   repository mounted read-only and `target/` in a named volume.
2. **CI.**
   - The test job runs as a matrix on `ubuntu-22.04` and `macos-15`.
   - The Linux job runs in the test image, the same one used locally.
   - Cargo builds are cached with `actions/cache`.
3. **`stability.yml`.** Nightly and on demand. It runs the suite 20 times on
   Linux and macOS and uploads each failing test's log. This is the
   twenty-clean-runs bar, moved off the dev Mac.
4. **`harness-trials.yml`.** Manual and weekly, never on pull requests.
   - A harness × path matrix runs `harness-trial.sh` in the harness image.
   - Credentials come from a protected `harness-trials` environment with a
     required reviewer.
   - Evidence is uploaded as artifacts. Only the fixture is sent to the model
     providers, by the agents themselves; Dispatch still uploads nothing.
5. **Release.** The suite must pass on both platforms before assets are built.

## 4. Packets

| Packet | Owner | Contract | Depends on |
|---|---|---|---|
| P0 | integrator | §2 questions answered by a local trial of 4 harnesses × 3 paths; fixture; stubs; test image skeleton; §2–3 amended | — |
| P1 | worker | B, the conformance table | P0 |
| P2 | worker | C, per harness, only where P0 found reliable signals | P0 |
| P3 | worker | D | P0 |
| P4 | worker | E | — |
| P5 | worker | F | P0 (image skeleton) |
| R | worker | docs, site claims list, version | all |

**Overlap is allowed by design.** P2 and P3 both touch the harness registry
(`SUPPORTED_HARNESSES`, `HarnessesConfig`, `adapter_for`). P1 and P4 both
consume check output. This exercises symbol facts on Dispatch itself (gap 5).
Record every verdict and every interaction, as in §7 of 0.4.13.

**Workers on other harnesses (decision D3).** At least one packet each is
built by Codex, Cursor and Hermes through `dispatch attach -- <cli>`; Claude
builds the rest. Outcomes are recorded per worker. Human review stays the
quality signal, and there is no ranking.

**Integrator scripts retire as P4 lands.** Once E1–E3 are in, the remaining
packets land with `check --merged` and `accept --expect` instead of `verify.sh`
and `land.sh`. The log records whether the scripts were still needed.

## 5. Acceptance

- `docs/harness-conformance.md` has a cell for each harness × path:
  `pass`, `fail` with the defect fixed or listed as known, or `not offered`.
  Each cell links its evidence.
- The 0.4.14 suite is green on Linux and macOS in CI. `stability.yml` shows
  20/20 clean runs on both, or every flake found is fixed.
- E1–E3 have regression tests. At least the last packets land without
  `verify.sh` or `land.sh`.
- `harness-trials.yml` has run once for every harness with credentials. Its
  records match the local P0 trial or explain the difference.
- A 0.4.13 state directory upgrades with identical outcomes. 0.4.13's
  discovery reproductions still hold.

## 6. Deliberately not built

- No scoring, ranking or routing of harnesses from conformance results.
- No new execution backend.
- No recognition of a harness's worktree without a reliable marker. No process
  scanning, and no walking another tool's home directory.
- No caching of merged checks yet (E4 measures first), and no automatic
  retries of flaky checks.
- No real agents on pull requests, and no credentials outside the protected
  environment.
- No services. GitHub Actions is the only remote, and it is test
  infrastructure, never required to run Dispatch.

## 7. Decisions for the user

- **D1. Credentials for `harness-trials.yml`.**
  - Which harnesses get keys in the protected environment, a monthly budget,
    and the schedule.
  - Recommended: weekly plus manual; every harness whose provider offers an API
    key; a cap of turns and wall-clock time per cell.
- **D2. Hermes native adapter in 0.4.14.** Recommended: yes, if P0 shows that
  `-z` with `--usage-file` gives a stable final result and usage. Otherwise
  Hermes stays on attach until it does.
- **D3. Workers on Codex, Cursor and Hermes.** Recommended: yes, one packet
  each. It is the strongest test of attach with those agents.
- **D4. Overlapping packets.** Recommended: yes, as in §4.
- **D5. Test images.** Publish to GHCR, or build per run with layer caching.
  Recommended: build per run first; publish only if build time hurts.
