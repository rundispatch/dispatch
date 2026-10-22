# Building Dispatch with Dispatch

This guide sets up the arrangement that built 0.4.12: coding agents, each in a
workspace Dispatch makes from the current integration source, supervised by a
pinned Dispatch release. You integrate their work one contribution at a time.
0.4.12 was built this way with Dispatch 0.4.11 as the supervisor. Earlier
releases, from 0.4.9, were built with Claude Code's own worktrees and Dispatch's
hooks, which section 5 keeps as an alternative.

0.4.13 was built the same way with Dispatch 0.4.12, but by four workers *at the
same time*. Each was an interactive Claude Code session wrapped with `attach --root
<clone> -- claude` (section 4), in auto mode. Their Work was still reviewed and
landed one at a time (section 7). Checking each against what had landed already
caught real integration problems: a test that named every field of a type another
worker extended, so the merged tree did not compile; a flaky test, whose failure made
the merged checks refuse an accept; and the owner's `git status` taking Git's
`index.lock`, so a test's own `git commit` failed under the load of parallel suites.
With parallel workers, run each worker's full suite once, at hand-off, rather than
continuously.

0.4.14 had workers on three harnesses, each wrapped the same way: Claude Code, Codex
(P3, the adapters) and Cursor Agent (P5, CI). The Cursor worker stopped at its plan's
request limit, and a Claude Code session finished P5 in the same workspace. From P3 on,
every packet landed with `dispatch accept <id> --expect <Δ>` (section 7) instead of the
integrator's `land.sh`, and `dispatch check <id> --merged` replaced its `verify.sh` for
P3, P2, P6, P6b and R. Under the load of several parallel suites on one Mac, and of other
jobs on it, tests and Dispatch itself gave up on fixed waits that were too short. That led
to P6, which lengthened the product's waits and made the tests wait for what they test.
It also led to P6b, whose stress mode found two more product defects: a key pressed during
a resize could be held back, and a resize could end `dispatch watch`.

The paths are examples. Keep each one outside `/tmp` and outside any build
directory, and use the same binary and state directory in every command.
`DISPATCH_HOME` can stand in for `--state-dir`, but the hooks in section 5 name
`--state-dir` explicitly, because Claude Code runs them with its own
environment.

## 1. Pin a released binary

The supervisor is a release, not something you build. A candidate build never
supervises the work that produces it.

```sh
mkdir -p ~/dispatch-dev/downloads/v0.4.11 && cd ~/dispatch-dev/downloads/v0.4.11
gh release download v0.4.11 -R rundispatch/dispatch \
  -p dispatch-macos-arm64.tar.gz -p SHA256SUMS
shasum -a 256 -c SHA256SUMS --ignore-missing     # dispatch-macos-arm64.tar.gz: OK
mkdir ~/dispatch-dev/dispatch-0.4.11
tar -xzf dispatch-macos-arm64.tar.gz -C ~/dispatch-dev/dispatch-0.4.11
cd ~/dispatch-dev/dispatch-0.4.11
jq -r .binary_sha256 BUILD.json
shasum -a 256 dispatch                           # must print the same hash
chmod a-w dispatch BUILD.json .
./dispatch version                               # dispatch 0.4.11
```

- **Two checks.** `SHA256SUMS` covers the archive, and `BUILD.json`'s
  `binary_sha256` covers the executable inside it. `BUILD.json`'s `head` names
  the source commit.
- **Where it lives.** Keep it outside the integration clone and outside every
  target directory, so that no build, `cargo clean` or `git clean` replaces or
  removes it.
- **Read-only.** The read-only file and directory stop anything from writing a
  different binary over it.

## 2. Use a dedicated state directory

```sh
mkdir -m 700 ~/dispatch-dev/state
```

- **Not your everyday state.** Keep the supervision state apart from
  `~/.dispatch`, and from any state a candidate build uses.
- **Not under `/tmp`.** macOS removes files in `/tmp` that have not been read
  for about three days. A state directory holds baselines, patches and logs
  that Work may not touch again for days. Losing some of them silently breaks
  that Work.

## 3. Make a dedicated integration clone

Accepted work is applied here. Use a separate clone on the release branch, not
the checkout you normally work in:

