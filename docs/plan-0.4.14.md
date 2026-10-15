# Plan: Dispatch 0.4.14, every agent checked the same way

**Executed 2026-10-14 to 2026-10-15; see §8.** Base: `4573303` (Dispatch 0.4.13 plus the README explainer and the `Site impact` rule in AGENTS.md). Packets run in
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
| Cursor Agent | 2026.09.28 (self-updated to 2026.10.14 during P0) | `-p --output-format stream-json` | `-w/--worktree [name]` at `~/.cursor/worktrees/<repo>/<name>`, outside the project; `--worktree-base <ref>` | Native adapter `cursor`, unchanged since v0.1.0, generic JSONL parsing; wrapped attach trialed in 0.4.6–0.4.9; no native run in any plan log |
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

### What P0 observed (2026-10-14)

Evidence is in `~/dispatch-dev/p0-0414/runs/<harness>-{worktree,native}/`. For
each harness, it ran task B once in its own worktree mode while the pinned
0.4.13 watched, and once as a native run through 0.4.13. `probe-worktree.sh`
and `probe-native.sh` reproduce them.

| | Claude Code 2.1.296 | Codex 0.155.1 | Cursor Agent 2026.10.14 | Hermes 0.18.2 |
|---|---|---|---|---|
| Own worktree | `<project>/.claude/worktrees/<name>`, branch `worktree-<name>` | `exec --enable worktrees --worktree`, a feature flag: `~/.codex/worktrees/<4 hex>/<project dir>`, detached at HEAD | `-p --worktree <name>`: `~/.cursor/worktrees/<project dir>/<name>`, `git worktree add -b <name>` from HEAD | pending: no provider configured |
| Marker in `.git/worktrees/<id>/` | `CLAUDE_BASE` | `codex-thread.json` (`ownerThreadId`) and `config.worktree` | none | pending |
| Process signal | lock with pid and start time | none | none; a `worker-server` process outlives the agent | pending |
| 0.4.13 discovery | Work within 10 s | not seen | not seen | — |
| Native run via 0.4.13 | completed | completed; usage total right, categories empty, no model in the stream | completed; `cacheReadTokens` dropped, categories empty | no adapter |
| Hooks | yes | yes (`~/.codex/hooks.json`: `SessionStart`, `Stop`, …), deferred | not checked | yes (`hermes hooks`) |
| Container credentials | `ANTHROPIC_API_KEY`, or `claude setup-token` | `codex login --with-api-key` | `CURSOR_API_KEY` | provider key |

**Other observations:**

- **Codex default model.** The configured default here (`gpt-6.1-sol`) is
  refused under a ChatGPT login (`turn.failed`, HTTP 400). The trials pass
  `-m gpt-6-sol`.
- **Cursor self-update.** Cursor updated itself in the middle of a run
  (2026.09.28 to 2026.10.14), so CI images must disable that.
- **Hermes.** `-z` without a provider exits 1. Its `--usage-file` records
  `"failed": true` and the message, with null counts.
- **The test image, on this Mac (linux/aarch64): 697 passed, 4 failed.**
  - One test needs a `docker` CLI in a trusted path.
  - Three need permission denials, which root ignores, so the image must run
    tests as a non-root user.

  This was the first time the suite ran in a Linux container.

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

**Interface (fixed in P0, shared by P1 and P5):**

- **Invocation:** `scripts/harness-trial.sh <harness> <path>`.
  - `<harness>` is one of `claude`, `codex`, `cursor` or `hermes`.
  - `<path>` is one of `attach`, `native` or `worktree`.
- **Environment:**
  - `DISPATCH_BIN` is the Dispatch binary, `dispatch` on `PATH` by default.
  - `TRIAL_OUT` is the output root, `./harness-trials` by default.
  - `TRIAL_TIMEOUT_SECS` is the wall-clock limit for each agent invocation,
    600 by default.
- **Output:**
  - `$TRIAL_OUT/<harness>-<path>/record.json` (schema in §3 A) next to the
    evidence files;
  - one summary line on stdout.
- **Exit status:** 0 pass, 1 fail, 2 not offered, 3 skipped (no CLI or no
  credentials).
- **Credentials, from the environment only and never written into the
  record:**
  - claude: `ANTHROPIC_API_KEY` or `CLAUDE_CODE_OAUTH_TOKEN` (from
    `claude setup-token`);
  - codex: `OPENAI_API_KEY`, fed to `codex login --with-api-key` in a fresh
    `CODEX_HOME`;
  - cursor: `CURSOR_API_KEY`;
  - hermes: the provider key its config names (set in P0).

  On a developer machine, an existing login counts as credentials.

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

