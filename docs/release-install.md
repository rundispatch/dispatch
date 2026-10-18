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

### Upgrading to 0.4.16

No migration; the schema stays at 24. Existing records are not rewritten.
- **Rollback.** Back up the whole state directory before upgrading, and roll back only by
  restoring it. 0.4.15 reads 0.4.16's records and ignores the two new state directories; its
  `dispatch clean` neither lists nor removes them, so delete `check-workspaces/` yourself if
  you go back.
- **Additive JSON and events.**
  - `checks.reused`, recorded instead of `coherence.merged_checks` when finish, `check
    --merged` or accept finds a recorded pass for the exact tree: `by` (`finish`, `check` or
    `accept`), `reused_run`, `logs` (the reused run's), `passed_at`, `key`, `tree`, `source`,
    `delta_digest` and `passed: true`; `patch` for `check`. When finish reuses a pass, its
    candidate checks are recorded as passed with duration 0 and point at the reused logs.
  - `coherence.merged_checks` gains `tree_id` (the Git tree that was checked), `warm`
    (always `true`: the run used the project's check workspace) and `load_average` (1, 5 and
    15 minutes when the checks started, or `null` where it cannot be read). `by` can now be
    `finish`. A finish whose current source with Δ could not be built records `error` and
    `logs` instead of results.
  - `baseline.finished` gains `programs_resolved` and `checks_deferred`. Without
    `checks.baseline`, a native run no longer runs a suite on S0 before the agent: it has
    `checks_passed: null`, and `baseline_checks` holds only the commands checked on S0 after
    the candidate failed them, so it is often empty with checks configured.
  - `status --json` gains `checks_reused`, every `checks.reused` payload of the run, present
    only when there is one. `check --merged --json` puts a reuse's payload in
    `merged_checks`, and a skip as `{"skipped": "<why>"}`.
- **New state directories.**
  - `memo/`: one JSON file per pass, named by its key's digest. An entry is used for 24 hours
    and only while the logs it names still exist. Expired entries stay until `dispatch clean`,
    which lists it as `<state>/memo · recorded passes (N)` and removes it only after you
    confirm.
  - `check-workspaces/<sha256 of the source path>/`: one internal Git repository per project,
    with the project's ignored build output. It can grow as large as your own build. `dispatch
    clean` lists it and removes it after you confirm, unless its source lock is held.
- **Changed behaviour.**
  - **Finish on a moved source** checks the current source with Δ in the check workspace, not
    the Work's workspace, and checks S0 + Δ in the workspace only when that fails. Consent
    finishes do the same.
  - **Native baselines.** Without `checks.baseline`, no checks run before the agent. Each
    check's program is resolved first, and a missing one still stops the run, now saying
    `` check program not found: <program> (from `<command>`); the agent was not started ``. Without `checks.baseline`, S0 is
    checked afterwards with only the `checks.verify` commands the candidate failed, and
    `baseline_checks` holds those. Commands you list in `checks.baseline` still run on S0
    before the agent, as in 0.4.15, and `baseline.finished` then also records `checks_passed`
    with `checks_deferred: false`. For Git sources, outside strict mode, the
    source fingerprint covers tracked and non-ignored untracked files only, so a change to
    ignored build output after a run no longer blocks its accept. Strict mode and plain
    directories keep the whole-tree fingerprint. A native run that 0.4.15 made, in a Git source
    with ignored build output, still checks as unmoved and accepts with 0.4.16.
  - **Merged checks take the project's source lock.** `check --merged` and finish wait for
    it, saying `Merged checks: waiting for the source lock on <path>`. Accept and auto-apply
    hold it from their evaluation through the apply, as in 0.4.15, with their merged checks
    under it. An accept that finds it held now waits, up to the run's
    `execution.timeout_secs`, saying `Apply: waiting for the source lock on <path>`, where
    0.4.15 refused at once; a waiting auto-apply says `Auto-apply: waiting for the source
    lock on <path>`.
  - **`dispatch check --merged`** on finished Work whose source has not moved runs nothing,
    as accept does, unless a recorded pass is reused first. Its closing line now reads
    `accept checks again only if the source moves; a tree with a recorded pass is not checked
    again`.
  - **Git failures.** A source whose `git rev-parse` fails for any reason other than "not a
    git repository" is now an error carrying Git's message, no longer a plain directory.
- **Restart the owner.** Run `dispatch stop && dispatch start` so the project owner is 0.4.16.
  An older owner records verdicts as before, but re-reads every worktree each tick, and its
  consent finishes neither consult the memo nor check what would land.

### Upgrading to 0.4.15

No migration; the schema stays at 24. Existing records are not rewritten.
- **Rollback.** Back up the whole state directory before upgrading, and roll back only by
  restoring it.
- **Additive JSON and events.**
  - `workspace_removed` on `status --json` results, when the Work's workspace was removed:
    `{at, exact, last_seen_at}`. `last_seen_at` is when the kept last-seen changes were
    written, and `null` for an exact capture. `check --json` carries the same object only when
    there is a removal; otherwise its keys are unchanged.
  - `workspace.removed` events for a workspace seen missing add `last_seen_at`:
    `{"exact": false, "no_changes", "last_seen_at"}`. `attach.finished` adds
    `workspace_removed`, `null` unless the workspace was removed.
  - Older records have no `last_seen_at`, and show the time of their last-seen changes as
    unknown.
  - Discovered Work can have agent `hermes`. Its one session has the worktree's name,
    `hermes-<8 hex>`, as `session_id`, `source: "discovered"`, and `started_at` from when Git
    made the worktree.
- **Sessions end after removal.** `runtime.session_ended` (`process_gone`) and
  `runtime.agent_gone` can now come after `workspace.removed`: the owner follows a removed
  workspace's recorded agent process by its exact identity until it is gone.
- **Work whose worktree vanished** can now be finished from its last-seen changes with
  `dispatch finish <id>`, or `f` in `watch`. Its `status` line now reads `removed <time>: only
  the changes last seen were kept; finish them yourself (dispatch finish <id>) or close it
  (dispatch close <id> --reason workspace-removed)`. Close still works as before.
- **`dispatch start` waits for the first scan.** It returns once the owner has listed the
  worktrees already there, still within 120 s, and the watcher record
  (`watchers/<key>.json`) gains `discovery_ready`. An "Already watching" start waits for that
  too.
- **The Hermes hook.** `dispatch setup` → Runtime integrations → `Install the Hermes hook`
  appends one block to `~/.hermes/config.yaml` (or `$HERMES_HOME/config.yaml`), after a
  backup beside it (`config.yaml.dispatch-backup-<time>`):

  ```yaml
  # Added by dispatch setup, which removes it unless it is edited.
  hooks:
    on_session_finalize:
      - command: "'<dispatch>' --state-dir '<state>' hook hermes"
        timeout: 300
  ```

  `Remove the Hermes hook`, in the same menu, takes out exactly that block and gives the file
  its bytes back. If the Dispatch binary moves, the command changes, and Hermes asks for consent
  again on the hook's next run.
- **`hermes config set` drops the marker.** Hermes 0.18.2 writes `config.yaml` with PyYAML's
  `yaml.dump` (`set_config_value` → `atomic_yaml_write`), which drops every comment, Dispatch's
  marker line included, and reformats the block. The hook still runs, and setup still reports
  `Hermes hook: installed`, but `Remove the Hermes hook` then changes nothing and says:

  ```text
  Nothing changed: Dispatch's hook in <path>/config.yaml is not as Dispatch added it; it was edited or added by hand, so remove it there yourself
  ```

  Remove it by hand: delete the `on_session_finalize` entry whose command ends in
  `hook hermes`, and the `hooks:` key if nothing else is under it.
- **Restart the owner.** Run `dispatch stop && dispatch start` so the project owner is 0.4.15.
  An older owner discovers once per `poll_secs` tick, does not recognize Hermes worktrees, does
  not keep a fresh Δ for them, and records no session's end once a worktree was removed. Hermes worktrees present when the 0.4.15 owner first scans
  are listed, not tracked.

### Upgrading to 0.4.14

No migration; the schema stays at 24. Existing records are not rewritten.
- **Rollback.** Back up the whole state directory before upgrading, and roll back only by
  restoring it. In a check on a scratch state directory, 0.4.13's `history` and `status` still
  read Work that 0.4.14 had recorded: merged-check events, review digests and a discovered Codex
  Work.
- **Additive JSON and events.**
  - `coherence.merged_checks`, with `by` (`check` or `accept`), `source` (`{"git": {head,
    dirty}}` or the plain directory), `delta_digest`, `passed`, `duration_ms`, `results` (each
    command with its `exit_code`, `status`, `duration_ms` and logs) and `logs`. `check --merged`
    adds `patch`. An observation from `check --merged` does not move the run's state revision;
    one from `accept` is part of the accept.
  - `delta_digest`, the full SHA-256 of the patch bytes, on `check --json` and on the
    `review.accepted` and `review.rejected` events. `check --merged --json` has exactly
    `delta_digest`, `merged_checks`, `run_id` and `validity`.
  - Discovered Work can have agent `codex` or `cursor`, with `workspace_owner: "runtime"` and
    the provenance `discovered` from 0.4.13. Its one session has the Codex thread or the Cursor
    worktree name as `session_id`, `source: "discovered"`, and `started_at` from when Git made
    the worktree. It never has an `agent_process` or an end.
  - New Cursor runs record `cache_read_tokens` and `cache_write_tokens` categories, and a total
    labelled `uncached-input+output+cache-creation`. New Codex runs record cache-write and
    reasoning-output counters; their total is unchanged.
- **Waits and error texts.** `dispatch start` and `dispatch stop` wait up to 120 s for the
  background owner. Their errors read `the background owner for <root> did not become ready in
  120 s; see <log>` and `the watcher for <root> (pid N) did not stop within 120 s`. Provider
  probes wait up to 60 s, and a setup write waits up to 30 s for another writer before
  `another setup write is in progress`.
- **`harnesses.hermes` in `dispatch.yml`** takes `model`, `provider`, `executable` and
  `extra_args`. 0.4.14 refuses a `provider` that is not one plain identifier; 0.4.13 ignores the
  section.
- **New flags.** `dispatch check --merged [--allow-unsafe-local]` and
  `dispatch accept --expect <digest>`. Output without them is unchanged, except the new `Δ` line
  on `finish`, `check` and `diff --stat`, and the per-command lines on `finish` and `accept`.
- **Restart the owner.** Run `dispatch stop && dispatch start` so the project owner is 0.4.14.
  An older owner does not see Codex or Cursor worktrees, and does not record a session's end
  after `finish`. Codex and Cursor worktrees present when the 0.4.14 owner first scans are
  listed, not tracked.

### Upgrading to 0.4.13

No migration; the schema stays at 24. New fields live in the existing run JSON, and existing
records are not rewritten.
- **No rollback without a backup.** Once a 0.4.13 owner has found Work, 0.4.12 cannot read the
  state directory: `status` and `history` fail on the `discovered` provenance. Back up the
  whole state directory before upgrading, and roll back only by restoring it.
- **Additive JSON.**
  - The attachment's `provenance` can be `{"discovered": {"commit"}}`: Work the owner found
    in a Claude Code worktree that appeared while the project was watched. Its S0 is that
    commit, the worktree's merge base with the project, with `confidence: "full"`. Its
    observed session has `session_id` `lock:<name>@<pid>` and `source: "discovered"`.
  - `attach.created` records `via: "discovery"` for that Work. Its `registration_ms` is
    measured from the session's process start, as the lock records it, not from a hook.
  - New events `runtime.agent_running` and `runtime.agent_gone`, each with
    `{agent_process, previous}`, when the owner records or clears the process a session lock
    names.
  - `runtime.session_ended` has two more reasons: `process_gone` (the lock was left by a
    process that is gone) and `lock_released`. A session discovery found already ended carries
    `end_reason: "process_gone"` on the attachment, with no separate event.
  - `watch --json`'s `watcher` record carries `inventory`, a list of `{path, first_seen,
    base?, lock?, attach}`, only when something is found, not tracked.
  - The owner keeps two more files beside its record: `watchers/<key>.inventory.json`, the
    watch's list of worktrees found, not tracked, and `watchers/<key>.live.json`, its file
    counts for the Work it follows. `dispatch stop` removes both. Readers ignore the counts
    unless the owner that wrote them holds the project.
- **`status` changed for working attached Work.** Instead of `Runtime 0.0s`,
  `Files changed 0` and `Diff +0 / -0`, it prints `Runtime unknown`, a new
  `Tracked for 12m` line, and `Files changed <n> now` or `unknown`. There is no Diff line
  until the Work is finished. Under Workspace it adds `agent running (pid N)` while a
  recorded agent process runs. After `Project:` it adds a "Found, not tracked" section when
  there is something to list. Delivered results print as before. `status --json` is
  unchanged.
- **`start` and `setup`.**
  - `dispatch start` on a terminal, in a project without `checks.verify`, now offers the
    checks it detected before it starts watching. "Continue without checks" is focused, and
    Esc starts nothing. Without a terminal it watches at once, as before, and prints one line
    saying no checks are configured.
  - `dispatch setup --checks --verify '<command>'`, repeatable, saves exactly those commands
    without a terminal. It never grants consent. `dispatch setup --checks` without a terminal
    and without `--verify` fails, naming `--verify`. Provider setup is unchanged.
- **Restart the owner.** Run `dispatch stop && dispatch start` so the project owner is 0.4.13.
  An older owner discovers nothing, records no liveness, and counts no files, so `status`
  shows files as `unknown`. Worktrees present when the 0.4.13 owner first scans are listed,
  not tracked.

### Upgrading to 0.4.12

No migration; the schema stays at 24. New fields live in the existing run JSON, and existing
records are not rewritten.
- **ID-less `accept` and `reject` refuse when several results wait.** Without an ID they act
  only when exactly one result waits for review for the source. With none or several they
  change nothing, exit 1 and list the waiting results with explicit commands. Scripts should
  pass the Work's ID; a unique prefix still works.
- **`reject` no longer closes unfinished Work.** In 0.4.11, rejecting unfinished Work whose
  workspace was gone closed it. Now `reject` refuses unfinished Work and names
  `dispatch close <id> --reason unfinished` or `--reason workspace-removed`. STOP now points
  to `dispatch close <id> --reason already-present` instead of `reject`.
- **Additive JSON.**
  - `outcome.closed` (`{reason, at, replacement?, explanation?}`) on Work a person closed, in
    the run's metadata, `status --json` and every event's `outcome`. Closed Work reads as the
    state `closed` in `watch --json`, `serve --json` and `history`, with `review` null.
  - `work.closed` gains the reasons `unfinished`, `already_present` (with `world_digest`) and
    `superseded` (with `replacement`). Records 0.4.11 wrote, `workspace_removed`, read as
    before.
  - The attachment gains `source_at_start`: `"not_observed"`, or `{"observed": {at,
    source_head, differing, paths: [{path, applied_work?}]}}`. It is absent for workspaces
    Dispatch made and for Work created before 0.4.12. `attach.created` carries
    `"not_observed"`, and the new event `attach.source_observed` carries the observation once.
    `status --json` does not include it.
- **Human output changed.** Anyone parsing it should read `check --json`, `status --json` or
  `watch --json` where they exist.
  - `accept` prints `Merged checks: …` before it applies.
  - `finish` and a native result's summary are headed by the verdict (for example
    `Finished, but stale (REFRESH)`), add `Work <id>` with its patch, `Coherence:` and
    `At accept`, and list verdict-driven commands under `Then`. The trailing
    `Next: dispatch check <id>` is gone.
  - `check` adds `At accept: …`, and its `Next` for attached REFRESH and for STOP changed.
  - `clean` shows full Work IDs and adds a "Kept, because Dispatch did not make them" list.
- **Worktree session ends are recorded.** Claude Code reports a `claude --worktree`
  session's end from the main checkout; 0.4.12 records it on the Work by session. Sessions
  that ended before the upgrade still read as open: finish or close their Work. `claude -p`
  sessions report no end at all.
- **Restart the owner.** Run `dispatch stop && dispatch start` so the project owner is 0.4.12.
  An older owner never records the start observation, so Work a runtime registers while it
  runs stays "not observed".

### Upgrading to 0.4.11

No migration; the schema stays at 24, and nothing on disk changes.
- **`dispatch watch` on a terminal looks different.** It now shows a table of named Work,
  with the selected Work's details below, or two-line cards below 90 columns. Piped `watch`,
  `watch --plain`, `watch --json`, `serve`, `status` and every JSON contract are unchanged. A
  script that reads the interactive screen should read `watch --json` instead.
- **No auto-apply mode in `watch`.** It no longer shows the `Shift+Tab` mode hint or reacts to
  it. Auto-apply for attached Work is still set per Work (`attach --auto-apply`).
- **Restart the owner.** Run `dispatch stop && dispatch start` so the project owner is 0.4.11.
  Otherwise `watch`'s header reports the version mismatch, as `status` does.

### Upgrading to 0.4.10

No migration; the schema stays at 24.
- **Registration in progress.** Work being registered is assembled in
  `<state>/registrations/` and moved into `runs/` only when complete. The
  watching owner publishes pending registrations and removes failed ones.
- **Old partial runs stay.** Partial run directories that 0.4.9 or earlier left
  in `runs/` (no `metadata.json`) are not touched.
- **Attach is cheaper outside strict mode.** Attaching Work no longer
  fingerprints the integration root's whole tree; the fingerprint was read only
  in strict mode. With `coherence.accept: strict`, attach still walks the root,
  but a file that disappears during that walk now counts as a difference, not an
  error.
- **Restart the owner.** Run `dispatch stop && dispatch start` so the project
  owner is 0.4.10. Only a 0.4.10 owner publishes pending registrations.

### Upgrading to 0.4.9

No migration; the schema stays at 24.
- **New events.** Two event types are written to the existing events table:
  `interaction.landed`, once on each applied run, and `interaction.outcome`,
  recorded by the watching owner. 0.4.8 reads a state directory that contains
  them and ignores them.
- **Restart the owner.** Run `dispatch stop && dispatch start` so the project
  owner is 0.4.9. A 0.4.8 owner records no outcomes, so its landings would show
  as having no outcome in the queries.
- **Live patches are now written by rename.** `delta-live.patch` and the
  owner's scratch patches are published atomically, as files of mode 0600.

### Upgrading to 0.4.8

No migration; the schema stays at 24, and 0.4.7 reads everything 0.4.8
writes.
- **The owner compares Work.** While it watches a project, it compares the Work
  not yet integrated. It keeps that view in
  `watchers/<key>.interactions.json`, which is removed when the owner stops.
  After upgrading, run `dispatch stop && dispatch start` so the owner is 0.4.8;
  an older owner writes no interactions.
- **New output fields.** `watch --json` work objects gain `interactions`
  (`null` while not watched). `status --json` gains it while watched. `status`
  gains a `Concurrent` section.
- **Claude preflight.** A `claude --version` that gives no answer no longer
  refuses the profile, and an `auth status` that gives none refuses that launch
  only. A refusal recorded before upgrading still holds until you authorize
  again in `dispatch setup`.

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
