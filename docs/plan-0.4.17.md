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

Built with workers wrapped by the pinned Dispatch 0.4.16 (`fa11caf`, state
`~/dispatch-dev/state-0417`). Every packet landed with `accept --expect`, CI validated each
landing on draft PR #27, and no coherence override was needed. Beyond the plan:
- P8 added distribution on four platforms and download counts, which the user asked for during
  the build.
- P7 and P9 fixed intermittent failures, by the user's standard that every CI job passes.
- Q fixed the findings of the line-by-line review of the packets Codex and Hermes Agent built,
  by the user's rule.
- Two REFRESH verdicts were re-applied 3-way (P5b, Rb), and P2 was re-applied twice (P2b with
  the review's fixes, P2c on top of P4).

### P0 (2026-10-21)

The Claude Code probe, the native start profile, the first-use and upgrade baselines and the
override fixtures in §2, with their scripts in `~/dispatch-dev/p0-0417/`.

### Packets, in landing order

| Packet | Worker | Work | Finish | Landed |
|---|---|---|---|---|
| P6 native start, part 2 | Hermes Agent 0.18.2 | `01M5DT6FZAF9Q802PWSA2P2CPP` | CONTINUE, 800 passed | `33dc805`, `--expect 5eb57359cde0`; source unmoved, no rerun |
| P1 install and stay running | Claude Code | `01M5DT6FZBKNEP7FW7P9TQKH7S` | CONTINUE, 813 passed on the current source | `010f748`, `--expect ee14cda7e4d5`; reused that pass |
| P5 mentions | Codex 0.155.1, gpt-6-sol | `01M5DT6FZARGWDXMPV1RN4TJEB` | REFRESH: a path its test code named (`src/main.rs`) was a strong fact under 0.4.16; 824 passed | not landed: superseded by P5b |
| P5b P5's Δ re-applied | the integrator, no agent | `01M5F6F8NQAF4NF13VKGVBTA7G` | CONTINUE; reused P5's pass | `a5fa0d8`, `--expect cfe714569fe3`; source unmoved |
| P3 session end and resume | Claude Code | `01M5E6NR88VAKVDS9X101H257M` | CONTINUE; the merged run failed on its own race, S0 passed | `4a55522`, `--expect 4989c912297f`; merged checks rerun (834 passed) |
| P4 needs you | Claude Code | `01M5FG4XZ2Z79CNGBASK61FW7V` | CONTINUE, 845 passed | `aeb2ac8`, `--expect 12c8afa471db`; source unmoved |
| P8 four platforms, download counts | Claude Code | `01M5FH5A8DXPHK0F9GAKWQCN7Z` | CONTINUE, 851 passed on the current source | `12fe021`, `--expect aa78089e3efd`; waited 710 s for the source lock, then reused that pass |
| P7 session races under load | Claude Code | `01M5FG4XZ23R2S986H45H2EN59` | CONTINUE, 856 passed on the current source | `ba0fafa`, `--expect b2ccb9ce5d85`; reused that pass |
| Q review fixes, test-code mentions | Claude Code | `01M5FMB91RGBDAAPPSE24FDEPN` | CONTINUE; merged run 806/4 and S0 795/4, on P3's races | `5c2b0b2`, `--expect c64163483e30`; merged checks on top of P7 (864 passed) |
| P2 one setup pass | Codex 0.155.1, gpt-6-sol | `01M5DT6FZBJ02N96WHXBGD0FVZ` | REFRESH: its patch no longer applied | not landed: superseded by P2c |
| P2b P2 with the review's fixes | Claude Code | `01M5FHGP8KYZ9KDTWGGDWEJRTB` | REFRESH: its patch no longer applied after P4 | not landed: superseded by P2c |
| P2c P2b on top of P4, hook update | Claude Code | `01M5FNBX912MDZ6P586Q9JJMRD` | CONTINUE, 872 passed on the current source | `f12c60c`, `--expect 61751df9031b`; reused that pass |
| P9 the remaining intermittent failures | Claude Code | `01M5FZE4M22CZ9XMP8F7Q0219E` | CONTINUE, 876 passed | `c4780dc`, `--expect 3ec4c582a0f3`; source unmoved |
| R docs, version | Claude Code | `01M5FR1GSH2FDC34FD14NH1Z4E` | REFRESH: README.md changed under it (P2c); 876 passed on the current source | not landed: superseded by Rb |
| Rb R's Δ re-applied | the integrator, no agent | `01M5GAPEJDVFBFNZBRRC3Q0F51` | CONTINUE; reused R's pass | `076904d`, `--expect 13764678c456`; source unmoved |

- **The review of non-Claude work** (`~/dispatch-dev/notes-0417/review-non-claude.md`):
  - P6: 3 findings; P5: 5; P2: 7.
  - All were fixed by Claude Code, in Q and P2b, and kept in P2c.
  - All were verified in the code before landing.
- **Defects found before they landed:**
  - **R, checking every claim on a combined build:**
    - a typed `attach` or `finish` under standing consent recorded no authority, so accept
      applied on a moved source without the merged checks;
    - setup left `dispatch.setup-lock` in the project, so a first Work reported it as a
      difference from S0.
    - P2c fixed both, with tests that fail on the code before.
  - **P2c:** 0.4.16's four Claude Code hooks would have read as "changed by hand" once P4's
    six were current, so setup would never have updated them.
- **Intermittent failures** (the standard: a flaky test is a defect). Counts are from CI's
  stress image, whole test binaries:
  - **P7:**
    - a tick acting on a record loaded before Hermes's hook recorded its removal;
    - a worktree being deleted read as unlocked;
    - a tick holding the run lock read as a finish in progress;
    - discovery reopening before the `SessionStart` hook;
    - four test-only races.
    - `session_end` failed 12 of 12 runs on the old code and 0 of 30 after; `runtime_hooks`
      2 of 12, then 0 of 30.
  - **P9:**
    - the hook's join and accept waited a fixed 5 s for a run lock that legitimate holders
      exceed under load;
    - a stop held back by the mid-run message gap (60.9 s, now about 1 s);
    - a Hermes worktree listed before its lock;
    - `start` after its owner died;
    - three test races its loops found.
    - `discovery` failed 1 of 51 runs, then 0 of 30; `coherence_watch` 2 of 45, then 0 of 30;
      `needs_you` 2 of 10, then 0 of 20.
- **CI on PR #27:**
  - the stress job failed on `a5fa0d8`, `4a55522` and `aeb2ac8`, all on races P7 fixed;
  - every job was green on `fde3972`, `33dc805`, `010f748`, `12fe021` and on every commit
    from `ba0fafa` on;
  - the release build matrix was green on all four runners from `12fe021` on.

### Workers, as their harnesses reported

- **Codex (P5):** 3h37m; 375,652 tokens: input 337,897 (+19,915,776 cached), output 37,755
  (reasoning 17,093).
- **Codex (P2):** 774,663 tokens: input 733,243 (+17,842,944 cached), output 41,420 (reasoning
  19,108). "Worked for 16h 2m 59s", wall clock with approvals and idle time.
- **Hermes Agent (P6):** 2h29m, 176 tool calls; tokens not shown.
- **Claude Code** (P1, P2b, P2c, P3, P4, P7, P8, P9, Q, R): usage not reported at exit.

### Against §6

- **First use:** the install line and 2 Dispatch commands (`setup`, `accept`) to a first
  landed change. Measured end to end on a release build, with a stand-in for Claude Code.
  Met.
- **Upgrade:** 0.4.16 upgraded in place through `install.sh`. The next command printed
  `Restarted watching … with Dispatch 0.4.17 (was 0.4.16).`. Met.
- **Overrides:** none. The three 0.4.14 patterns no longer REFRESH, and the matrix passes on
  both source kinds. Met.
- **Sessions:** met.
  - A tick's p50 was 47 ms without a consent finish and 43 ms with one running (P3; poll 2 s,
    7 Works).
  - No resumed Work's S0 holds unreviewed Δ (P3's and P7's tests).
