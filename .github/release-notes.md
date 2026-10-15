## Dispatch 0.4.14 — Every agent, checked the same way

Dispatch's promise should not depend on which agent wrote the change. 0.4.14 adds a runner that
puts Claude Code, Codex, Cursor Agent or Hermes Agent through each path Dispatch offers it, fixes
what the first trials found, and gives the person landing Work the commands that the 0.4.13
integrator had to write scripts for. The trials record pass or fail against Dispatch's own
contract. They never compare the agents or their code.

- **Merged checks before accept.** `dispatch check <id> --merged` runs your `checks.verify` on
  the source as it is now, with the Work's change applied, in a scratch copy, exactly as accept
  would. It prints one line per command and the Δ it checked:

  ```text
  Merged checks on the current source (Git HEAD 2319edab8fb7) with this Δ: passed
    PASS      exit 0       0.1s  python3 -c "import auth"  logs: <state>/runs/<id>/coherence-checks/<check>/verify-01.{stdout,stderr}.log
    PASS      exit 0       0.0s  test -f auth.py  logs: <state>/runs/<id>/coherence-checks/<check>/verify-02.{stdout,stderr}.log
  Nothing was applied and no verdict changed. accept runs its own merged checks, because the source can move after this.
  Δ 50c4c5a68351
  ```

  - For unfinished attached Work, the change is the workspace as it is now, new untracked files
    included, as `finish` would take it. The Work's patch files are not touched, and an
    `Unfinished:` line says where this Δ was written.
  - It applies nothing and changes no verdict. One `coherence.merged_checks` event records the
    source state (Git HEAD and whether it had uncommitted changes, or a plain directory), the Δ
    digest, and each command's result and duration.
  - It exits 1 if a check fails or cannot run. Plain `dispatch check` still exits 0 for any
    verdict.
  - It needs the authority `finish` has: the Work's own `--allow-unsafe-local`, standing consent
    for exactly these checks (`dispatch setup --checks`), or `--allow-unsafe-local` on this check
    alone, which is not recorded on the Work.
  - Accept still runs its own merged checks, because the source can move after.
- **A digest that ties the review to what lands.** `finish`, `check` and `diff --stat` print
  `Δ <12 hex>`, the start of the SHA-256 of the patch bytes, and `check --json` adds
  `delta_digest`. `dispatch accept <id> --expect <digest>` (12 or more hex characters) refuses
  before anything else when the frozen Δ is not the one you reviewed:
  `error: the frozen Δ of <id> is 50c4c5a68351, not the expected Δ 000000000000; nothing was applied`.
  The full digest is recorded with the review (`review.accepted`, `review.rejected`).
- **Check results you can read.** `finish`, `check --merged` and `accept` print each check
  command with `PASS`, `FAIL`, `TIMED OUT` or `NOT RUN`, its exit code, its duration and its
  logs. A refused accept prints `Merged checks: did not pass` and the lines. Test-runner counts
  are not parsed, so they stay unknown.
- **Codex and Cursor Agent worktrees become Work.** In a watched project, discovery now
  recognizes them beside Claude Code's, on the same `git worktree list` path, each by the one
  reliable sign it leaves:
  - **Codex** (`codex exec --worktree`): wherever the worktree is, `codex-thread.json` in its
    Git metadata (`.git/worktrees/<id>/`). The session is its `ownerThreadId`.
  - **Cursor Agent** (`cursor-agent --worktree <name>`): exactly
    `~/.cursor/worktrees/<project folder, lowercased>/<name>`, on branch `<name>`. Cursor
    lowercases the folder name, so Dispatch does too, on macOS and Linux alike. One path is
    tested; `~/.cursor` is never walked.

  As for Claude, a worktree that appears after watching began becomes Work with S0 at its merge
  base with the project, and one that was already there is listed as found, not tracked. Neither
  agent records anything that names its running process, so their liveness is unknown, and
  Dispatch says so instead of guessing:

  ```text
  the end of a Codex session is not observable — when it is done, finish to freeze its changes (dispatch finish <id>); or close it: dispatch close <id> --reason unfinished
  ```

  Their Work never reads `agent running` and never turns idle by itself. `watch` shows the
  origin as `codex session in its own worktree, found by Dispatch; its end is not observable`.
- **Fixed: a session's end was lost when `finish` came first.** In 0.4.13, Work finished within
  a tick or two of its Claude session's exit kept that session open for good. Finished Work is
  now followed while it still records its agent process, and the end is recorded once that
  exact process is gone or its lock is released. Finished Work never takes on a new agent. The
  conformance trials found this.
- **Native adapters brought up to date.**
  - **Cursor:** the final `result` event, the model from `system/init`, and errors are parsed
    from the current `stream-json`. Cache reads are no longer dropped: input, output, cache
    reads and cache writes are kept as separate categories. The total is uncached input plus
    output plus cache creation, labelled `uncached-input+output+cache-creation`, the convention
    Codex's total already uses.
  - **Codex:** a rejected model now shows the provider's own message from `turn.failed`, also
    when the process exits non-zero. Cache-write and reasoning-output counters are recorded
    separately; the total is unchanged.
  - **Hermes Agent** (new): `dispatch run --agent hermes` runs `hermes -z <task>` with
    `--usage-file` and `--yolo`. `harnesses.hermes` in `dispatch.yml` takes `model`, `provider`,
    `executable` and `extra_args`; `dispatch doctor` reports Hermes. Usage comes from the
    per-attempt usage file, and null counts stay unknown. Its tests use Hermes's own failure
    output and a usage file shaped like its documented fields: the adapter has not yet run
    against a real successful Hermes session.
