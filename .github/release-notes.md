## Dispatch 0.4.17 — Out of the box

Dispatch should work out of the box: install it in one line, set a project up in one pass, and
let agents work with nothing to do by hand until you review. In 0.4.16, getting to a first
landed change took a manual download, `dispatch setup` and its Runtime integrations menu,
`dispatch setup --checks` twice, `dispatch start`, a `dispatch finish` after any session
that kept its worktree, and `dispatch accept`. An upgrade needed `dispatch stop && dispatch start`, a typed `attach`
needed `--allow-unsafe-local` even when you had already allowed the checks, and a file named
in a comment counted as a dependency. In 0.4.17 the path is the install line, `dispatch
setup`, your agent's session, and `dispatch accept`.

### Out of the box

- **Install in one line.**

  ```sh
  curl -fsSL https://rundispatch.sh/install | sh
  ```

  Until the website redirects that address, the same script is at
  `https://github.com/rundispatch/dispatch/releases/latest/download/install.sh`.
  - It picks this machine's archive from the release: `dispatch-macos-arm64.tar.gz` (Apple
    silicon), `dispatch-macos-x86_64.tar.gz` (Intel), `dispatch-linux-x86_64.tar.gz` or
    `dispatch-linux-aarch64.tar.gz`. A shell running under Rosetta gets the arm64 build. macOS
    on Intel and Linux on aarch64 are new; each archive is built and smoke-tested on its own
    architecture by CI, and macOS arm64 is the platform exercised interactively.
  - It verifies the archive against the release's `SHA256SUMS` and installs nothing on a
    mismatch, or when the release has no archive for this machine.
  - It puts `dispatch` in `~/.local/bin` (`DISPATCH_INSTALL_DIR` to choose another directory,
    `DISPATCH_VERSION=0.4.17` for a particular release), under a temporary name that is then
    renamed into place, so a running binary is never written into. Run it again to upgrade.
  - It never edits your shell's files. When the directory is not on `PATH`, it prints the
    line to add and where:

    ```text
    Installed dispatch 0.4.17 at /Users/you/.local/bin/dispatch

    /Users/you/.local/bin is not on your PATH. Add this line to ~/.zshrc, then open a new terminal:

      export PATH="/Users/you/.local/bin:$PATH"
    ```

  0.4.16 had no installer: you downloaded, verified and placed the binary by hand.
- **Download counts without telemetry.** The archives, `SHA256SUMS` and `install.sh` are
  GitHub release assets, so GitHub's own download counts measure installs: one install
  downloads each of its three files once. Dispatch itself sends nothing. A nightly job in this
  repository records those counts on its `stats` branch as CSV files.
- **Watching comes back.** A project watched with `dispatch start` is watched again by the
  next Dispatch command or hook there, after a reboot, a crash or an upgrade in place, until
  `dispatch stop`.
  - An owner that stopped is started again by the next Dispatch command in the project
    (`status`, `watch`, `attach`, `finish`, `check`, `accept`, `start`), which says, on
    stderr:

    ```text
    Watching /Users/you/src/project again: its owner had stopped.
    ```

    A Claude Code or Hermes session that starts there does the same, and notes it in the
    owner's log instead, since hook output reaches the model. The owner comes back with the
    `PATH`, `HOME`, `TMPDIR` and `LANG` it had under `dispatch start`. While `dispatch.yml`
    does not parse, nothing is started and the command says why (`Could not watch <root>
    again: …`).
  - After an in-place upgrade, the next command replaces the owner:

    ```text
    Restarted watching /Users/you/src/project with Dispatch 0.4.17 (was 0.4.16).
    ```

    0.4.16 printed `… it runs Dispatch 0.4.15, this is 0.4.16: dispatch stop && dispatch start`
    instead. Only an owner started from the same executable path is replaced, or an owner
    0.4.16 or earlier started, when this version is newer. An older binary or another install
    path never replaces an owner; `status` and `start` still name the version it runs.
  - The owner exits by itself when its project or Dispatch's state directory is deleted. It
    used to run until a signal: 14 owners left by interrupted 0.4.14 and 0.4.15 test runs on
    2026-10-14 and 15 were still running a week later on the machine that built this
    release, their projects deleted.
  - `status` in a project with no Work prints `No Work in <root> yet.` and the project line
    (who watches, consent) and exits 0; `status --json` prints `null` and `status --jsonl`
    nothing. 0.4.16 failed with `no Dispatch runs for <project>`.
