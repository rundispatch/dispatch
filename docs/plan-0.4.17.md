# Plan: Dispatch 0.4.17, out of the box

**Draft, 2026-10-21.** Base: `fa11caf` (Dispatch 0.4.16). Second step on the road to 0.5.0
(`docs/roadmap-0.5.0.md`). Built as 0.4.16 was: workers wrapped by the pinned released
Dispatch, here 0.4.16, which brings the verification memo to its first build. Packets are
landed with `accept --expect`, and CI validates each landing on a draft PR.

## 1. Objective

Dispatch works out of the box: from install to a first landed change in at most three
Dispatch commands, with nothing to do by hand while agents work.

- **Install and upgrade:** install in one line, and upgrade in place without restarting
  anything.
- **Setup:** one pass covers checks, consent, hooks and watching.
- **Authority:** standing consent covers exactly the commands it names, wherever Dispatch runs
  them.
- **Sessions:** with consent, Work finishes by itself when its session ends, and opens again
  when the session resumes.
- **Needs you:** Dispatch says when an agent is waiting on you.
- **Mentions:** a file named only in a comment is not a dependency.
- **Native start:** native runs start in seconds, even next to gigabytes of build output.

## 2. Evidence (P0, 2026-10-21)

The scripts and raw output are in `~/dispatch-dev/p0-0417/`.

### First use and upgrades today

Read from the code at `fa11caf`, and observed where noted. From install to a first landed
change, Dispatch's own path is:
1. download, verify and place the binary by hand;
2. `dispatch setup`, then Runtime integrations, to install Claude's hooks;
3. `dispatch setup --checks`, which picks one detected command and returns;
4. `dispatch setup --checks` again, to let Work run it by itself;
5. `dispatch start`;
6. after the session ends, `dispatch finish <id>`, since nothing finishes on a session end;
7. `dispatch accept`.

That is six Dispatch commands plus a manual install.

- **Upgrading in place (observed).** With an owner started by 0.4.15, 0.4.16's `start` says:

  ```
  Already watching …: watched in the background since 17:20 (pid 81093); it runs Dispatch 0.4.15, this is 0.4.16: dispatch stop && dispatch start
  ```

  `status` in a project with no runs fails with `no Dispatch runs for <project>`.