- **Harness conformance.** `scripts/harness-trial.sh <harness> <path>` runs one shared scenario
  through an agent's wrapped attach, native run or own worktree mode, with its own state
  directory, and writes a record and the evidence. [`docs/harness-conformance.md`](docs/harness-conformance.md)
  explains the scenario and holds the table. The runner adds no command, flag or output to
  Dispatch.
- **On a busy machine Dispatch no longer gives up early.**
  - A provider CLI's version or account probe may take up to 60 s, instead of 5 s. One that
    never answers is still unknown, and refuses only that launch.
  - `dispatch start` and `dispatch stop` wait up to 120 s for the background owner, instead of
    10 s. Their errors now read `did not become ready in 120 s` and `did not stop within 120 s`.
    An owner that exits is still reported at once.
  - Confirming that a stopped agent's process group is gone takes up to 10 s, instead of 500 ms.
  - A setup write waits up to 30 s for another writer, instead of refusing at once.
  - `dispatch watch` and the review screens no longer lose a key pressed as the terminal is
    resized, and they wait up to 60 s, not 2 s, for the terminal to report its cursor on a
    resize, so a busy machine no longer ends them.
- **For contributors.**
  - The suite runs in a pinned Linux image: `scripts/test-in-docker.sh` runs fmt, clippy and
    test in `ci/test.Dockerfile` as an ordinary user, with `--init` so orphans are reaped. The
    tests need only Rust, `git`, `python3` and `sh`; no `docker` CLI.
  - CI runs the suite on Linux (in that image) and macOS, plus `tests under load` on every pull
    request and push.
  - A release runs the suite on its tag before it builds any asset.
  - `stability.yml` runs the suite 20 times nightly on both platforms and lists each failing
    test with its count. `harness-trials.yml` runs real agents through the conformance runner,
    by hand or weekly and never on pull requests, with keys from a protected environment. Only
    the fixture reaches the model providers, sent by the agents; Dispatch uploads nothing.
  - `DISPATCH_DOCKER_STRESS=1 scripts/test-in-docker.sh cargo test --locked` runs the suite on a
    loaded machine (2 CPUs, 16 busy loops, 8 test threads). CI's `tests under load` job does
    the same on every pull request, and `stability.yml` adds one such run each night. CPU
    limits alone reproduced none of the timing failures, because the test harness then runs
    one test per CPU.

Not in 0.4.14: the end of a Codex or Cursor session is not observable, and there are no Codex or
Cursor hooks. Hermes's own worktrees are not recognized yet: its worktree mode could not be
observed, because no provider was configured. A Hermes native run under the Docker backend
cannot record usage, because the attempt's output directory is not mounted. There is still no
process scanning, and nothing is finished, closed or cleaned up automatically.

No migration: the schema stays at 24. JSON and events are additive: the
`coherence.merged_checks` event, `delta_digest` on `check --json` and on review events, and
Discovered Work with agent `codex` or `cursor`. "Upgrading to 0.4.14" in
`docs/release-install.md` has the details; restart the owner after upgrading.

Built with Dispatch: each packet's worker ran in a workspace Dispatch made. Claude Code built most
packets and Codex built the adapters. A Cursor Agent worker started the CI packet, stopped at its
plan's request limit, and Claude Code finished it in the same workspace. From P3 on, every packet
landed with `dispatch accept --expect`, in place of the integrator's `land.sh`, and
`dispatch check --merged` replaced its `verify.sh` for P3, P2, P6, P6b and R. Three times a
person accepted over a file-level REFRESH whose only broken fact was a file the patch named
without depending on it: the plan document, named in code comments (P2, P4), and `Cargo.lock`,
named as a CI cache key (P6b, after the version bump). All three are recorded as
`coherence.overridden`, with the person's explanation.

Conformance with this release: Claude Code 2.1.296, Codex 0.155.1 and Cursor Agent 2026.10.14
passed all three paths (attach, native, their own worktree mode) on 2026-10-15. Hermes Agent was
skipped on all three: no provider was configured. The table and evidence are in
[`docs/harness-conformance.md`](docs/harness-conformance.md).

Site impact: `dispatch check` gains `--merged` and `--allow-unsafe-local`; `dispatch accept` gains `--expect`; `finish`, `check` and `diff --stat` print a `Δ <digest>` line and `check --json` adds `delta_digest`; `finish`, `check --merged` and `accept` print one line per check command; `--agent hermes` and a `harnesses.hermes` config section; doctor lists Hermes; Codex and Cursor Agent worktrees become Work in a watched project (new `status` hint, `watch` origin and owner-log lines), so "only Claude Code's worktrees are recognized" no longer holds; the `start`/`stop` errors say 120 s instead of 10 s; Cursor's token total and semantics change.