- **One setup pass.** `dispatch setup` in a project, on a terminal, now covers everything in
  order, and each step can be skipped:
  1. **Checks:** the commands Dispatch detected, as a multi-select, plus "Other command…",
     saved to `dispatch.yml`. Focus is on "Continue without checks" (or "Keep current
     checks").
  2. **Consent:** the exact commands and what they may do; Allow or Cancel, with focus on
     Cancel.
  3. **Hooks:** for `claude` and `hermes` found on `PATH`, the exact hooks Dispatch adds;
     Install or Skip, with focus on Skip.
  4. **Watching** starts, unless it already runs.

  Provider accounts moved to a last row, "Agents Dispatch launches…". `setup --checks`,
  `--verify` and `setup claude|codex` work as before. Without a terminal, `setup` says
  `setup needs a terminal to choose project checks, allow them, and install hooks; use
  dispatch setup --checks --verify '<command>' to save exact checks without one`. 0.4.16 took
  `setup`, Runtime integrations, and `setup --checks` twice.
  - **Consent version 2.** Check consent now also covers typed `attach` and `finish`, and
    Work Dispatch finishes when its session ends. A consent given before 0.4.17 no longer
    holds and is asked again once: `status` and `watch` say `check consent no longer holds:
    it did not cover typed attach, finish, or session end`, and setup says why it asks.
  - **Typed `attach` and `finish`** need no `--allow-unsafe-local` when consent holds for
    exactly the project's checks. They record it on the Work (`attach.authorized {by:
    "project consent"}`), so accept runs the merged checks as it does for the flag. Without
    consent they refuse with `finish runs your
    checks.verify on the host; pass --allow-unsafe-local, or give this project standing
    consent for these checks with dispatch setup`.
  - **Native `run` and `refresh`** ask `[1] This run only  [2] Allow for this project
    [Enter] Cancel:` instead of `[y/N]`. "Allow for this project" is standing consent for
    exactly the configured agent command and checks, given only on a terminal; a change to
    either asks again. A piped `1`, `y` or `yes` still allows that one run, and a piped `2`
    is refused with `allowing for this project needs a terminal, so nothing was granted;
    answer 1 for this run only, or pass --allow-unsafe-local`. The flag works as before.
  - **Hooks from an earlier Dispatch** (0.4.6 to 0.4.16 installed four) are offered as an
    Update to the six 0.4.17 installs, `Notification` and `Stop` added, showing the hooks they
    become (`Hook command, unchanged: …` after an upgrade in place). Hooks you edited are left
    alone and reported as `changed by hand`.
- **Work finishes when its session ends** (Claude Code and Hermes). With consent, the owner
  finishes Work a runtime registered once its session has ended and its process is gone,
  judged by the worktree's lock: a closed terminal leaves the lock behind, naming a pid that
  no longer runs. It is 0.4.16's finish, run as a task of its own, so other Work's verdicts
  keep moving while its checks run, and it never applies anything. `/clear` and `/resume` end
  a session in a process that keeps running, so they finish nothing. 0.4.16 finished only
  when a runtime removed the worktree; otherwise you ran `dispatch finish`.
  - **A resumed session goes on from the same S0.** When a session starts again in a
    worktree whose Work is finished and unreviewed, or still being finished, the new Work
    begins from that Work's S0, so its Δ holds both sessions' changes. The earlier Work closes
    as `superseded` by it, its Δ kept, and a finish still running is cancelled first. If that
    S0 can no longer be read, the session starts Work of its own and the earlier Work stays
    for review.
  - **The hook names the Work** whichever sees the session first, the hook or the owner's
    discovery: `Dispatch is tracking this worktree as Work <id>; see it with dispatch watch.`
    0.4.16 replied nothing when discovery had registered it first.
  - While the owner finishes Work, a typed `finish` or `close` of it says `Work <id> is being
    finished by its project's owner, as you allowed (checks running); wait for it, or close
    it once it is ready`.
  - Codex and Cursor send no session end, so their Work is still finished by you.
- **Dispatch says when an agent is waiting on you** (Claude Code). Two new hooks,
  `Notification` and `Stop`, mark the Work: `waiting on you: permission · Claude needs your
  permission`, or `input`, or `turn ended`, for a turn that ended and waits for your next
  prompt.
  - `dispatch watch` lists it under "needs you", `dispatch status` prints the line with how
    long it has waited, and `watch --json` carries `needs_you: {kind, message, since}` or
    `null`.
  - It clears when the session moves again: Claude writes its transcript after the mark (for
    a turn's end, more than 10 s after it), another hook arrives, the session ends, or the
    Work is finished.
  - The hooks never reply, so they never keep Claude working.
  - When a wrapped agent exits, `dispatch attach -- <agent>` prints finish's verdict and next
    step, `Attached work <id> finished: Ready for review` with `Coherence`, `At accept` and
    `dispatch accept <id>` / `dispatch reject <id>`, instead of `Attached work <id> finished;
    next: dispatch check <id>`.
- **A file named only in a comment is not a dependency.**
  - A path in a Rust or Python comment, doc comment or docstring makes no fact.
  - A path in test code (Rust `#[cfg(test)]` items, `.rs` files under `tests/`, Python
    `test_*.py`, `*_test.py` or files under `tests/`), or in a file without symbol support
    (YAML, Markdown, TOML, shell), makes a weak fact. When it breaks, the verdict stays
    CONTINUE with the reason recorded, and the merged checks decide at accept:

    ```text
    Coherence: CONTINUE — mentioned file Cargo.lock changed since the work started; the merged checks decide at accept
    ```

  - When no checks can run, it is REFRESH, saying why first: `analysis_uncertain: merged
    checks cannot run — no checks are configured (checks.verify is empty)`, then
    `mention_changed: …`.
  - A path that library code uses, in a string or `include_str!`, stays a strong fact.

  These are the three patterns behind 0.4.14's overrides: a plan document named in Rust
  comments (twice), and `Cargo.lock` named in `hashFiles('Cargo.lock')` in a workflow.