- **Native start:** not met.
  - 10.8 s median against the 10 s target: 10.6, 10.8 and 11.0 s alone, at load 7.3 to 18.2.
  - 0.4.16 took a 45.7 s median at the same load, so this is about 4 times faster.
  - P6's profile puts the rest in the opens the fingerprints still make.
- **Suite runs:** 17 for 11 landed Works (1.55), with 6 reuses. Met.
- **Orphans:** an independent full run on the final tree passed 876, 0 failed, and left the
  owner list as it was (16 before and after, none from the run). Met.

## Carried, not in this release

- Native start's remaining 0.8 s over target: the fingerprints still open each file they hash
  (P6's profile).
- `record_session_end` and `record_needs_you` wait 3 s for the run's lock inside Claude Code's
  5 s hook timeouts (P9's open question 1).
- A second `SessionStart` in one worktree can give up while the first waits for busy Work
  (P9's open question 2).
- `EARLIER` holds one earlier hook set; the next release that changes the set makes it a list
  (P2c).
- `f` in `watch` still asks before a finish under consent (P2c).
- The `ubuntu-22.04` runner deprecation and the glibc floor (P8): a decision for the user.
- A guard for a capture that overlaps a removal (0.4.15 P5's note).
- Ties between same-second modification times in `freshest_patch`.

The 0.4.15 stress failure in `hermes_worktrees`, carried since, was found and fixed (P7).