The user chose every recommendation (D1–D5) on 2026-10-14. During the build
they also chose:
- P5 finished by Claude after Cursor's free plan ran out;
- P1 on Claude, because Hermes had no provider;
- validating on GitHub CI through draft PR #22, while unrelated jobs held the
  dev Mac near load 200;
- packet P6 after the user set the standard "all CI should pass";
- stress mode as the CI guard on every pull request.

## 8. Execution log

### Setup (2026-10-14)

| Item | Value |
|---|---|
| Supervisor | Dispatch 0.4.13 release binary, SHA-256 `00495cef…` (matches its `BUILD.json`), read-only, state `~/dispatch-dev/state-0414` |
| Integration clone | `~/dispatch-dev/integration-0414`, branch `release-0.4.14` from `4573303`; `/dispatch.yml` and `/.claude/` excluded at the root only |
| Workers | Root-wrapped `dispatch attach -- <agent>` sessions in herdr tabs: Claude Code in `--permission-mode auto`, Codex 0.155.1 (`-m gpt-6-sol`, workspace-write sandbox), Cursor Agent with `--force` |
| Validation | Draft PR #22 on GitHub CI (Linux in the test image, macOS natively, later the stress job), plus Dispatch's own merged checks at each accept |

**P0 (integrator):**
- the probes in §2;
- the fixture from the explainer's provenance run;
- the test-image skeleton;
- the runner interface (§3 B).

The first container run of the suite found 4 Linux-only failures: three ran as
root, and one needed a host `docker`.

### Landing record