```sh
git clone git@github.com:rundispatch/dispatch.git ~/dispatch-dev/integration
cd ~/dispatch-dev/integration
git switch -c release-0.4.12 origin/main   # or `git switch release-0.4.12` if it exists upstream
printf 'dispatch.yml\n.claude/\n' >> .git/info/exclude
```

`dispatch.yml` and `.claude/` are your local setup, not part of the release.
Excluding them through `.git/info/exclude` keeps them out of every baseline
(S0), every patch (Δ) and every commit, without touching the tracked
`.gitignore`. `.claude/` also holds Claude Code's local settings and the
worktrees `claude --worktree` makes.

Write `dispatch.yml` with the project's checks:

```yaml
coherence:
  poll_secs: 5
checks:
  verify:
    - cargo fmt --check
    - cargo clippy --all-targets -- -D warnings
    - cargo test
```

These checks run on the host:
- `finish` runs them in the Work's workspace;
- `accept` runs them again on the merged tree when the clone has moved since
  the Work began.

Never share one target directory across these checks. With a 0.4.9
supervisor, use the namespaced form in section 8 instead.

Start the owner, which keeps every Work item checked against the clone as it
moves, and open the view:

```sh
cd ~/dispatch-dev/integration
~/dispatch-dev/dispatch-0.4.11/dispatch --state-dir ~/dispatch-dev/state start
~/dispatch-dev/dispatch-0.4.11/dispatch --state-dir ~/dispatch-dev/state watch
```

Leaving `watch` with Ctrl+C does not stop the owner.

## 4. Run workers in workspaces Dispatch makes

This is the recommended arrangement. Each worker starts from the clone's
current source, including accepted work you have not committed yet, and no
worker edits the clone itself.

From the integration clone, wrap the agent:

```sh
cd ~/dispatch-dev/integration
~/dispatch-dev/dispatch-0.4.11/dispatch --state-dir ~/dispatch-dev/state \
  attach --root ~/dispatch-dev/integration --task "<packet>" \
  --agent claude --allow-unsafe-local -- claude
```

- **The workspace.** Dispatch records the clone's exact world as S0, uncommitted
  and untracked files included, ignored ones not, and checks it out under
  `<state>/workspaces/<id>` on its own `dispatch/<id>` branch. The agent runs
  there; your clone is not touched.
- **Interactive.** The agent gets your terminal, so you read and approve its
  permission prompts yourself. Dispatch prints the workspace and branch before
  the agent starts, and nothing while it runs.
- **When it exits,** Dispatch freezes the patch, runs the checks in the
  workspace, and prints one line naming the Work's ID.
- **Any agent.** Replace `--agent claude -- claude` with another agent's label
  and command. The agent needs no worktree support of its own.
- **No `--worktree` inside it.** Do not ask the agent to make another worktree
  in Dispatch's workspace.

`--allow-unsafe-local` acknowledges that `checks.verify` runs on the host.
Nothing here is sandboxed.

**No Dispatch hooks in the clone.** Wrapped workers need none. With hooks, a
Claude Code session in the workspace would also be reported to Dispatch, join
the wrapped Work, and could show it idle while the wrapper still runs. Hooks
installed user-wide report to the state directory they name; as long as that
state does not watch this clone, they stay silent here.

**A permission allowlist, if you want fewer prompts.** Claude Code reads the
clone's `.claude/settings.local.json` in the workspace, which is a linked
worktree of the clone. Keep it to permissions only, with no hooks and no
`worktree` setting:

```json
{
  "permissions": {
    "allow": [
      "Bash(cargo fmt:*)", "Bash(cargo clippy:*)", "Bash(cargo test:*)",
      "Bash(cargo build:*)", "Bash(git status:*)", "Bash(git diff:*)",
      "Bash(git log:*)", "Bash(git show:*)", "Bash(rg:*)", "Bash(ls:*)",
      "Read(//Users/you/dispatch-dev/prompts/**)",
      "Write(//Users/you/dispatch-dev/handoffs/**)"
    ]
  }
}
```

Every command not on the list still asks. Keep the workers' instructions and
reports outside the clone, so they never enter a patch.

**Build caches.** Each workspace keeps the default `target/`, so set no
`CARGO_TARGET_DIR` for workers or checks:
- Dispatch's workspaces live under the state directory, outside the clone, so
  nothing that walks the clone walks them.
