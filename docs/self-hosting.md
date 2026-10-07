# Building Dispatch with Dispatch

This guide sets up the arrangement that built 0.4.9 and 0.4.10: parallel coding
agents, each in its own workspace, supervised by a pinned Dispatch release. You
integrate their work one contribution at a time. Each command below was run as
written against a real clone and state directory, with Dispatch 0.4.9 as the
supervisor.

The paths are examples. Keep each one outside `/tmp` and outside any build
directory, and use the same binary and state directory in every command.
`DISPATCH_HOME` can stand in for `--state-dir`, but the hooks name
`--state-dir` explicitly, because Claude Code runs them with its own
environment.

## 1. Pin a released binary

The supervisor is a release, not something you build. A candidate build never
supervises the work that produces it.

```sh
mkdir -p ~/dispatch-dev/downloads/v0.4.9 && cd ~/dispatch-dev/downloads/v0.4.9
gh release download v0.4.9 -R rundispatch/dispatch \
  -p dispatch-macos-arm64.tar.gz -p SHA256SUMS
shasum -a 256 -c SHA256SUMS --ignore-missing     # dispatch-macos-arm64.tar.gz: OK
mkdir ~/dispatch-dev/dispatch-0.4.9
tar -xzf dispatch-macos-arm64.tar.gz -C ~/dispatch-dev/dispatch-0.4.9
cd ~/dispatch-dev/dispatch-0.4.9
jq -r .binary_sha256 BUILD.json
shasum -a 256 dispatch                           # must print the same hash
chmod a-w dispatch BUILD.json .
./dispatch version                               # dispatch 0.4.9
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
git switch -c release-0.4.10 origin/main   # or `git switch release-0.4.10` if it exists upstream
printf 'dispatch.yml\n.claude/\n' >> .git/info/exclude
```

`dispatch.yml` and `.claude/` are your local setup, not part of the release.
Excluding them through `.git/info/exclude` keeps them out of every baseline
(S0), every patch (Δ) and every commit, without touching the tracked
`.gitignore`. `.claude/` also holds the worktrees `claude --worktree` makes.

Write `dispatch.yml` with the project's checks:

```yaml
coherence:
  poll_secs: 5
checks:
  verify:
    - cargo fmt --check
    - cargo clippy --all-targets -- -D warnings
    - cargo test --lib
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
~/dispatch-dev/dispatch-0.4.9/dispatch --state-dir ~/dispatch-dev/state start
~/dispatch-dev/dispatch-0.4.9/dispatch --state-dir ~/dispatch-dev/state watch
```

Leaving `watch` with Ctrl+C does not stop the owner.

## 4. Install the hooks in the project only