| Packet | Commit | Work | Worker | Verdict at accept | How it landed |
|---|---|---|---|---|---|
| P5 | `e16f58e` | `01M4XKDRKYK7DZMPFEWPMQ6W35` | Cursor Agent, then Claude Code | CONTINUE | `land.sh` tree match. The first accept was refused by one PTY test failing on the merged tree at load ~180; the retry at load ~45 passed |
| P4 | `60092f1` | `01M4XK1P84AN460DYTYYSCYE3H` | Claude Code | REFRESH (file-level: `docs/plan-0.4.14.md`) | `land.sh` tree match; `--despite-refresh` on the user's explanation; merged checks passed |
| integrator | `6f6b490` | — | — | — | Docker CLI on the macOS jobs (the first CI run with P5 failed one test on macOS); later removed by P6 |
| P3 | `8909f26` | `01M4XKPCD84TFRBYBKHBK018Z9` | Codex | CONTINUE (symbols) | `check --merged` (candidate), then `accept --expect 3641b0113705`; `land.sh` cross-check matched |
| P2 | `55b9ea1` | `01M4XKKGX22580FBCJX4VXA0G2` | Claude Code | REFRESH (file-level: `docs/plan-0.4.14.md`) | `check --merged` failed 7 `native_runs` deadline tests at load ~60; an accept was refused on a `coherence_override` timing test at load ~176; the third attempt passed at load ~10. `accept --expect 58c1314b2e71 --despite-refresh` |
| P1 | `587a970` | `01M4XM5H7RK6Q11WQ2VW6EATHA` | Claude Code | CONTINUE | `accept --expect 832036317fa1`; `land.sh` cross-check matched |
| P6 | `7b9723d` | `01M4Y4J7GXND5XSCESB4M1NWBE` | Claude Code | CONTINUE | `check --merged` on the live Δ, then `accept --expect 34e9f1493268` after finish |
| integrator | `2705b4c` | — | — | — | `harnesses.hermes.provider` errors named the provider "model" (found by R) |
| integrator | `2b3f377` | — | — | — | the conformance judge accepts an end Dispatch declares not observable (found by the conformance rerun) |
| R | `2394d5c` | `01M4YW8PPXWSVSG2D1GKNB4H1K` | Claude Code | CONTINUE | `check --merged`, then `accept --expect 6a7ecf201e43` |
| P6b | `34a2445` | `01M4YW8EW12X20WYVKMHSV6YT1` | Claude Code | REFRESH (file-level: `Cargo.lock`, R's version bump) | `check --merged`, then `accept --expect 81e74158762c --despite-refresh` |

- **Usage:** Codex reported 226,562 tokens for P3 at exit (186,570 input plus
  15,568,512 cached, 39,992 output). The Claude and Cursor workers' usage is
  unknown.
- **No Hermes worker:** no provider was configured.

### What the build and its checks caught

**In the harnesses (P0 and the conformance runs):**
- 0.4.13 did not see Codex or Cursor worktrees.
- Cursor's cache-read tokens were dropped.
- A Codex model rejection was reported as a generic failure.
- Cursor lowercases the project folder in its worktree path.
- In 0.4.13, a discovered session's end was lost when `finish` came first.

**In the test infrastructure:**
- Three tests assumed a non-root user, and one needed a host `docker`.
- `cargo` ran as PID 1 in the container and never reaped orphans.

**Timing (P6, P6b):**
- Dozens of failures depended on machine speed. Four were product waits that
  were too short:
  - provider probes;
  - `start`/`stop`;
  - process-group exit;
  - the setup lock.
- A key pressed during a resize was held back.
- A resize could end `watch` on a busy machine.

**In the integrator's own work:**
- The judge disagreed with P2's design for unobservable session ends.
- The Hermes provider error named the provider "model".

### Coherence on Dispatch's own build

- **No REFRESH caught real stale work.**
- **Three REFRESH verdicts were judged false and overridden:**
  - the plan document named in comments (P2, P4);
  - `Cargo.lock` named as a CI cache key (P6b).

  They are recorded as `coherence.overridden` and noted in
  `docs/coherence-validation.md`.
- **Same file, different functions:** P2 and P3 edited different functions of
  `src/orchestrator.rs` and were correctly reported as not interacting.
- **Symbol facts:** they were exercised (P3 was judged at the symbol level after
  P4 landed), but no symbol fact broke.

### Integrator trust gaps (§2), closed or not

| Gap | Result |
|---|---|
| 1. Merged checks before accept | Closed: `check --merged` replaced `verify.sh` for P3, P2, P6, P6b and R |
| 2. Review tied to what lands | Closed: `accept --expect` for every packet from P3 on. `land.sh` ran only as a cross-check, for P3 and P1 |
| 3. Readable check results | Closed: per-command lines in `finish`, `check --merged` and `accept` |
| 4. Merged-check cost | Measured: 10–14 min per full suite at low load, 20 min at load ~180. Durations are recorded in `coherence.merged_checks` |
| 5. Symbol facts exercised | Partly: overlapping packets ran, and symbol-level judgments occurred, but no stale work arose |

### Conformance (final, 2026-10-15, Dispatch 0.4.14 at `34a2445`)

| Harness | attach | native | own worktree |
|---|---|---|---|
| Claude Code 2.1.296 | pass | pass | pass |
| Codex 0.155.1 (gpt-6-sol) | pass | pass | pass |
| Cursor Agent 2026.10.14 | pass | pass | pass |
| Hermes Agent 0.18.2 (OpenAI Codex provider, gpt-6.1-sol; configured after the main run) | pass | pass | fail: Hermes's `chat --worktree` removes its worktree, with the changes, at session end; not recognized |

Evidence: `~/dispatch-dev/conformance-0414/trials/`. The earlier run at
`2705b4c` (0.4.13 version string) is in `trials-2705b4c/`.

### Final validation (integrator, 2026-10-15, on `bc17785`)

- **GitHub CI, run 38748890945:** green on all three jobs.
  - `tests under load (ubuntu-22.04)`, 20 min;
  - `checks (ubuntu-22.04)`, 13 min;
  - `checks (macos-15)`, 30 min.

  The P6b push before it (`34a2445`) was green on the same three jobs.
- **Natively on the dev Mac, final tree:** `cargo test --locked --no-fail-fast`
  passed 734, failed 0, ignored 5 (47 binaries), at load 5–8.
- **State compatibility:**
  - The 0.4.14 candidate ran `check --merged` and `accept --expect` on this
    release's own state, which the pinned 0.4.13 watcher kept serving and
    reading throughout.
  - The schema is unchanged at 24.
  - Packet R checked that 0.4.13 reads a state written by 0.4.14.
- **Not yet run.** These run only from the default branch, so they need the
  merge first; `harness-trials` also needs the protected environment and its
  secrets in the repository settings:
  - `stability.yml` (20 runs, and the nightly stress run);
  - `harness-trials.yml` on GitHub.

### Hermes, after the user configured a provider (2026-10-15)

- The first provider tried was Anthropic through the user's Claude subscription.
  Anthropic refused it: third-party apps draw on "extra usage". The second
  choice, OpenAI Codex, worked.
- **Two findings came from the refusals:**
  - `hermes -z` exits 0 when the provider refuses, prints the message on stdout,
    and leaves `failure` out of the usage file. The adapter already marked such
    runs failed, but recorded an empty reason; fixed in `0e76728`.
  - `hermes -z` ignores `--worktree`. The runner now uses `hermes chat -q …
    --worktree` for the worktree path. Hermes then works in
    `<repo>/.worktrees/hermes-<id>`, adds `.worktrees/` to the checkout's
    `.gitignore`, and removes the worktree and its branch, with the agent's
    changes, at session end.
- **Recognizing Hermes worktrees needs more than a marker.** Dispatch would have
  to keep the Δ before Hermes removes the worktree. This is left for a later
  release.
- **Conformance:** attach pass, native pass, worktree fail.