- **Authority.** A typed `attach` refuses without `--allow-unsafe-local` even when standing
  consent covers the checks. `check --merged` already accepts that consent. Native runs ask
  `[y/N]` each time, and `refresh` needs the flag again. README's `attach --auto-apply -- claude
  -p …` example can never auto-apply: with checks, attach refuses without the flag, and
  without checks, auto-apply is never eligible.
- **Orphaned owners.** After 0.4.16, 14 `serve --background` owners from interrupted test runs
  were still running, 3 to 4 days later, with their projects deleted. The owner loop ends only
  on a signal.
- **Downloads.** Files fetched with `curl` or `gh` carry `com.apple.provenance`, not
  `com.apple.quarantine`. The unsigned binary runs; only a browser download would need
  `xattr -d com.apple.quarantine`.

### Claude Code's session hooks (user-approved probe)

Claude Code 2.1.296, in a scratch repo with logging hooks in its `.claude/settings.local.json`
(`claude-probe/`, 17 payloads, `FINDINGS.md`):

| What happened | `SessionEnd` reason | cwd | Worktree lock at hook time | Process |
|---|---|---|---|---|
| `/exit`, or Ctrl-C twice then "Keep worktree" | `prompt_input_exit` | main checkout | already released | exits right after the hook |
| `/clear` | `clear` | the worktree | held | keeps running, new `session_id` (`SessionStart` `source: clear`) |
| `/resume`, switching to another conversation | `resume` | the worktree | held | keeps running (`SessionStart` `source: resume`) |
| Terminal closed | `other` | the worktree | **left behind**, naming a gone pid | gone |

- **Resuming re-locks.** `claude --worktree <name> --resume <id>` takes the worktree's lock
  again, with the new process, before `SessionStart` (`source: resume`, same `session_id`).
- **`/resume` across worktrees.** Picking a conversation from another worktree continues it in
  the current one. At exit, Claude then offers to keep or *remove that other worktree*,
  unlocks it, and leaves the current one locked.
- **Notification** carries `notification_type` and `message`: `permission_prompt` ("Claude
  needs your permission") about 7 s after the dialog, and `idle_prompt` ("Claude is waiting for
  your input") 60 s after the turn ended. No hook fires when a permission is granted; the next
  events are `PostToolUse` and `Stop`.
- **Stop** carries `stop_hook_active`, `last_assistant_message`, `background_tasks` and
  `permission_mode`.

### Mentions

0.4.14's three overrides (from that build's events):
- two came from `docs/plan-0.4.14.md` named only in Rust comments and doc comments;
- one came from `Cargo.lock` named only as `hashFiles('Cargo.lock')` in workflow YAML.

`include_str!("../schema/user.json")` in coherence-matrix scenario 08 is code, not a mention,
and stays REFRESH.

### Native start

Pinned 0.4.16 on a Git repository (345 tracked files) with an ignored `target/` of 3.6 GB
(9,650 files). The agent is a stand-in that stamps its start.

| Run | Invocation to agent | Load average (1 min) |
|---|---:|---:|
| 1 | 74.7 s | 6.1 |
| 2 | 48.1 s | 12.2 |
| 3 | 47.6 s | 18.2 |

- **The run's events:** `baseline.finished` to `attempt.started`, the agent's workspace being
  made, takes 37–39 s. Another 6–20 s come before `run.created`, in the source snapshot.
- **A `sample` of an unstripped release build** during that gap:
  - 18,094 of 18,965 samples are in `source::copy_tree_contents`;
  - 16,450 are in `open()` inside `std::fs::copy`;
  - only 1,600 are in `fclonefileat`.
- **The cause:** Microsoft Defender's real-time protection is on, and it intercepts `open()`.
  Opening and closing the 9,650 files alone takes 11.2 s.
- **The same tree copied with `cp -c -R`,** by path with `clonefile(2)` and no `open`, takes
  4.3 s.
- A walk of every file takes 0.24 s, and hashing the non-ignored files 0.27 s.

The bytes and the hashing cost little; the per-file `open` before each clone, done twice, is
what native start pays. Endpoint security is common on company Macs.

## 3. Contracts

### A. Install and stay running (P1)

- **`scripts/install.sh`** (POSIX `sh`), uploaded by `release.yml` as a release asset:
  - It supports macos-arm64 and linux-x86_64, and refuses anything else, saying so.
  - It downloads the archive and `SHA256SUMS` from `releases/latest/download`, or from the
    version in `DISPATCH_VERSION`. Tests point `DISPATCH_RELEASE_URL` at a local directory.
  - It verifies the checksum and extracts into a temporary directory.
  - It moves `dispatch` into `~/.local/bin` (or `DISPATCH_INSTALL_DIR`) under a temporary name
    in that directory, then renames it into place. It never writes into an existing binary,
    which macOS would kill.
  - It prints `dispatch version`. When the directory is not on `PATH`, it prints the exact line
    to add and never edits a dotfile.
- **Owner record version 2** gains `exe`, the binary that started the owner. Version 1 records
  are still read.
- **Bringing an owner back:** `background::ensure_current(state, root)`.
  - **A record with a free serve lock:** the owner died without `stop` (reboot, crash). Start it
    again in the background.
  - **A held lock and another version:** stop the owner and start this one, but only when the
    record's `exe` is this binary (upgraded in place), or, for a version 1 record, when this
    version is newer. An older binary, or another install path, never replaces an owner.
  - **One line** on stderr: `Watching <root> again: its owner had stopped.` or `Restarted
    watching <root> with Dispatch 0.4.17 (was 0.4.16).`
  - **Who calls it:** `start`, `status`, `watch`, `attach`, `finish`, `check` and `accept`.
  - **Hooks** call a variant that spawns and returns without waiting for discovery. It writes
    its line to the owner log, never to stdout: `SessionStart` stdout reaches the model.
- **The owner exits when its project or its state directory no longer exists.** It checks with
  two `stat` calls per tick, logs it, and removes its record.
- **`status` with no runs** prints the project line (watching, consent) and says there is no
  Work yet, instead of an error.

### B. One setup pass, one authority (P2)

- **`dispatch setup` in a project, on a terminal,** is one pass. Each step can be skipped, and
  nothing is granted without a deliberate choice:
  1. Checks: the detected commands as a multi-select, plus "Other command…", saved to
     `dispatch.yml`.
  2. Consent: the exact commands and `consent::SCOPE`; Allow or Cancel, with focus on Cancel.
  3. Hooks: for `claude` and `hermes` found on `PATH` without Dispatch's hooks, the exact
     additions; Install or Skip, with focus on Skip. A hook that names another Dispatch
     executable is offered as Update, showing the exact change.
  4. Watching starts, unless it already runs.
- **What stays:** `setup --checks`, `--verify` and `setup claude|codex` keep working. Accounts
  move to a last row, "Agents Dispatch launches…".
- **Consent record version 2.** Its scope adds:
  - typed `attach` and `finish`, which run consented checks without `--allow-unsafe-local`;
  - the owner's finish when a session ends (contract C).

  Version 1 consents are asked again once, saying why.
- **Native launches (`run`, `refresh`)** offer "Allow for this project" next to "This run
  only". That consent is terminal-only and standing, for exactly the configured agent commands
  and checks. The flag still works.
- **README:** the auto-apply example works once consent holds, and says so. The quick start
  becomes: install, `dispatch setup`, `dispatch accept`.

### C. Session end and resume (P3)

- **`SessionEnd` only marks the Work,** within its 5 s timeout, as today.
- **The owner finishes Work by consent when all of these hold:**
  - its session ended, by hook or by liveness;
  - its process is gone, judged by the lock's pid and start time (a closed terminal leaves the
    lock behind);
  - consent holds;
  - the Work is Working and registered by Claude Code or Hermes.

  `/clear` and `/resume` keep the process alive and never trigger it. Nothing is applied.
- **The finish runs as its own task,** one per Work, with a `CancellationToken`
  (`executor::run_checks_with_config_and_cancel`). The tick loop does not wait for it. It is
  0.4.16's finish: frozen Δ and the memo first; checks in place when the source is unmoved,
  and in the check workspace when it moved.
- **Reopening.** A session starts again in a workspace whose latest Work from that runtime is
  finished and unreviewed. It is seen by `SessionStart` with `source: resume`, or by discovery
  finding a new lock.
  - The new Work takes the old Work's S0, so its Δ covers both.
  - The old Work closes as `superseded` by it, with its Δ kept.
  - If the old Work's finish is still running, the hook leaves a reopen marker, and the owner
    cancels the finish and closes the Work within a tick.
- **Attribution follows the workspace,** never `session_id`: `/resume` carries conversations
  across worktrees.
- **Codex and Cursor are unchanged:** they send no end signal.

### D. Needs you (P4)

- **New Claude hooks,** installed and removed by `setup`, with a 5 s timeout:
  - one `Notification` hook, reading `notification_type`: permission_prompt, idle_prompt,
    agent_needs_input, …;
  - `Stop`.

  Both only record `runtime.needs_you {kind, message, at}` and never reply.
- **"Waiting on you: <kind>"** shows in `watch` (the needs-you group), `status`, and `watch
  --json` (a `needs_you` field). It clears when the session moves again: the transcript's
  mtime passes the mark (one `stat` per waiting Work per tick), a later hook arrives, or the
  session ends. `Stop` means the turn ended: "waiting for your next prompt".
- **A wrapped agent's exit** prints finish's verdict and next step instead of `next: dispatch
  check <id>`.

### E. Mentions are not dependencies (P5)

- **Comments:** path tokens inside comments, doc comments and Python docstrings of Rust and
  Python files (tree-sitter, `coherence/symbols.rs`) make no fact.
- **Weak facts:** mentions in files without symbol support (YAML, Markdown, TOML, shell) make
  a weak fact, `FactOrigin::Mentioned`.
  - When it breaks, the verdict stays CONTINUE with the reason recorded, and the merged checks
    decide at accept.
  - It is REFRESH when no checks can run.
- **Strong facts stay strong:** string literals in code (`include_str!`, `open("x.json")`).

### F. Native start, part 2 (P6)

- **Copy files by path.** `copy_tree_contents` copies each file with `clonefile(2)` by path on
  macOS (`CLONE_NOFOLLOW`), keeping its mode, so the file is never opened. On any error it
  falls back to `std::fs::copy`, as today (another volume, a filesystem without clones).
  Linux is unchanged.
- **Fidelity is unchanged:** the walk, its filter, symlinks, directory permissions, the copied
  fingerprint and `snapshot_is_exact_private_and_excludes_state`.
- **Target:** invocation to agent at most 10 s with this repository and its 3.6 GB `target/`,
  at ordinary load on this Mac with Defender on (today 47.6–74.7 s).

### G. AGENTS.md (R)

New clauses under "What Dispatch does by itself":
- **Session end:** with standing check consent, a runtime-reported session end, once the
  session's process is gone, may freeze Δ and run the consented checks. It never applies. A
  resumed session continues from the same S0, and the unreviewed result is closed as
  superseded.
- **Standing consent** given in a terminal is the unsafe-local acknowledgement for exactly its
  commands.
- **Owner restart:** a project watched with `dispatch start` and never stopped is watched again
  by the next Dispatch command or hook there, after a reboot or an in-place upgrade. One line
  says so; `dispatch stop` ends it.

## 4. Packets

| Packet | Contract | Harness | Main files | Wave |
|---|---|---|---|---|
| P1 Install and stay running | A | Claude Code | `scripts/install.sh`, `.github/workflows/release.yml`, `orchestrator/background.rs`, `orchestrator/serve.rs` (exit), `runtime.rs` (hook restart), `main.rs` (callers), `presenter.rs` (status) | 1 |
| P2 One setup pass, one authority | B | Codex | `presenter/setup.rs`, `setup.rs`, `consent.rs`, `orchestrator/attach.rs` (`prepare`), `orchestrator.rs` (native consent), README example | 1 |
| P5 Mentions are not dependencies | E | Codex | `coherence/facts.rs`, `coherence/symbols.rs`, `models.rs`, `coherence.rs`, `tests/coherence_matrix.rs` | 1 |
| P6 Native start, part 2 | F | Hermes Agent | `source.rs` (`copy_tree_contents`) | 1 |
| P3 Session end and resume | C | Claude Code | `orchestrator/serve.rs`, `orchestrator/attach.rs`, `orchestrator/registration.rs`, `orchestrator/discovery.rs`, `orchestrator/liveness.rs` | 2, after P1 |
| P4 Needs you | D | Claude Code | `runtime/claude.rs`, `runtime.rs`, `orchestrator/attach.rs`, `orchestrator/serve.rs`, `presenter/watch.rs` | 3, after P3 |
| R Docs, AGENTS.md, version, release notes | G | Claude Code | docs, `AGENTS.md`, `Cargo.toml`, `.github/release-notes.md` | last |

## 5. Deliberately not built

- 3-way or batch accept (0.5.0).
- Notes to agents (0.4.18).
- Replies from the `Stop` hook (0.5.0's catch-up).
- launchd or systemd agents: owners come back lazily instead.
- Edits to dotfiles.
- Project-wide auto-apply.
- Automatic `clean`.
- Session ends for Codex and Cursor, which send none.
- Directory-level cloning.

## 6. Exit criteria (measured on this release's own build and fixtures)

- **First use:** install to a first landed change on the first-use fixture takes at most 3
  Dispatch commands (today 6, plus a manual install).
- **Upgrade:** upgrading from 0.4.16 in place needs no `stop && start`.
- **Overrides:** none on this build; the three 0.4.14 patterns no longer REFRESH; the coherence
  matrix passes on both source kinds.
- **Sessions:**
  - a consent finish never holds the owner's tick: its p50 is unchanged while one runs;
  - no resumed Work has an S0 containing unreviewed Δ.
- **Native start:** within contract F's target.
- **This build:** at most 1.75 suite runs per landed Work, with `checks.reused` counted. It is
  the memo's first real use.
- **Orphans:** no `serve --background` process outlives a full test run.

## 7. Execution log

(empty)

## Carried, not in this release

- A guard for a capture that overlaps a removal (0.4.15 P5's note).
- Ties between same-second modification times in `freshest_patch`.
- The 0.4.15 stress failure in `hermes_worktrees`, not seen since.