`dispatch setup` → **Runtime integrations…** installs four Claude Code hooks
user-wide, in `~/.claude/settings.json`. They point at the binary that ran
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
        "command": "'/Users/you/dispatch-dev/dispatch-0.4.9/dispatch' --state-dir '/Users/you/dispatch-dev/state' hook claude" }] }
    ],
    "SessionEnd": [
      { "hooks": [{ "type": "command", "timeout": 5,
        "command": "'/Users/you/dispatch-dev/dispatch-0.4.9/dispatch' --state-dir '/Users/you/dispatch-dev/state' hook claude" }] }
    ],
    "WorktreeRemove": [
      { "hooks": [{ "type": "command", "timeout": 300,
        "command": "'/Users/you/dispatch-dev/dispatch-0.4.9/dispatch' --state-dir '/Users/you/dispatch-dev/state' hook claude" }] }
    ],
    "PreToolUse": [
      { "matcher": "ExitWorktree",
        "hooks": [{ "type": "command", "timeout": 300,
        "command": "'/Users/you/dispatch-dev/dispatch-0.4.9/dispatch' --state-dir '/Users/you/dispatch-dev/state' hook claude" }] }
    ]
  }
}
```

- **`"worktree": {"baseRef": "head"}`.** Without it, `claude --worktree` starts
  each worktree from the remote's default branch, not from the release branch
  you have checked out. The worker would begin from the wrong source.
- **Timeouts.** These are the ones `dispatch setup` installs.
- **One set of hooks.** Claude Code runs the hooks from every settings file.
  If Dispatch's hooks are also in `~/.claude/settings.json`, a session in this
  clone is reported to both binaries and both states.

## 5. Run workers in isolated workspaces

Each worker gets a workspace of its own. No worker edits the integration clone
itself.

- **Claude Code:** from the integration clone, run `claude --worktree <name>`.
  The session works in `.claude/worktrees/<name>` and opens with "Dispatch is
  tracking this worktree as Work `<id>`". A session started in the clone itself
  is told that Dispatch cannot tell its edits from yours and does not track
  them.
- **Any other agent CLI:** from the integration clone, run:
  ```sh
  ~/dispatch-dev/dispatch-0.4.9/dispatch --state-dir ~/dispatch-dev/state \
    attach --allow-unsafe-local --agent <label> -- <agent command>
  ```
  Dispatch makes a workspace under the state directory and runs the agent
  there. When the agent exits, Dispatch freezes the patch and runs the checks.
  Your checkout is not touched.

`--allow-unsafe-local` acknowledges that `checks.verify` runs on the host.
Nothing here is sandboxed.

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
  outside `/tmp`, with hooks in that project's `.claude/settings.local.json`
  only.

## 7. Integrate one contribution at a time

Land one piece of Work, then look at the rest again before landing the next.
Run these from the integration clone. `D` below stands for
`~/dispatch-dev/dispatch-0.4.9/dispatch --state-dir ~/dispatch-dev/state`.

1. **Read its state:** `D status <id>` shows its sessions and its `Concurrent`
   section, and `D watch` shows the whole project.
2. **Review it in its workspace:** for example,
   `git -C .claude/worktrees/<name> status` and
   `git -C .claude/worktrees/<name> diff`. Read the worker's report, and run
   fmt, clippy and the full suite yourself on HEAD plus the patch.
3. **Get the human's approval.** Recommend accept, revise or reject. A person
   decides; an agent's opinion is not the approval.
4. **Finish it:** `D finish <id> --allow-unsafe-local` freezes Δ and runs the
   checks in the Work's workspace. Wrapped Work (`attach --`) finishes by
   itself when the agent exits. Then `D diff <id>` prints the frozen patch;
   confirm it is the one you reviewed. Before `finish`, `diff` shows no
   changes.
5. **Accept it:** `D accept <id>` validates the patch against the clone as it
   is now. If the clone has moved, it runs the checks on the merged tree. Then
   it applies the patch.
6. **Commit it in the integration clone:**
   ```sh
   git status --short     # only the patch: dispatch.yml and .claude/ are excluded
   git add -A && git commit
   ```
7. **Recheck the remaining Work:** `D watch` shows every other Work item judged
   against the new HEAD: CONTINUE, REFRESH or STOP, with reasons.

**Stale Work.** Finish it, then `D reject <id>`; `reject` refuses Work that
has not been finished. Attached Work has no `refresh`, so start a new worker
from HEAD and give it the old patch and the reasons.

**When you are done,** `D stop` ends watching.

## 8. Reuse a check cache only when it is safe

> A build cache may be reused only when one candidate workspace's compiled
> artifacts can never stand in for another's checks.

Dispatch runs `checks.verify` in two kinds of place:
- `finish` runs it in the Work's own workspace.
- `accept` runs it on the merged tree, in a new scratch directory under
  `<state>/runs/<id>/`, which is deleted afterwards.

Each command runs under `/bin/sh -lc`, with the environment cleared except
`PATH`, `HOME`, `TMPDIR` and `LANG`. A `CARGO_TARGET_DIR` exported in your
shell does not reach the checks. One exported by your login profile would
reach every check workspace at once. To check, run
`/bin/sh -lc 'echo "${CARGO_TARGET_DIR-unset}"'`.

**For Rust**, use one of these:
- **The default target directory per workspace.** Set no `CARGO_TARGET_DIR`,
  as in the `dispatch.yml` above. Each workspace builds into its own `target/`.
  Merged-tree checks build from nothing each time, which is slower but
  correct.
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
  a 0.4.9 supervisor needs (section 9). Every merged-tree check gets a new
  directory, because its scratch path is new. Nothing removes these
  directories, so delete `~/dispatch-dev/check-targets` yourself when no checks
  are running.

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
