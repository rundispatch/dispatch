# Work coherence: technical reference

This page describes what the code in `src/coherence.rs` and `src/coherence/` does.
For the short version and the commands, see the [README](../README.md#work-coherence-keeping-results-valid-while-the-code-moves)
and the [product guide](product-guide.md). Coherence is part of the experimental
developer preview.

## The problem and the approach

A run works in a private copy of a frozen snapshot while the real source can change:
you edit files, another run is accepted, a build writes output. Before coherence, the
only protection was a whole-tree content hash: any difference (except `.git` and
`.dispatch`) refused the apply, including an edited README or a build directory.

Coherence treats finished work as an optimistic transaction. It asks whether the work
is still valid against the source **as it is now**, using only what the run already
stores. It adds no database table, no daemon, no index and no change to how agents are
launched or admitted. It never starts an agent by itself.

## Model

| Concept | Meaning | Where it lives in the code |
|---|---|---|
| S0 | The snapshot the run started from. | The run's private baseline repository: `RunRecord.baseline_path`, `baseline_commit` (and `source_fingerprint`, the strict whole-tree hash). The baseline commit tracks only the paths the source's ignore rules include (repository `.gitignore` files and `info/exclude`); ignored files are still copied onto disk so build caches remain available, and a plain-directory source, which has no ignore rules, keeps tracking everything. |
| Δ | The patch the agent produced. | The candidate's `diff_path` (`git diff --binary --full-index --no-renames`). Mid-run, a work-in-progress patch of the live workspace. |
| World (S1) | The source tree now, minus what its own ignore rules exclude. | Computed on demand as `world::WorldObservation`; never stored as a snapshot. |
| MustHold | A fact the work relied on, derived from (S0, Δ). | `MustHold` (`kind`: `Signature` or `File`; `origin`: `Modified`, `Referenced` or `FileFallback`). |
| Validity | The verdict. | `Validity { decision, evaluated_at, world_digest, world_changed, changed_files, reasons, analysis }`. |
| Decision | What to do. | `Decision`: `Continue`, `Refresh`, `Stop`. |
| Reason | Why, with evidence. | `Reason { code, fact_id, path, detail }`; `ReasonCode` is `fact_broken`, `fact_missing`, `same_symbol_edited`, `patch_conflict`, `already_applied`, `integration_check_failed`, `analysis_uncertain`. |
| Analysis level | How deep the check went. | `AnalysisLevel`: `symbols`, `files_only`, `integration`. |
| Per-run record | Latest verdict and lineage. | `RunRecord.coherence: Option<CoherenceRecord>` with `version` (1), `refreshed_from`, `facts`, `validity`, `first_invalid_at`. |

Facts are derived only from S0 and Δ, so they can be recomputed at any time.

### Verdicts

`evaluate` (in `src/coherence.rs`) returns `Continue` unless a reason says otherwise:

| Reason code | Produced by | Decision |
|---|---|---|
| `already_applied` | L0: the patch reverses cleanly against the current source | `Stop` |
| `patch_conflict` | L0: the patch does not apply cleanly | `Refresh` |
| `fact_broken` | L1: a referenced declaration's contract, or a watched file's bytes, changed | `Refresh` |
| `fact_missing` | L1: the declaration or file no longer exists | `Refresh` |
| `same_symbol_edited` | L1: a declaration the patch edits was also edited in the source | `Refresh` |
| `analysis_uncertain` | L1: a file that holds a fact does not parse; L2: a check could not run | `Refresh` at accept; dropped mid-run |
| `integration_check_failed` | L2: a configured check failed or timed out on the merged tree | `Refresh` |

At most 20 reasons are kept in a `Validity`; result projections keep 5.

## The pipeline

### World observation (`world.rs`)

`observe(source, baseline, baseline_commit, kind)` compares the current source with S0.

- A Git source (or linked worktree) contributes what `git ls-files -co --exclude-standard`
  lists: tracked and untracked files that its ignore rules do not exclude. A plain
  directory contributes everything except `.git` and `.dispatch`, so it cannot honor
  `.gitignore` and sees build output as change. The baseline commit (`source::create_snapshot`)
  is built to match: for a Git source it tracks only the same non-ignored paths, so a build
  artifact that exists at snapshot time is never force-tracked into S0 and later mistaken
  for part of Δ.
- Each present file is hashed as a Git blob (`hash-object --no-filters`) using the run's
  baseline repository, and compared with `git ls-tree -r` of `baseline_commit`. Symlinks
  are hashed by target text and never followed; parents that became symlinks are
  skipped.
- Changes are `Added`, `Modified`, `Deleted`, `ModeOnly` (executable bit) or `Symlink`.
  A baseline path missing now is `Deleted` unless the source's ignore rules exclude it
  (`git check-ignore --no-index`).
- A nested repository (a committed gitlink, or an untracked directory with its own
  `.git`) is not comparable and is skipped on both sides; it is never reported as
  deleted.
- `digest` is a SHA-256 over the sorted current non-ignored tree
  (`dispatch-world-v1`), not over the diff. It identifies the exact world that was
  evaluated.
- `signal(source, kind)` is the cheap change doorbell used by the watcher; it reads
  no file contents (see below).

### L0: file and patch state (`coherence.rs`)

1. World unchanged, or the patch file is empty: `Continue`, no Git or parsing work.
2. `git apply --check --reverse` in the source succeeds: `Stop`, `already_applied`.
3. `git apply --check` fails: `Refresh`, `patch_conflict`, with Git's message (at most 300 characters).
4. Otherwise go to L1.

`git apply` is only ever run with `--check` during evaluation, so evaluation never
modifies the source.

### L1: symbol and file facts (`facts.rs`, `symbols.rs`)

The work is proportional to the patch and the changed files, not the repository.
Everything is read from the baseline commit through the hardened Git wrappers.

**Symbol extraction** (`symbols.rs`, tree-sitter) handles `.rs` and `.py` only.

- Rust: `fn`, `struct`/`union`, `enum`, `type`, `const`/`static`, `trait` (members as
  `Trait::method`), `impl` methods (`Type::method` or `<Type as Trait>::method`) and
  items inside `mod` blocks. Nested items in function bodies, macros and `use`/`mod x;`
  are not symbols.
- Python: `def`, `class` (methods as `Class.method`) and module-level `ALL_CAPS`
  assignments. Definitions nested in functions or compound statements are not symbols.
- `sig_fp` is a SHA-256 over the declaration's tokens with comments dropped, preceded by
  its attributes or decorators, leaving out function bodies and, for traits and
  classes, their members. Struct and enum field lists are part of `sig_fp`.
  `full_fp` covers everything except comments and whitespace; a container's `full_fp`
  folds in its members'.
- If the parse tree has any error or missing node, the whole file is flagged
  (`has_error`) and its table is not trusted.

**Deriving facts** from the patch (`derive_facts`):

- The patch is parsed for text files. Binary files, symlinks and mode-only changes are skipped.
- `Modified` (signature) facts: for a modified `.rs`/`.py` file whose baseline parses,
  every baseline declaration that a removed line falls in, or into which text is
  inserted strictly inside its span. Context lines never count. A class or trait counts
  only for changes outside its members and is compared by header (`sig_fp`) alone.
- `Referenced` facts: identifiers on the lines the patch adds (identifiers, type names and
  method-call names), after removing names shorter than 3 characters, a fixed denylist
  (`new`, `get`, `len`, `main`, `self`, `default`, `clone`, `from`, `into`, `map`,
  `unwrap`, ... see `DENYLIST`) and names the patch itself declares. A name binds to a
  baseline declaration only when exactly one declaration with that base name exists in
  the baseline's `.rs`/`.py` files. Names found in more than 8 files, or whose `git grep`
  matches more than 40 files, and ambiguous names are left unbound and skipped. There is
  no scope or import resolution.
- `File` facts: a modified file in a language without symbol support, a modified `.rs`/`.py`
  file whose baseline does not parse, and baseline files the added lines mention by path
  (an exact tracked path, or a bare or partial name that matches exactly one tracked
  file). The fact is the SHA-256 of the baseline bytes. An added file in an unsupported
  language produces no fact.
- Facts are deduplicated by id (`sha256(kind|path|subject)` prefix), ordered Modified,
  Referenced, File, and capped at 200.

**Evaluating facts**: only facts whose path is among the world's changed files are
checked, always from what is on disk now.

- `File` fact: file gone is `fact_missing`; different bytes is `fact_broken`.
- `Signature` fact: file gone is `fact_missing`; file does not parse is `analysis_uncertain`;
  no declaration with that qualified name is `fact_missing`.
  For a `Modified` fact, no declaration with the same `full_fp` (or `sig_fp` for
  containers) is `same_symbol_edited`. For a `Referenced` fact, no declaration with the
  same `sig_fp` is `fact_broken`, with the detail `old signature => new signature`
  (each truncated to 100 characters). One exception keeps compatible calls valid: a
  referenced Python function whose header only gained optional parameters (defaults,
  `*args`, `**kwargs`, or keyword-only parameters with defaults after `*`), with every
  original parameter, the name, `async` and the return annotation unchanged, still
  holds (`symbols::python_call_compatible`). Rust signatures compare exactly.
- The analysis level is `symbols` when any signature fact's file was among the changed files,
  otherwise `files_only`. If the baseline repository cannot be read, L1 returns no
  reasons and `files_only`.

### L2: integration checks (`integration.rs`)

Run only at accept time, only when L0 and L1 said `Continue` **and** the world moved,
`coherence.integration_checks` is true, `checks.verify` is not empty, and, for the
local backend, the run itself was approved for local execution (the run's
`unsafe_local` flag). Otherwise the verdict is returned unchanged.

1. Copy the current source into a scratch directory under the run directory
   (`coherence-scratch-<id>`): for Git sources exactly the non-ignored files, so there
   is no build cache; for plain directories the whole tree. Nested repositories are copied whole.
2. `git apply` the patch there.
3. Run each `checks.verify` command with the run's execution configuration and
   timeout. Logs stay under `<run dir>/coherence-checks/<id>/`; the scratch copy is
   removed on every exit path.
4. Any check that did not pass adds a reason (`integration_check_failed` with the
   command, what failed in the check's own words and the log path, or
   `analysis_uncertain` if it could not run) and the verdict
   becomes `Refresh`. A failure to build the scratch tree or apply the patch also
   yields `Refresh` with `analysis_uncertain`. All passing sets the analysis level to `integration`.
   The words are an excerpt of the check's output: the first two lines that read
   like a failure (containing `FAIL`, `ERROR`, `Error`, `error:` or `panicked`, or
   starting with `assert`), stderr first, else the last non-empty line, for example
   `ERROR: test_admin_token (…) / TypeError: validate() missing 1 required positional
   argument: 'token'`.

There is no cooperative cancellation of these checks. They run while the per-run and
per-source apply locks are held, so another accept of the same run or source waits.

## The accept path

`apply_locked` (used by `dispatch accept`, the review view and `dispatch apply`) takes
the per-run and per-source locks and calls `coherence::gate`:

- The configuration frozen with the run (`config.snapshot.yml` in the run directory)
  is read. If it cannot be read or parsed, the gate is `Legacy` (strict).
- `coherence.accept: strict`, or a source whose whole-tree fingerprint equals the
  snapshot's: `Legacy`. The original all-or-nothing `safe_apply` runs. **Attached work
  (`run.mode == RunMode::Attached`) never takes the fingerprint half of this
  shortcut**: its S0 is a merge-base commit while its `source_fingerprint` is the
  root's working tree at attach time, so an equal fingerprint does not reliably mean
  an unmoved world the way it does for a run Dispatch launched. Attached work still
  goes `Legacy` under `coherence.accept: strict`. See
  [attach.md](attach.md#the-gate-rule-for-attached-runs).
- Otherwise `evaluate_run`, then L2 if the verdict was `Continue`. `Continue` yields
  `Compatible`; `Refresh` and `Stop` yield `Blocked`.

Attached runs go through this same `gate` and the same `auto_apply` as native work —
there is no second accept path for work Dispatch did not launch.

`Compatible` applies through `source::apply_validated`, which requires the current
world digest to equal the evaluated one before the dry run and again after it, so a
change during validation aborts with nothing applied. All-or-nothing `git apply`
semantics and the existing path safety checks are unchanged.

The human's accept/reject decision is recorded before the apply is attempted (it
is the quality signal and is not withdrawn when apply is blocked). When the gate blocks:

- nothing is applied and the source is unchanged;
- the run's application state becomes `blocked_by_source_drift`, the review stays accepted;
- the latest validity is stored in `run.coherence.validity`;
- an `application.failed` event carries `application`, `error` and `coherence`;
- the error text names `dispatch refresh <id>` or, for `Stop`, `dispatch reject <id>`.

When a moved-world apply succeeds, the validity is stored and the `result.applied`
event includes it under `coherence`. A typed error (`CoherenceBlocked`) replaced the
former matching on error text for this path.

## Automatic application (auto-apply)

Auto-apply applies a finished result to the source under a session or invocation
policy. It is never acceptance: acceptance is a human quality judgment, application
is a mechanical source change, and auto-apply performs only the second. An
auto-applied run never writes `review.accepted`; `outcome.review` stays `pending`
until a human looks at it, and nothing about the policy itself is persisted, only
its evidence. `orchestrator::apply::auto_apply` (`src/orchestrator/apply.rs`) is the
one function that performs it: the TUI session mode calls it right after a goal
reaches a Ready, review-pending result; `dispatch run --auto-apply` and
`dispatch refresh --auto-apply` call it right after the run returns.

The attempt has three stages, each recorded.

### Stage 1: eligibility

Checked before the source lock is taken. The run lock is acquired first (waited up
to 5 s). Every failure below returns `Skipped`, and every one except `run_busy` and
`not_ready` commits `auto_apply.skipped {reason}`.

| Check | Skip reason |
|---|---|
| The run's operation lock is free within 5 s | `run_busy` (nothing persisted) |
| `lifecycle == Finished`, `work_result == Ready`, `status == ReadyForEvaluation`, `review == Pending`, `application == NotApplied` | `not_ready` (nothing persisted) |
| Exactly one candidate | `not_sole_candidate` |
| `planning::verify_delivery` passes | `delivery_unverifiable` |
| `config.snapshot.yml` is readable | `config_unreadable` |
| `checks.verify` is non-empty | `verification_not_configured` |
| `outcome.verification == Passed` | `verification_failed`, `verification_inconclusive`, `verification_not_run`, or `verification_not_configured` |
| On the local backend, `environment.unsafe_local` is true | `integration_checks_unavailable` |
| The sole candidate's `diff_path` is non-empty | `empty_delta` |
| The source lock is free within the run's `execution.timeout_secs` | `source_busy` |

`verification_not_configured` is never eligible: without `checks.verify` there is no
L2, and the only evidence would be that the agent exited zero. No configuration key
relaxes this.

`empty_delta` keeps a run that produced no change — an agent (native or attached) that
edited nothing, or touched only ignored paths — reviewable instead of recording it as
applied with zero files changed.

`run_busy` and `not_ready` persist nothing because the run may be owned by a live
process holding no operation lock, or by a human review holding it; writing an event
here would bump the run's `state_revision` out from under that owner.

### Stage 2: the gate under both locks

With both locks held, `decide()` (`src/orchestrator/apply.rs`) calls
`coherence::gate` and applies a stricter authorization predicate than human accept:

| Gate result | Authorized | Else |
|---|---|---|
| `Legacy`, current `fingerprint_tree(source) == source_fingerprint` (byte-identical world) | yes, via `safe_apply` | |
| `Legacy`, fingerprint differs (strict mode with a moved world; a config that was readable in stage 1 and became unreadable here) | no | `auto_apply.blocked { reason: strict_mode_drift }` |
| `Compatible(v)`, `!v.world_changed` | yes, via `apply_validated` | |
| `Compatible(v)`, `v.world_changed && v.analysis == integration` | yes, via `apply_validated` | |
| `Compatible(v)`, `v.world_changed` and analysis is `symbols` or `files_only` (L2 skipped or disabled) | no | `auto_apply.blocked { reason: integration_evidence_missing }` |
| `Blocked(v)`, `v.decision == Stop` | no | `auto_apply.blocked { reason: verdict_stop }` |
| `Blocked(v)`, `v.decision == Refresh` | no | `auto_apply.blocked { reason: verdict_refresh }` |

A blocked outcome stores the validity (`remember_validity`), sets
`application: blocked_by_source_drift`, commits `auto_apply.blocked {reason,
coherence}`, and leaves `review` untouched. Nothing is applied and nothing is
rejected automatically.

This is the accept-time safety invariant plus one stricter rule: **a policy applies
only a verdict whose evidence is complete for the exact world it names** — an
unmoved world with passed candidate verification, or a moved world whose merged
tree passed the project's own checks. Human accept applies a moved world on L0/L1
evidence alone when L2 was skipped for lack of authority (see "The accept path"
above); auto-apply refuses that case (`integration_evidence_missing`).

### Stage 3: apply, and the one retry

`source::safe_apply` or `source::apply_validated` runs with the gate's digest. If
the digest fence fails — someone edited the source between authorization and the
real `git apply` ("source changed during apply validation" or "source has changed
since this run was created") — stage 2 is re-run exactly once against the new
world. A second fence failure ends as `auto_apply.blocked { reason:
world_moved_during_validation }`. There is no further loop and no sleep-and-retry.

Any other apply failure (for example a Git error after authorization) commits
`application.failed {application: "failed", error, applied_by: "auto_apply"}` and
returns `Failed { error }`; the policy itself is unchanged for later runs — a Git
failure is per-run, not a reason to silently stop applying.

### Locks

| Lock | Path | Bound |
|---|---|---|
| Run | `runs/<id>/.operation.lock` | 5 s |
| Source | `locks/source-<sha256(source_path)>.lock` | the run's `execution.timeout_secs` (an L2 run by another applier can take that long) |

Both are acquired with `OperationLock::acquire_wait`, a bounded poll (100 ms) of the
existing non-blocking `flock` acquisition; the non-Unix branch polls `create_new`
the same way. No SQLite transaction is held while either lock is held.

### `RunOutcome.applied_by`

`Option<AppliedBy>`: `human` or `auto_apply`
(`#[serde(default, skip_serializing_if = "Option::is_none")]`). `None` for a run
that was never applied, and for a record written before this field existed.
`apply_locked` sets it once, at the moment of application, from the caller's
`ApplyAuthority`; only `ApplyAuthority::Human` also sets `review: Accepted`, so an
auto-applied run's review stays `pending`.

### Post-hoc review

A human may still accept or reject an auto-applied run afterward. This is recorded
review only — there is no second apply, and rejection does not revert the source:

- `dispatch accept <id>` on an auto-applied run prints "Result was already applied
  by auto-apply; your review is recorded." and records `review.accepted`.
- `dispatch reject <id>` prints "Result rejected. The source tree was not changed."
  and "Rejection does not revert the source." and records `review.rejected`.

Both leave `application: applied` and `applied_by: auto_apply` unchanged; only
`outcome.review` moves off `pending`. This is the signal for a human later
disagreeing with a CONTINUE verdict that was acted on automatically.

## The mid-run watcher (`watch.rs`)

For native runs, the native engine starts one `Watcher` per attempt. It stops when the
attempt returns or the watcher is dropped. It evaluates again whenever the source or
the agent's work so far changed, so the order in which they change does not matter.

Attached work uses this same `Watcher` type, not a different mechanism: the wrapped
attach owner loop starts one for the agent it spawned, and the project owner
(`dispatch start`, or `serve` in the foreground) follows every foreign or orphaned
attached run on its own tick, evaluating when the source or that work moved. The
project owner also keeps the stored verdict of every Ready result awaiting review,
native or attached, equal to what `check` shows, recording it when the source
moves. All of them persist through the same
`apply::persist_verdict` step described above (`remember_validity` plus
`coherence.checked`/`coherence.invalidated`) — only `native::apply_watch` still
implements `mid_run: stop`, and it never applies to attached work. See
[attach.md](attach.md#shared-verdict-persistence) for the wrapped owner loop and
[the project owner](attach.md#the-project-owner-start-stop-serve-and-watch).

- Every `coherence.poll_secs` it takes `world::signal`: for Git, `HEAD`, the hash of
  `git status --porcelain=v2 -z --untracked-files=all`, and size and modification time of
  the reported files (at most 5,000); for plain directories a metadata walk. If the signal
  is unchanged nothing else happens.
- If it moved, it observes the world, writes the work so far as a patch
  (`source::snapshot_delta`, temporary Git index, no validation, to `delta-live.patch`
  outside the workspace) and runs L0 and L1. It never runs L2.
- `analysis_uncertain` reasons are dropped, and a `Refresh` that rested only on them
  becomes `Continue`, because a person may be mid-edit.
- The watcher never touches the database or the run record. It sends a message over a
  channel; the run's owning loop applies it (`apply_watch`) through the ordinary
  transition path, so there is a single writer of the run's revision.
- A message is sent only when the decision differs from the last one sent (a run is
  assumed valid until a message says otherwise), or when the verdict is still invalid
  for a different world digest. At most one message per 60 seconds; a held-back
  verdict is sent later unless superseded.
- The applied verdict is stored via `remember_validity`, which stamps `first_invalid_at`
  the first time a non-`Continue` verdict is stored.
- `mid_run: stop`: after `coherence.stopped` is recorded, the attempt's cancellation
  token is cancelled: `Stop` always stops, `Refresh` only with `stop_on_refresh: true`.
  The existing supervisor kills the process group and confirms cleanup. The run ends
  `Interrupted` with work result `cancelled`, `FailureKind::StaleWork` and exit code 1.
  If the goal deadline had already passed, the failure is `Deadline` instead. The partial
  patch is kept; no review is recorded (asserted by `tests/coherence_watch.rs`). A verdict that arrives after the attempt has
  ended is recorded but can no longer stop anything.

## Work against Work: interactions (`interactions.rs`)

Coherence judges Work against the World. While a project is watched, the owner
also compares every piece of Work that is not yet integrated with every other,
and reports where they already touch. It is advisory: it changes no verdict,
and it blocks, reorders, refreshes or stops nothing. When one piece lands, the
others are judged by coherence as before.

**Who takes part** (`serve.rs` `Owner::delta_of`), with the Δ that stands for
each:
- a delivered result awaiting review: its kept patch (frozen);
- attached Work whose workspace a runtime removed with its exact Δ kept: the
  kept patch (frozen);
- live Work, native or attached: its workspace now, snapshotted into the
  owner's scratch with a stat-cached index.

Applied, reviewed, closed and lost Work never lands, so it takes no part.

**Footprint** (`interactions::footprint`). Derived from each Work's own (S0, Δ)
through the facts layer; there is no second parser.
- **Symbol writes:** the declarations the Δ modifies, and whether each keeps its
  contract. The contract is what a `Referenced` fact relies on: the signature,
  or for Python a compatible call. A declaration can also be removed.
- **Symbol adds:** the declarations it introduces.
- **Symbol reads:** its `Referenced` facts.
- **File writes:** added and deleted files, and modified files without
  declaration-level analysis (an unsupported language, binary, symlink or
  mode-only, or an unreadable baseline).
- **File reads:** files its code names.
- **Text edits:** removed lines, insertions and hunk spans, against the S0 blob
  (`index <old>..`) of each supported file.

Identities are the root-relative path plus the qualified declaration name, so
Work with different S0s is compared by what it names.

**Rules** (`interactions::between`), strongest first:
1. **File:** a path either side judges as a whole, which the other touches in
   any way. When both write it, the entry is symmetric.
2. **Same declaration:** both write it.
3. **Uses:** one changes the contract of, or removes, a declaration the other
   reads. A change to a body alone is not reported: the other relies on the
   signature only, so the World check would not act on it either. Behaviour
   is the checks' business.
4. **Textual overlap:** the same file from the same S0 blob, where one's
   changed lines fall inside the other's hunk, context included, or both insert
   at one place. It is evidence of overlap, not a prediction that `git apply`
   fails. It is not evaluated across different S0 blobs, and it is not shown
   for a path where both change the same declaration.

Read/read never interacts, and neither do different declarations of one file
whose hunks keep apart.

**Mid-edit.** Live Work often has a file that does not parse for a moment.
Such a file never becomes a file-level interaction: the Work keeps its last
clean footprint (`last_seen`), or claims nothing yet (`pending`). Once the Work
is frozen, a file that does not parse is judged as a whole, as the facts layer
does.

**Cost.** Pairs are compared each tick: O(n²), and 1 ms for 50 pieces of Work
on this machine. A footprint is derived again only when that Work's Δ, or
whether it is frozen, changes; that costs about 150–260 ms on a 2,000-file
repository.

**Where it shows.** The view's rows, the interactive `watch`, `status`, and
`watch --json`/`status --json`: see
[attach.md](attach.md#the-project-owner-start-stop-serve-and-watch). The
owner's view of it (`watchers/<key>.interactions.json`) is disposable: it is
written only by the owner, trusted only while the owner holds the project, and
removed when it exits.

## Human override (`dispatch accept --despite-refresh`)

The file and symbol analysis can be more cautious than the work requires. A human who
has checked that the work still holds may apply it anyway:

- only a `Refresh` whose every reason is `fact_broken`, `fact_missing`,
  `same_symbol_edited` or `analysis_uncertain` (`coherence::overridable`). `Stop`,
  `patch_conflict` and a failed or unrunnable integration check are never
  overridable, and the refusal says so;
- an explanation is required (`--explanation` or `--explanation-file`);
- `coherence::gate` then runs `checks.verify` on the merged tree regardless of the
  analysis level (`AcceptGate::Overridden`); a failing check refuses, and so does a
  configuration where no check can run;
- the apply goes through `apply_validated` against the verified world digest, under
  the same locks and fences as any accept;
- `CoherenceRecord.overridden` keeps the overridden verdict, `coherence.validity`
  the verification, and a `coherence.overridden {coherence, explanation}` event is
  committed before the human review. The Work line shows the verdict `overridden`.

Auto-apply and `serve` never override. The review menu does not offer it; it is a
typed, explained command.

## Refresh (`orchestrator::refresh_request`)

`dispatch refresh [run]` requires a finished, Ready, unapplied run and returns a
`RunRequest` for a **new** run:

- the task is the original text with a fixed addendum naming the earlier run and up to
  ten reasons (from a fresh evaluation), replacing any earlier addendum;
- launch choices are repeated: the backend and timeout, and an explicit agent, model
  or effort (a run made before 0.4.1 without a profile redoes the agent that produced
  its result);
- `--allow-unsafe-local` and `--allow-forwarded-env` must be passed again if the original
  run needed them; without them the command fails before any launch;
- the new run's `coherence.refreshed_from` names the old run; the old run is unchanged;
- it goes through `run_dispatch` like any other run (a new launch, budget and funding check).

There is no automatic refresh and no retry loop.

## Events

Events are free-form `event_type` strings in the existing `events` table; every payload
also gets the run's `outcome`, as for all events.

| Event | When | Payload |
|---|---|---|
| `coherence.invalidated` | Watcher verdict is `Refresh` or `Stop`. | `{"coherence": <Validity>}` |
| `coherence.checked` | Watcher verdict is `Continue` after an invalid one was sent. | `{"coherence": <Validity>}` |
| `coherence.stopped` | `mid_run: stop` is about to cancel the attempt. | `{"coherence": <Validity>}` |
| `application.failed` | Apply refused or failed. | `application`, `error`, and `coherence` when the gate blocked it; `applied_by: "auto_apply"` on the policy path only |
| `result.applied` | Apply succeeded. | `files_changed`, and `coherence` when the world had moved; `applied_by: "auto_apply"` on the policy path only (absent, not `null`, on the human path) |
| `run.stopped` | After a watcher stop. | `failure: "stale_work"`, `reason` |
| `auto_apply.skipped` | An auto-apply attempt was not eligible (see "Automatic application" above). | `{"reason": <string>}` |
| `auto_apply.blocked` | The gate, or its stricter authorization predicate, refused an auto-apply attempt. | `{"reason": <string>, "coherence": <Validity or null>}` |

## Configuration

All keys are optional (`coherence:` in `dispatch.yml`); the block is not serialized when
every value is default. Unknown keys are ignored. The block is frozen with each run.

| Key | Default | Meaning |
|---|---|---|
| `accept` | `validate` | `validate` checks the moved source; `strict` restores the whole-tree refusal. |
| `mid_run` | `observe` | `observe` records verdicts; `stop` cancels the agent. |
| `stop_on_refresh` | `false` | With `mid_run: stop`, also cancel on `Refresh` (otherwise only on `Stop`). |
| `poll_secs` | `10` | Watcher interval; must be positive (validation error otherwise). |
| `integration_checks` | `true` | Run `checks.verify` on the merged tree when the world moved. |

## Persisted or recomputed

Recomputed on every use: the world, symbol tables, facts and the verdict shown by
`status`, `check`, `explain` and the control `result`. Accept never trusts a stored verdict.
Interactions between Work are derived by the project owner from each Work's (S0, Δ).
The file it keeps them in is disposable and removed when it stops; nothing about them
is stored in the database or the run.

Stored: `RunRecord.coherence` in the run projection JSON (`runs.run_projection_json`
and `run.json`): `refreshed_from`, the latest stored `validity` (with its world
digest) and `first_invalid_at`. History is in the `events` table. The `facts` field
exists on the record but current code does not populate it; facts are always
recomputed from the baseline and the patch. Coherence data is not part of any sync envelope.

`RunOutcome.applied_by` is also stored (`human` or `auto_apply`, `None` when
unapplied). It is set once, at application, and never recomputed; a later post-hoc
`review.accepted`/`review.rejected` does not change it.

There is no schema migration; the schema version stays 20. Every field is
`#[serde(default)]`, so a `run.json` written by an older version deserializes, and
such a run gets coherence checking at accept time because everything needed
(baseline, patch, current source) already exists.

## Safety properties preserved

- No SQLite write transaction is held across parsing, `git` or checks; evaluation itself writes nothing to the database.
- The watcher never writes state, and there is one writer of the run's revision.
- Evaluation is read-only (`git apply --check` only); the scratch tree is separate from the source and removed afterwards.
- The apply is guarded by the evaluated world digest, before and after the dry run.
- Strict mode and unmoved sources use the pre-existing `safe_apply` unchanged.
- Nothing launches an agent except an explicit `run` or `refresh`; refresh needs the same acknowledgements again.
- Local execution is still not a sandbox; integration checks add no authority beyond the run's own approval.
- The original-baseline lineage, completed-attempt evidence, the launch record and cancellation fencing are untouched. Coherence-stopped runs are not counted as agent failures.
- Auto-apply reuses `gate` and `apply_validated`/`safe_apply` verbatim; it adds a
  stricter authorization predicate on top, never a weaker one, and applies through
  the same digest and fingerprint fences as human accept.
- Auto-apply never records a review: it never writes a review revision or
  `review.accepted` and never changes `outcome.review`. Human evidence stays human.
- A run whose operation lock is held by another owner (a live process, or a human
  review) is left untouched by an auto-apply attempt: nothing is persisted and
  nothing is applied out from under that owner.
- The auto-apply policy itself is never persisted; only what happened is
  (`applied_by`, the `auto_apply.*` events). A process restart loses the policy, and
  no stored flag authorizes a later, unattended application.

## Fixture matrix

`tests/coherence_matrix.rs` (Unix only) builds real repositories, takes a real Dispatch
snapshot, edits a real candidate workspace, collects the real patch, moves the source, and
asks three detectors: **strict today** (whole-tree fingerprint), **file overlap** (a changed
file is also a patch file) and **coherence** (`coherence::evaluate`). It has 36 scenario entries,
each run on a Git source and a plain-directory source (some in Rust and Python variants):
unrelated changes, same-file edits, callee signature and body changes, struct field
changes, transitive behavior, a JSON schema file, an identical patch already landed,
same-symbol edits, ignored build output, syntax errors, deleted and renamed files,
binary, mode and symlink changes, nested repositories, and Python/Rust class, trait and
impl cases.

```sh
cargo test --test coherence_matrix -- --nocapture
cargo test --test coherence_matrix -- --ignored --nocapture   # timing report only
```

The first prints a table per row (`strict`, `overlap`, `coherence`, `expect`, `status`,
changed-file count, evaluate and observe milliseconds) and a summary per detector.
The JSON report is written to `target/tmp/coherence-report.json`.

How to read it:

- **FC** (false continue) means the detector would let stale work through; it is the
  dangerous error. The test requires 0 for coherence.
- **FR** (false refresh) means it would block valid work; it costs a rerun. The test
  requires 0 for coherence and strictly fewer than file overlap.
- **Invalidation recall** is the share of rows that should be invalidated and were.
- `status` is `ok`, `FAIL`, `pending` (expected verdict not yet reachable) or `known_gap`
  (reported, not asserted). Currently every row is asserted.
- The matrix runs L0 and L1 only. Row 07 (transitive behavior change) expects `CONTINUE`
  on purpose: it is what integration checks are for.
- The matrix measures a designed set of scenarios, not real-world precision. It says nothing about repositories or edit patterns it does not contain.

## Known limits

- Symbol facts: Rust and Python only. Other languages get file-level facts; any change to such a file is `REFRESH`, even a harmless one.
- Referenced symbols bind by unique base name, not by name resolution; ambiguous, very common and short names are skipped. Names left unbound, and files whose baseline could not be analysed, are counted internally but not reported.
- Same-symbol concurrent edits are `REFRESH` (`same_symbol_edited` or `patch_conflict`).
  A Python edit that only appends after the last line of a function is not seen as a same-symbol edit by the symbol layer; the patch check and your checks still apply.
- Transitive behavior changes are caught only if `checks.verify` covers them.
- Integration checks use your `checks.verify` on a scratch copy of the non-ignored files with no build cache
  (a `cargo test` builds from scratch unless the command points `CARGO_TARGET_DIR` elsewhere), hold the apply locks while running, and cannot be cancelled.
  They do not run for a local-backend run that was not approved for local execution.
- `dispatch check`, `status` and `explain` evaluate L0 and L1 only; integration checks run at accept. When accept refused a result on the merged tree, they show that refusal for as long as the source is unchanged, and `refresh` passes its reason to the new agent. Once the source moves, they show the fresh L0 and L1 verdict until the next accept runs the checks again.
- Plain-directory sources cannot honor `.gitignore`; ignored build output counts as world change (the patch usually still applies).
- Nested repositories and submodules are not analysed.
- A source that moved since S0 does not stop a native run, including the continuation after a clarification answer: every attempt starts from the run's original snapshot, the mid-run watcher reports what the move means for the work, and the accept gate judges the result against the source as it is then. (Before 0.4.4 a continuation stopped with `source_drift` whenever the tree differed.)
- The mid-run watcher covers native runs and wrapped attach, and observes by default. Every `poll_secs` it builds the work-in-progress patch with a temporary Git index (taken with the trusted baseline repository, so ignored build output never counts) and evaluates again when either the source's signal or that patch changed. A change that landed before the agent touched the same code is therefore reported once the agent touches it.
- Agent time after invalid is wall-clock time from attempt timestamps; it is not cost, and it is only meaningful for a run whose watcher stored an invalid verdict during the attempt.
- Apply is not crash-atomic: a crash between `git apply` and the database update leaves patched source and an unapplied run (pre-existing).
- Interactions between Work are advisory. They exist only while the project is
  watched, and they are exact to the declaration only for Rust and Python;
  other files interact as whole files. A change to a body alone is never an
  interaction, even when behaviour its callers rely on changes. Referenced
  declarations bind by unique base name, so a wrong binding can report a use
  that is not one. Their precision on real repositories is not measured yet.
- Evidence is from fixtures and a small number of runs. False-refresh and false-continue rates on real repositories are not measured. What is claimed today, what is recorded during real use, and what would falsify the thesis are in [coherence-validation.md](coherence-validation.md).
