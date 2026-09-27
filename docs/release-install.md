# Dispatch 0.4.0: install, upgrade, uninstall

Dispatch 0.4.0 is an experimental developer preview. It adds
[attached work and `dispatch serve`](attach.md): observing and, if you ask, applying
work an external agent (Claude Code, Codex, Cursor or anything else) produced in its
own worktree, under the same [work coherence](coherence.md) model as work Dispatch
launched itself, on top of [auto-apply](product-guide.md#auto-apply) from 0.3.0.

Runtime scope: macOS arm64 is the platform exercised interactively for this
release. Linux x86_64 is built and smoke-tested by CI (build, package, `dispatch
version`) but was not exercised interactively. Windows, SSH/tmux and native GUI
terminals are not certified. Packages are unsigned and not notarized.

## Try without replacing an installation

Download `dispatch-macos-arm64.tar.gz` (or `dispatch-linux-x86_64.tar.gz`) and
`SHA256SUMS` from the GitHub release, or build a local archive (see the end of this
page). Extract the archive into a new directory, then run the executable by its
absolute path. `BUILD.json` records source and binary identities. From the output
directory, `shasum -a 256 -c SHA256SUMS` checks the archive and adjacent executable.
The archive contains only `dispatch`, `LICENSE`, `INSTALL.md`, and `BUILD.json`.
No account state, logs, grants, credentials or fonts are bundled.

```sh
shasum -a 256 -c SHA256SUMS --ignore-missing
mkdir -p /tmp/dispatch-install
tar -xzf dispatch-macos-arm64.tar.gz -C /tmp/dispatch-install
/tmp/dispatch-install/dispatch version
cd /path/to/your/project
/tmp/dispatch-install/dispatch --state-dir /path/to/private/dispatch-state setup
/tmp/dispatch-install/dispatch --state-dir /path/to/private/dispatch-state setup --checks
/tmp/dispatch-install/dispatch --state-dir /path/to/private/dispatch-state
```

The expected version is `dispatch 0.4.0`. Use `command -v dispatch` and
`type -a dispatch` to locate older PATH installations. An explicit path is the
reliable way to select this build. Do not replace a working binary just to try
it. Ordinary work uses the provider's account; test-only setup must use fixture
homes and fake executables, as the acceptance scripts do.

## Upgrade, restore, uninstall

Stop all Dispatch sessions before upgrading. Back up the complete private state
directory and keep the matching binary. SQLite migrations now preserve a consistent
600-permission `dispatch.schema-N-*.db` backup beside the database before upgrading
historical schemas. These backups preserve database state, not artifact files; keep
a full state-directory copy as well. Schema is 24 as of 0.4.1. Newer schemas are
refused.

To roll back, stop every session and restore a complete matching backup into a
separate directory; point the matching old binary at it with `--state-dir`. Never
point an old binary at current state or restore only a DB over newer artifacts.

### Upgrading to 0.4.7

No migration; the schema stays at 24. State is forward-only: a run finished by
check consent records `finish_reason: by_consent`, which 0.4.6 cannot read.
- The state directory gains `projects/`, one private file per project whose checks
  you let run by themselves (`dispatch setup --checks`). Each file names the exact
  `checks.verify` commands approved; a change to them voids the consent until you
  approve again.
- New command: `dispatch clean`. Rejected workspaces that 0.4.6 kept under
  `<state>/workspaces/` are listed by it; nothing is removed without asking.
- `dispatch watch` on a terminal is now interactive. `watch --json`, `--plain` and
  output that is not a terminal are unchanged.
- After upgrading, `dispatch stop && dispatch start` replaces an owner started by
  the old binary; only a 0.4.7 owner verifies Work by consent.

### Upgrading to 0.4.6

No migration; the schema stays at 24. State is forward-only: runs written by 0.4.6
use new attachment values (`workspace_at_start`, `workspace_owner`, `sessions`,
`workspace_removed`, `managed`) that 0.4.5 cannot read.
- `dispatch attach -- <agent>` run from the checkout itself now makes the agent a
  workspace under `<state>/workspaces/` instead of refusing.
- `dispatch finish` counts only paths that can enter Δ. Ignored build output no
  longer fails it.
- **Claude Code hooks** are installed only through `dispatch setup` → Runtime
  integrations. The hook command names the Dispatch binary and state directory it
  was installed with; if you move either, install the hooks again.

### Upgrading to 0.4.5

No migration; the schema stays at 24.
- New commands: `dispatch start`, `watch` and `stop`.
- The project owner (`start`, or `serve` in the foreground) now also records
  verdicts on Ready results awaiting review, native or attached, when the source
  moves. So `serve`, `watch` and `history` agree with `check`.
- The state directory gains `watchers/` (one record and one log per watched
  project).
- `status` ends with a `Project:` line. `serve --json` is unchanged; `watch --json`
  adds a `watcher` object.
- After upgrading, `dispatch stop && dispatch start` replaces an owner started by
  the old binary; `watch` names the version the owner runs.

### Upgrading to 0.4.4

No migration; the schema stays at 24. An accept that coherence refuses now leaves
the result pending instead of recording an acceptance. `check` and `status` show a
refusal by the merged-tree checks until the source moves. Answering a native run's
question after the project changed now continues the run instead of stopping it with
`source_drift`, and `history`, `status` and `serve` show such a waiting run as
`question` (it was `working`). `CoherenceRecord` gains an
optional `overridden` field, `serve --json` gains `overridden`, and `check --json`
gains `landed_by`.

### Upgrading to 0.4.3

No migration; the schema stays at 24. Setup is now menus; the `resources.yml` it
writes has the same fields as before, so 0.4.1 and 0.4.2 still read it. `serve
--json` work objects gain `origin`, `s0`, `verification`, `review` and `applied_by`,
and `agent` names the harness for native runs (it was `dispatch`). `history` shows
WORK, STATE and VERDICT instead of STATUS and CANDIDATES.

### Upgrading to 0.4.2

0.4.2 fixes `dispatch setup` in 0.4.1, which could not save a profile once the state
directory had a database: it reported `no such table: capacity_authorizations`. No
migration; the schema stays at 24. If setup failed under 0.4.1, run it again.

### Upgrading to 0.4.1

0.4.1 removes sync, public priors and routing, capacity and admission, the control
protocol, private evidence, planning, blind comparison and the automatic retry. Its
first open migrates the database to schema 24 and leaves a `dispatch.schema-21-*.db`
backup. Migration 24 drops the tables of the removed features; the backup keeps
every row. Human judgments stay in the database: reviews, and the blind evaluations
and routed-run feedback recorded before 0.4.1. Earlier runs keep loading; their run
mode reads as `native`, and fields that 0.4.1 no longer uses are kept in their
metadata as they were.

Finish, accept or reject work started by 0.4.0 before upgrading. A 0.4.0 run still
queued or executing when you upgrade is not closed by 0.4.1, because Dispatch never
closes a run whose attempt may still have a live agent. It stays unfinished.

In `resources.yml`, the `capacity:` block is ignored. A Codex profile now needs the
account evidence that `dispatch setup codex` records, so run setup again for an
existing Codex profile. In `dispatch.yml`, `execution.max_parallel` is ignored.
`--json` output names the execution policy `execution` (it was `phase3`) and the run
mode `native`.

### Upgrading to 0.2.0

Work coherence adds no database migration: the schema version stays at 20, the
version of the unreleased 0.1.3 candidate. A state directory created by the
published 0.1.x releases is older (v0.1.2 is at schema 11), so its first open runs
the normal historical migrations to 20 and leaves a private
`dispatch.schema-N-*.db` backup beside the database (see above). Work coherence
itself adds one optional field to a run's stored record and needs no backfill. A run created by an older version has no stored coherence data;
when you accept it, Dispatch derives everything it needs from the run's baseline,
its patch and the current source, so it gets coherence checking at accept time
automatically. It reads the configuration frozen with that run, which is a file
copied at run creation; if that copy cannot be read, accept keeps the strict
any-drift refusal.

The `coherence:` block in `dispatch.yml` is optional. With no block, accepting a
result onto a source that changed elsewhere now validates the patch instead of
refusing on any difference. To restore the old any-drift refusal, set
`coherence.accept: strict` before starting the run. See the
[coherence reference](coherence.md) for every key.

### Upgrading to 0.3.0

No database migration: the schema version stays 20, unchanged since 0.2.0.
Auto-apply adds one optional field to a run's stored outcome,
`RunOutcome.applied_by` (`human` or `auto_apply`), serialized only when a run was
actually applied; it is omitted entirely (not `null`) on every unapplied run and on
every run written by an older version, so old `run.json`/database records still
deserialize with no backfill. Every TUI session still starts with auto-apply off,
exactly as before this release, because the mode is session memory and was never
persisted. Nothing changes for existing runs: a run created by 0.2.0 or earlier is
reviewed exactly as it always was unless you explicitly opt into `--auto-apply` or
the session toggle for the *next* run.

### Upgrading to 0.3.1

No migration: a new baseline no longer force-tracks files a Git source's own ignore
rules exclude (see `docs/coherence.md`, "World observation"). Runs created before
0.3.1 keep their old baselines untouched and may still show a `.gitignore`'d build
artifact in their patches; only runs created by 0.3.1 or later get the fix.

### Upgrading to 0.4.0

**Migration 21** (`attached_work_mode`) rebuilds the `runs` table the same way
migration 13 did: rename to `runs_v20`, recreate `runs` with the same columns plus a
widened `CHECK (run_mode IN (..., 'attached'))`, copy every row across, drop
`runs_v20`, then recreate the index and trigger migration 19 added on `runs` (the
rename leaves them bound to the old table name). Foreign keys are turned off for the
duration of this migration only, exactly as for migration 13, so the drop does not
fail on a database with rows in `attempts`, `control_runs` or `planned_goals` that
reference a run. Opening a schema-20 state directory creates a
`dispatch.schema-20-*.db` backup first, like any other historical-schema upgrade (see
above); schema becomes 21. An older 0.3.x or earlier binary refuses a schema-21 state
directory, exactly as it already refuses any newer schema — do not point it at
upgraded state.

Nothing changes for a run that already existed before the upgrade: `attach`,
`finish` and `serve` are new commands that only ever create `RunMode::Attached` runs
going forward; no existing run's stored record, outcome or events are rewritten by
this migration beyond the table rebuild itself. See [attach.md](attach.md) for what
the new commands do.

Uninstall by removing only the executable you installed and its archive/extraction
directory. Keep `~/.dispatch` (or your explicit state directory), project files and
provider accounts. State deletion and provider logout are separate owner actions.

## Build a local archive

```sh
cargo build --release --locked --target-dir /tmp/dispatch-rc/current/target
python3 scripts/package-local.py /tmp/dispatch-rc/current/target/release/dispatch /tmp/dispatch-rc/release
```

The package metadata, ordering and compression are deterministic for the same
binary and declared source manifest. This does not claim bit-identical Rust builds
across different machines or toolchains. No signing keys, tags, pushes or uploads
are involved. Publication is a separate owner decision.