- **Native runs clone files by path on macOS.** The agent's workspace is copied with
  `clonefile(2)` by path, so a file is never opened just to copy it; on any error, Dispatch
  copies it as before. Linux, the snapshot's contents and its checks are unchanged. The cause
  was endpoint security (here Microsoft Defender), which intercepts every `open()`.
  On a Git repository with a 3.6 GB ignored `target/` (9,650 files), invocation to agent,
  alternating builds on one Mac with Defender on:

  | Run | 0.4.16 | 0.4.17 | Load average (1, 5, 15 min) at 0.4.16's / 0.4.17's |
  |---|---:|---:|---|
  | 1 | 138.1 s | 27.6 s | 71.3, 35.1, 20.5 / 74.6, 55.7, 31.6 |
  | 2 | 85.9 s | 16.5 s | 61.3, 56.5, 34.1 / 59.2, 59.9, 38.0 |
  | 3 | 56.9 s | 19.1 s | 53.2, 58.5, 38.0 / 36.0, 52.3, 37.2 |

  Medians 85.9 s and 19.1 s, at loads far above ordinary. Measured earlier at lower
  load, 0.4.16 took 47.6 to 74.7 s (1-minute load 6.1 to 18.2). In a profile of one
  0.4.17 start, the source fingerprints, which still open each file, took about as many
  samples opening files as the copy took in all (2,769 and 2,624).

  MEASURE: native start with 0.4.17 on the same repository at ordinary load (runs, seconds, load averages)

### Fixes found under load

- A Hermes session's end is recorded when its process exits, even when the owner looks at the
  moment Hermes removes its worktree. Under load it could end early as `lock_released` while
  Hermes still ran.
- A session resumed while the owner is busy with its Work for a moment joins that Work. Only
  the owner's own finish by consent makes it start Work over the old one.
- When the owner's discovery sees a resumed session before its `SessionStart` hook and could
  not reopen the old Work, the new Work starts from the worktree as found, as the hook does.
  Before, it started from the merge base, so the old change was in both Works' Δ.
- A session that starts while its Work is busy (Dispatch recording it, or your `dispatch
  finish` running the checks) waits for it, within the hook's 40 s budget, instead of failing
  after 5 seconds. If the finish ended meanwhile, the session goes on from that result.
- `dispatch accept` and `reject` wait while another process holds the Work, saying `Waiting
  for Work <id>: another process is using it.`, instead of refusing after 5 seconds on a busy
  machine.
- With `mid_run: stop`, a stop is never held back by the one-message-a-minute limit.
- A Hermes worktree is tracked even when Hermes locks it more than 3 seconds after making it.
- `dispatch start` right after its owner died says `✓ Watching`, as after any stopped owner.

### Not in 0.4.17