- `/target/` is ignored by the tracked `.gitignore`, so it never enters S0 or Δ.
- `finish` on an unmoved source runs the checks in the same workspace and reuses
  the worker's own build. Merged-tree checks, at accept and at a `finish` on a moved
  source, run in the project's check workspace under the state directory, which
  keeps its own `target/` between runs: the first builds from nothing, later ones
  incrementally. `dispatch clean` removes it.

A workspace is removed once its Work is applied. Otherwise it stays until
`dispatch clean`, which never removes one that unfinished Work, or Work waiting
for review, still uses.

## 5. Alternative: Claude Code worktrees with hooks

Each worker runs `claude --worktree <name>` from the clone, and Dispatch's
hooks turn each session into Work. Such a worktree starts from a commit, never
from uncommitted changes, so commit each accepted contribution (section 7,
step 6) before starting the next worker. With a 0.4.12 or later supervisor, a
worker that still began from older source is reported once, in `status` and
`watch`. With a 0.4.13 or later supervisor, a worktree made while the clone is
watched becomes Work without the hooks, from its merge base with the clone; the
hooks add an S0 taken at the session's start and the exact changes kept when
Claude Code removes the worktree.

`dispatch setup` installs Claude Code's hooks (four up to 0.4.16, six from 0.4.17,
which adds `Notification` and `Stop`) user-wide, in `~/.claude/settings.json`. They point at the binary that ran
setup and at its state. To build Dispatch with Dispatch, you want them in this
clone only, pointing at the pinned release and the state above. Write the same
four entries to `~/dispatch-dev/integration/.claude/settings.local.json`, with
your absolute paths:

```json
{
  "worktree": { "baseRef": "head" },
  "hooks": {
    "SessionStart": [
      { "hooks": [{ "type": "command", "timeout": 60,
        "command": "'/Users/you/dispatch-dev/dispatch-0.4.11/dispatch' --state-dir '/Users/you/dispatch-dev/state' hook claude" }] }
    ],
    "SessionEnd": [
      { "hooks": [{ "type": "command", "timeout": 5,
        "command": "'/Users/you/dispatch-dev/dispatch-0.4.11/dispatch' --state-dir '/Users/you/dispatch-dev/state' hook claude" }] }
    ],
    "WorktreeRemove": [
      { "hooks": [{ "type": "command", "timeout": 300,
        "command": "'/Users/you/dispatch-dev/dispatch-0.4.11/dispatch' --state-dir '/Users/you/dispatch-dev/state' hook claude" }] }
    ],
    "PreToolUse": [
      { "matcher": "ExitWorktree",
        "hooks": [{ "type": "command", "timeout": 300,
        "command": "'/Users/you/dispatch-dev/dispatch-0.4.11/dispatch' --state-dir '/Users/you/dispatch-dev/state' hook claude" }] }
    ]
  }
}
```

- **`"worktree": {"baseRef": "head"}`.** Without it, `claude --worktree` starts
  each worktree from the remote's default branch, not from the release branch
  you have checked out. With it, a worktree starts from the clone's local HEAD,
  so it carries local commits. Neither `baseRef: head` nor a commit copies
  uncommitted changes: accepted work reaches a new worktree only once it is
  committed.
- **Timeouts.** These are the ones `dispatch setup` installs.
- **One set of hooks.** Claude Code runs the hooks from every settings file.
  If Dispatch's hooks are also in `~/.claude/settings.json`, a session in this
  clone is reported to both binaries and both states.
- **Sessions.** A session in its own worktree opens with "Dispatch is tracking
  this worktree as Work `<id>`". A session started in the clone itself is told
  that Dispatch cannot tell its edits from yours and does not track them.

Each worker builds in its own target directory, outside the repository. Put
this in the worker's instructions:

```sh
export CARGO_TARGET_DIR=~/dispatch-dev/targets/<name>
```

Give every worker a new name. With a 0.4.9 supervisor, start the workers
before anyone builds (see section 9).

## 6. Test candidate builds with their own state

A candidate is built from the integration clone into its own target directory,
and runs only against a state directory of its own:

```sh
cd ~/dispatch-dev/integration
CARGO_TARGET_DIR=~/dispatch-dev/targets/candidate cargo build --release --locked
mkdir -p ~/dispatch-dev/candidate
cp ~/dispatch-dev/targets/candidate/release/dispatch ~/dispatch-dev/candidate/
mkdir -m 700 ~/dispatch-dev/state-candidate
~/dispatch-dev/candidate/dispatch --state-dir ~/dispatch-dev/state-candidate version
```

- **Never point a candidate at the supervision state.** State is forward-only,
  and the pinned release may not read what a newer build writes.
- **Never replace the pinned binary** with a candidate while it supervises.
- **Run trials in a project of their own.** Use a separate clone and state,
  outside `/tmp`, with any hooks in that project's `.claude/settings.local.json`
  only.

## 7. Integrate one contribution at a time

Land one piece of Work, then look at the rest again before landing the next.
Run these from the integration clone. `D` below stands for
`~/dispatch-dev/dispatch-0.4.11/dispatch --state-dir ~/dispatch-dev/state`.

1. **Read its state:** `D status <id>` shows where its workspace is and, for
   Work that did not begin from the current source, what differed at the
   start. `D watch` shows the whole project.
2. **Review it in its workspace:** for example, `git -C <workspace> status`
   and `git -C <workspace> diff`. Read the worker's report. Then `D check <id>
   --merged` runs `checks.verify` on HEAD plus the Work's change as it is now,
   untracked files included, in the project's check workspace, and prints one
   line per command. From 0.4.16 a tree that already passed is not checked again
   (`passed, reused from <run>`), but only for the same `PATH`: a check run from
   inside an agent's shell, which may change `PATH`, is not reused by yours. Note the `Δ` it prints. From 0.4.14; before, run fmt, clippy and the
   full suite yourself on HEAD plus the patch.
3. **Get the human's approval.** Recommend accept, revise or reject. A person
   decides; an agent's opinion is not the approval.
4. **Finish it:** wrapped Work (`attach --`) finishes by itself when the agent
   exits. Other Work: `D finish <id> --allow-unsafe-local` freezes Δ and runs
   the checks on what would land: in the Work's workspace when the clone has not
   moved since the Work began, otherwise on the clone now plus Δ, which accept then
   reuses. The summary shows the verdict against
   the clone now and what accept would verify, and its `Δ`. Before `finish`,
   `diff` shows no changes.
5. **Accept it:** `D accept <id> --expect <Δ from step 2>` refuses, applying
   nothing, if the frozen Δ is not the one you reviewed, for example because the
   worker changed something after `check --merged`. Otherwise it validates the
   patch against the clone as it is now, prints whether the checks ran on the
   merged tree (one line per command), or why not, and applies the patch.
   Always pass the ID. Before 0.4.14, compare `D diff <id>` with what you
   reviewed instead.
6. **Commit it in the integration clone:**
   ```sh
   git status --short     # only the patch: dispatch.yml and .claude/ are excluded
   git add -A && git commit
   ```
7. **Recheck the remaining Work:** `D watch` shows every other Work item judged
   against the new HEAD: CONTINUE, REFRESH or STOP, with reasons.

**Work you will not accept.**
- **Stale (REFRESH):** attached Work has no `refresh`. Start a new worker from
  the current source and give it the old patch and the reasons. Once its Work
  is finished, `D close <old-id> --reason superseded --replacement <new-id>`.
- **Already in the clone (STOP):** `D close <id> --reason already-present`.
- **Unfinished, and nobody will finish it:** `D close <id> --reason unfinished`.
- **Wrong:** `D reject <id>`, after a person judges it. `reject` refuses Work
  that has not been finished.

`close` records no review, so landed or abandoned work is not counted as a
rejection.

**When you are done,** `D stop` ends watching.

## 8. Reuse a check cache only when it is safe

> A build cache may be reused only when one candidate workspace's compiled
> artifacts can never stand in for another's checks.

Dispatch runs `checks.verify` in two kinds of place:
- `finish` runs it in the Work's own workspace when the source has not moved,
  and when its merged-tree check fails, to show whose change broke it.
- merged-tree checks (`accept`, `check --merged`, and `finish` on a moved
  source) run it in the project's check workspace,
  `<state>/check-workspaces/<sha256 of the source path>`, which is kept between
  runs, with its ignored build output, until `dispatch clean`.

Each command runs under `/bin/sh -lc`, with the environment cleared except
`PATH`, `HOME`, `TMPDIR` and `LANG`. A `CARGO_TARGET_DIR` exported in your
shell does not reach the checks. One exported by your login profile would
reach every check workspace at once. To check, run
`/bin/sh -lc 'echo "${CARGO_TARGET_DIR-unset}"'`.

**For Rust**, use one of these:
- **The default target directory per workspace.** Set no `CARGO_TARGET_DIR`,
  as in the `dispatch.yml` above. Each workspace builds into its own `target/`.
  The check workspace keeps its own `target/`, so merged-tree checks build
  incrementally after the first.
- **A `CARGO_TARGET_DIR` namespaced per workspace.** Derive the directory from
  the workspace's path, and keep it outside the repository:
  ```yaml
  checks:
    verify:
      - cargo fmt --check
      - CARGO_TARGET_DIR="$HOME/dispatch-dev/check-targets/$(pwd -P | shasum -a 256 | cut -c1-16)" cargo clippy --all-targets -- -D warnings
      - CARGO_TARGET_DIR="$HOME/dispatch-dev/check-targets/$(pwd -P | shasum -a 256 | cut -c1-16)" cargo test --lib
  ```
  This keeps check builds out of worktrees inside the integration root, which
  a 0.4.9 supervisor needs (section 9). From 0.4.16 the check workspace's path
  is stable, so merged-tree checks for one project share one such directory;
  before, every merged-tree check got a new one, because its scratch path was
  new. Nothing removes these directories, so delete
  `~/dispatch-dev/check-targets` yourself when no checks are running.

**Never one shared `CARGO_TARGET_DIR` across Dispatch's check workspaces.**
Cargo does not rebuild for a new checkout path, and it bakes
`CARGO_MANIFEST_DIR` into test binaries. This happened in 0.4.9:
- `dispatch.yml` shared one target directory across check workspaces;
- B's `finish` reused a test binary built for C's merged tree, with C's deleted
  scratch path compiled in;
- B's checks failed spuriously, and C's may have run stale binaries.

**Other build systems** raise the same question: can output built for one
workspace be used for another without being rebuilt?
- **Build directories.** A CMake, Make or Gradle build directory, a
  `node_modules`, or a compiler's incremental state belongs to one workspace.
- **Shared caches.** A cache shared across workspaces is safe only if it is
  keyed on every input, including any absolute path compiled into the output.
- **When unsure,** give each check workspace its own build output and pay for
  the rebuild.

## 9. Known limits

**With a 0.4.9 supervisor, keep build output outside the integration root.**
- **Why.** When Dispatch 0.4.9 attaches Work, it fingerprints the integration
  root's whole tree, ignored files included. That covers the hook and every
  form of `attach`. `claude --worktree` places worktrees inside the root, so
  any `target/` under `.claude/worktrees/` is walked at each registration.
  Registration became slow, and `attach` failed on a file cargo deleted
  mid-walk.
- **What to do.** Use `CARGO_TARGET_DIR` for workers and the namespaced form
  for checks, and start every worker before anyone builds.
- **What changed.** From 0.4.10, attach fingerprints the root only in strict
  mode (`coherence.accept: strict`).

**A `SessionStart` hook can be killed under load.**
- **What happened.** Claude Code gives the hook 60 s. During the 0.4.9 build,
  at load 14–21, two hooks were killed mid-registration. Each left a run
  directory with no record, no Work, and no notice in the session.
- **What to do.** If a session did not show the tracking message, look for it
  in `D history`. If it is missing, attach its worktree by hand from the
  integration clone:
  ```sh
  ~/dispatch-dev/dispatch-0.4.9/dispatch --state-dir ~/dispatch-dev/state \
    attach --workspace .claude/worktrees/<name> --allow-unsafe-local --agent claude
  ```
  S0 is then the merge base with the clone, which is the worktree's start
  when it began from `head`.
- **Leftovers.** Leave the half-made directories under `<state>/runs/` alone.
- **What changed.** From 0.4.10, a session in its own worktree is tracked,
  pending, or told it is not tracked, and a killed hook leaves no half-made run
  behind (`docs/attach.md`, "Work a runtime registers").