- 3-way or batch accept (0.5.0).
- Notes to agents (0.4.18), and replies from the `Stop` hook (0.5.0's catch-up).
- launchd or systemd agents: owners come back when a Dispatch command or hook runs.
- Edits to dotfiles.
- Project-wide auto-apply.
- Automatic `dispatch clean`.
- Session ends for Codex and Cursor, which send none.
- Cloning whole directories.

No migration: the schema stays at 24. The watcher record becomes version 2 (`exe` and
`environment`), the check consent version 2 (asked again once), and native runs gain a
standing consent record. New events: `runtime.needs_you`, `runtime.needs_you_cleared`,
`work.reopened`, `work.not_reopened`; `work.closed` can have `by: "reopen"`. New values: the
fact origin `mentioned` and the reason `mention_changed`. "Upgrading to 0.4.17" in
`docs/release-install.md` has the details. There is no `dispatch stop && dispatch start` after
upgrading: the next command restarts the owner.

### Measurements

Counted from recorded events and logs:
- MEASURE: suite runs per landed Work for this build, with `checks.reused` counted (gate: at most 1.75)
- MEASURE: overrides on this build (`accept --despite-refresh`), and whether the three 0.4.14 patterns still REFRESH
- MEASURE: native start at ordinary load (contract target: at most 10 s on this repository)
- MEASURE: first use, install to a first landed change on the first-use fixture, in Dispatch commands (gate: at most 3; 0.4.16: 6 plus a manual install)

A reused result is counted as reused, never as time saved.

Site impact: install is `curl -fsSL https://rundispatch.sh/install | sh` (or `curl -fsSL https://github.com/rundispatch/dispatch/releases/latest/download/install.sh | sh`) into `~/.local/bin`, with `DISPATCH_INSTALL_DIR` and `DISPATCH_VERSION`, printing `Installed dispatch <version> at <path>` and, off `PATH`, the `export PATH=…` line to add; releases add `dispatch-macos-x86_64.tar.gz` and `dispatch-linux-aarch64.tar.gz` and `install.sh` as assets; the quick start is install, `dispatch setup`, work, `dispatch accept`; bare `dispatch setup` on a terminal is one pass (Project checks multi-select with `Other command…`, `Save selected checks`, `Continue without checks` or `Keep current checks`, `Cancel setup`; `Allow checks for this project?` with Allow/Cancel and a new consent text; `Claude Code hooks · Install` or `· Update` with Install/Skip; `Project setup is ready. Agents Dispatch launches can be configured separately.` with `Done` / `Agents Dispatch launches…`; then watching starts) and provider accounts moved under `Agents Dispatch launches…`; `dispatch setup` without a terminal fails with `setup needs a terminal to choose project checks, allow them, and install hooks; use dispatch setup --checks --verify '<command>' to save exact checks without one`; check consent from 0.4.16 or earlier no longer holds (`check consent no longer holds: it did not cover typed attach, finish, or session end`) and setup asks again saying `The earlier consent no longer holds: …`; typed `attach` and `finish` need no `--allow-unsafe-local` when consent holds (recorded as `attach.authorized` by project consent, so accept runs merged checks), and their refusal now ends `pass --allow-unsafe-local, or give this project standing consent for these checks with dispatch setup`; `finish --allow-unsafe-local` and `refresh --allow-unsafe-local` help changed; native `run` and `refresh` prompt `[1] This run only  [2] Allow for this project  [Enter] Cancel:` instead of `[y/N]`, and a piped `2` fails with `allowing for this project needs a terminal, so nothing was granted; answer 1 for this run only, or pass --allow-unsafe-local`; Claude Code hooks are six (`Notification` matched to `permission_prompt|idle_prompt|agent_needs_input` and `Stop`, 5 s each, added), the Update prompt shows `Hook command, unchanged: …` and `Dispatch's hooks there are replaced by exactly these:`, a hand-edited hook reads `Claude Code hooks: changed by hand in <path>; setup leaves that file as it is.`, and Remove says `Only Dispatch's own hooks are removed, hook by hook`; commands print `Watching <root> again: its owner had stopped.`, `Restarted watching <root> with Dispatch <new> (was <old>).`, `Could not watch <root> again: …` or `Could not restart watching <root> with Dispatch <new>: …`, and `start` no longer advises `dispatch stop && dispatch start` after an upgrade in place; the owner exits when its project or state directory is deleted; `status` with no Work prints `No Work in <root> yet.` and the `Project:` line, `status --json` prints `null` and `status --jsonl` nothing; with consent, Work a Claude Code or Hermes session registered is finished by the owner when the session ends and its process is gone, a resumed session's Work starts from the same S0 and the old Work is closed as `superseded by <id>`, and a typed `finish` or `close` during that finish says `Work <id> is being finished by its project's owner, as you allowed (checks running); wait for it, or close it once it is ready`; the `SessionStart` reply `Dispatch is tracking this worktree as Work <id>; see it with dispatch watch.` now also appears when discovery saw the session first; `status` and `watch` show `waiting on you: <permission|input|turn ended> · <message>`, `watch` has a needs-you group with state `waiting` and the next steps `Its session asks for your permission: answer it there.` or `Its session waits for your next prompt: continue it there, or f finish when it is done.`, and `watch --json` work lines gain `needs_you`; a wrapped `attach` prints `Attached work <id> finished: <heading>` with `Coherence`, `At accept`, `Review` and `Then` instead of `next: dispatch check <id>`; a path named only in a Rust or Python comment or docstring makes no fact, one in test code or a file without symbol support is weak (`mention_changed: mentioned file <path> changed since the work started`, `Coherence: CONTINUE — …; the merged checks decide at accept`, or `…; the merged checks passed`), and without runnable checks it is REFRESH with `analysis_uncertain: merged checks cannot run — <why>` first; `dispatch accept` and `reject` of a result another process holds wait, first printing `Waiting for Work <id>: another process is using it.` on stderr.
