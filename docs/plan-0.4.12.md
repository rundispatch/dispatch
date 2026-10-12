# Plan: Dispatch 0.4.12, safe review and honest source warnings

**Approved for execution 2026-10-12, after two Opus reviews (§8).**
Inspected source: `18a543b`, Dispatch 0.4.11. The user selected the 0.4.12/0.4.13
split after the first review.

## 1. Release objective and scope

Make reviewing the intended Work safe, warn when an external agent started from
older source, and give people a way to close Work without recording rejection.
Preserve truthful S0, frozen patches, source guards and human preference data.

For **0.4.12**, implement the confirmed correctness and recovery fixes: specific
Work IDs in hints; refusal of ambiguous ID-less decisions; honest finish/verdict
and verification-scope output; bounded source warnings; explicit close; repaired
workspace recovery; and clearer cleanup output.

Reserve **0.4.13** for automatic discovery, liveness observation and project
setup. Out-of-box discovery is an explicit user objective and remains planned.
Opus's objection that hooks were not unwanted does not remove that objective;
the reason for this split is the larger discovery/lifecycle contract and the
need to inspect existing runtime signals first.

0.4.12 warns and steers people to Dispatch's existing managed launch, which
inherits current source. That launch (`dispatch attach -- <agent>`) works for any
agent command, including harnesses that make no worktrees of their own, so it is
the advice Dispatch gives first; anything runtime-specific comes from that
runtime's adapter. It does not make `claude --worktree` inherit dirty source
or replace Claude's worktree creation. No automatic commit, push, fetch, refresh,
retry, apply, foreign-process control or new execution path.

## 2. Evidence and corrected diagnosis

The affected project's Dispatch/Claude versions and original artifacts are still
unknown. Record them before attributing every field report to this source version.

| Report or review finding | Established behavior | Proposed response |
|---|---|---|
| Accepted work absent from later agents | Accept does not commit. HEAD-based worktrees miss dirty source; remote-based worktrees can also miss unpushed commits. Managed launches capture current source | Warn about actual source divergence and recommend managed launch; do not rebaseline an old workspace |
| Discovery and foreign attach have different S0 | Hook registration captures the actual workspace at session start; foreign attach uses its merge base | Preserve both honest provenance forms; no switch from exact start capture to a historical base |
| Finish hints can accept different Work | Both finish and native result summaries print ID-less review commands; ID-less accept/reject select the newest pending Work for the source | Always print the relevant ID; refuse a mutating decision when multiple Work items match |
| Finish says ready while check says REFRESH | Finish summary describes workspace checks without checking current applicability | Show a fresh static verdict and check scope; leave merged checks at accept |
| Merged verification appeared absent | Accept's existing oracle skips checks for several reasons, including missing recorded host authority | Inspect the actual record/logs and expose whether checks ran or were skipped, with the reason |
| Verdict appears frozen | Check reevaluates source now; a finished workspace edit does not replace its retained patch | Distinguish source repair from a new delivery; close superseded Work during explicit recovery |
| Rows remain working after exit | End hooks record sessions but do not finish Work; watch already has an idle projection for recorded session end | Explain ended sessions and offer finish/close. Investigate missed end evidence before adding liveness tracking |
| Landed code is labeled rejected | STOP hints currently recommend reject. `close_removed` already emits non-preference `work.closed` | Generalize close and stop recommending rejection merely to retire Work |
| Full IDs seem necessary | Unique, case-insensitive prefixes already resolve; finish already requires an ID | Fix missing-ID hints and ambiguous selection, not a new prefix mechanism |
| Clean leaves Claude worktrees | Cleanup removes only Dispatch-owned workspaces | Explain retained external worktrees; preserve ownership boundary |
| Setup/discovery friction | Hooks currently provide discovery; standing check consent and check suggestions already exist | Keep the user objective in 0.4.13; reuse registration and consent, informed by P0 |

Two temporary CLI fixtures on installed 0.4.11 established:

- Accept changes source without moving HEAD. A new HEAD worktree has the old file;
  a root-wrapped launch's baseline has the accepted file.
- The same frozen patch changes REFRESH → CONTINUE after compatible source is
  restored. A unique 12-character run prefix resolves.
- Create A, then B; finish B, then A. A's summary suggests `dispatch accept`.
  Following it applies **B**, leaves A pending, and records acceptance for B.
  This is a reproduced release blocker.

The earlier planning run of
`cargo test --offline --test runtime_hooks --test coherence_integration --test attach_cli --test serve`
passed **53 tests**, with **2 measurement tests ignored**. These are existing
guarantees, not validation of the proposed fixes. No production code was changed.

Additional evidence checked after Opus's feedback:

- This repository has 15 linked worktrees, including 14 `agent-*` directories;
  all 15 have `CLAUDE_BASE` markers and none currently has a Git lock.
- Claude documents `fresh` as a remote-default base and `head` as local HEAD;
  fallback behavior depends on available local refs. Consequently, committing
  alone is not a general cure. [Claude worktree base documentation](https://code.claude.com/docs/en/worktrees#choose-the-base-branch)
- Claude also documents resumable sessions, subagent/background worktrees and
  locks left after noninteractive runs. A lock or marker alone cannot establish
  current agent liveness. Marker contents are not an S0 contract.
  [Claude worktree lifecycle documentation](https://code.claude.com/docs/en/worktrees#clean-up-worktrees)

## 3. P0: collect the facts that determine the fix

Collect available original evidence, without modifying the affected project or
fetching remote refs. If unavailable, keep unknowns explicit and continue with
the confirmed reproductions.

1. Dispatch and Claude Code versions, launch commands and effective `baseRef`
   setting, including which settings file supplied it.
2. Whether a remote exists, local root HEAD, cached `origin/HEAD`, workspace HEAD
   and its creation base. An absent ref remains absent; do not fetch to fill it in.
3. Root Git status when Wave 2 began: accepted changes still dirty versus committed
   but unpushed. Compare relevant source contents with the workspace, not just
   commit labels.
4. Recorded S0/provenance, frozen Δ and application events for both waves.
   Exact hook S0 includes preexisting uncommitted files and Dispatch's exclusions.
   A difference from creation HEAD alone does not prove bad capture; compare with
   the actual source visible when capture happened. For a clean start, equality
   with the creation tree is a useful sufficient check.
5. Frozen verification commands, backend, `unsafe_local`, integration-check
   policy, source digest and `coherence-checks` logs. Determine whether accept
   actually ran merged checks and why it might have skipped them.
6. For the `d` report: terminal dimensions, key sequence, selected Work, error and
   source changes during review. Reproduce with a PTY fixture where possible;
   otherwise retain an open investigation rather than invent a reviewer fix.
7. For the later discovery design: `CLAUDE_BASE` presence and Git lock behavior
   during start, exit, resume, backgrounding and process death on the relevant
   Claude versions. Inspect existing signals before proposing process scanning.

Keep raw observations and artifacts. Do not infer missing session times, models,
token usage, cost, human preferences or historical landing times.

## 4. 0.4.12 contracts

### A. Every review decision identifies its Work

- Native result and finish summaries print `diff <id>`, `check <id>` and eligible
  accept/reject/close commands with that same full ID. Audit other actionable
  hints for the same omission, including runtime hook replies (`src/runtime.rs`,
  `removed`); no broad prefix refactor.
- Explicit IDs retain existing unique-prefix resolution. Resolve once and use
  the canonical full ID for operation locks, writes and events.
- ID-less accept/reject may proceed only when exactly one eligible Work item for
  the source exists. Eligible means finished, ready, pending review and
  unapplied. With multiple matches, refuse without mutation and list names,
  unambiguous IDs and explicit commands; the refusal may also list in-flight
  Work, which does not count toward the match. With none, report none.
- Finish already requires an ID; keep that requirement. Read-only latest-run
  conveniences may remain, but show which Work they describe.
- Keep stable selection and review revision/candidate guards in `watch` and `d`.

**Regressions:** the reproduced A/B finish ordering; ID-less accept and reject
with zero/one/two results; explicit A acts on A; overlapping prefixes refuse;
prefix commands write full-ID events; every summary command targets its result;
PTY selection and concurrent delivery preserve the intended review target.

### B. Source warnings are accurate and bounded

Preserve exact hook-start S0, foreign merge-base S0 and managed current-source
capture. Existing S0/Δ never changes to make a result appear compatible.

The warning is about the start: this Work began from source that differed from
the integration source at that time. Compare in both directions:

- the workspace lacks changes present in the source, such as accepted dirty
  changes, or local commits missing from a remote-based workspace;
- S0 contains changes the source lacks, such as a session resumed in a workspace
  whose earlier Work was closed or rejected without landing.

Name applied or earlier Work only where retained evidence supports attribution;
otherwise describe divergence without guessing its author or treating it as a
new coherence verdict.

Make the observation once, as close to the start as possible, and record it as
an event together with the source state it compared against. It is not
recomputed later: drift after the start is the coherence verdict's job, and a
later comparison against moved source cannot say what the source held at the
start. If no owner observed the start, the warning is "not observed", never
synthesized at `status`/`check` time. Registration must not promise a warning it
has not computed.

Use existing Git/world primitives and recorded patch paths to limit comparisons.
Do not recursively fingerprint the root, walk nested worktree build output or
build a persistent applied-file index. Broad comparison work belongs in the owner
loop, after durable publication, not inside the 40-second hook registration
budget. Reuse its loaded project runs; one observation per Work means no
repeated notices.

The warning leads with the managed launch, `dispatch attach -- <agent>`, which
inherits current dirty source for any agent. Runtime-specific advice comes from
that runtime's adapter and appears only for its workspaces: for a Claude
worktree, explain that local HEAD plus `baseRef: head` carries commits, while
neither a commit nor that setting copies dirty source by itself. Never change
settings, commit or push in response to the warning. Concurrent work is allowed;
warnings remain advisory.

**Regressions:** accepted/uncommitted A → managed B sees A and its new Δ excludes
A; an old external B warns; local commit ahead of cached remote; no remote; linked
integration root; unrelated source edits; reverted accepted changes; correct
initial S0 with inherited dirty files; ignored/nested build churn; bounded warning
work outside registration; owner absence and restart do not lose capture; a
session resumed after closed, unlanded Work warns of the reverse divergence; the
observation is recorded once and not recomputed against later source; no owner at
the start shows "not observed"; a non-Claude workspace gets no Claude advice.

### C. Finish states applicability and verification scope

Retain Δ at finish even if it cannot currently apply. Compute a fresh **static**
verdict using the existing evaluator, then describe:

- the Work ID and frozen patch;
- workspace verification results, including no checks or failure;
- current applicability and concrete reasons;
- that merged checks run at accept when enabled and authorized.

REFRESH does not recommend accept. STOP recommends close for already-present
changes. CONTINUE does not imply merged checks have passed. Retain full Git
diagnostics as artifacts and show a useful bounded conflict explanation/path.

Keep merged verification at accept. Expose passed/failed/skipped status and the
reason for a skip, including disabled policy, no commands, unchanged source or
missing recorded host authority. Do not silently present a static verdict as
merged verification. Preserve existing authorization and apply eligibility;
changing that policy requires a separate decision supported by P0.

The accept gate still reevaluates source now and guards the digest between checks
and apply. A stored integration failure in the same world remains visible; an
explicit accept retry can rerun checks. No finish-time merged preflight or new
verification cache.

**Regressions:** finish/check agreement on applicability; workspace pass followed
by merged failure at accept; skipped checks with each reason; complete retained
diagnostic; source repair changes verdict; same-world failure retained; source
movement during checks blocks apply; `watch → d → review → accept/refusal → watch`
keeps Work identity and evidence. Fix `d` only for a reproduced defect.

### D. Close retires Work without a quality judgment

Extend the existing `work.closed` mechanism with one proposed command:
`dispatch close <id> --reason <reason>`, with `--replacement <new-id>` for
`superseded` and an optional human explanation.
Reasons cover unowned unfinished Work, removed/lost workspace, already-present
changes and an explicitly replaced delivery. Reuse operation locks, snapshots and
event persistence; do not add a separate `landed` lifecycle.

- Close never applies, commits, kills an agent or deletes a foreign workspace.
  For unfinished Work, preserve settled changes when its workspace exists and
  exact versus last-seen evidence when it is gone. For a delivered result, retain
  its original frozen Δ even if the workspace was subsequently repaired. Refuse
  an active Dispatch wrapper/native owner. Unknown foreign liveness is not proof
  of exit; human closure retires tracking rather than controlling the session.
- `already_present` checks presence of the full retained patch in current source
  and records the observation/digest. Display “closed: changes already present,”
  with a human explanation if supplied. Partial presence is not sufficient.
  This establishes presence, not authorship or the historical integration time.
- Emit no acceptance/rejection preference. Preserve earlier review events and
  evaluations; do not erase a prior human rejection or mark an already-delivered
  result as a failed agent attempt simply because it was closed.
- Replace STOP's reject advice with close in CLI, `watch` details and notices.
  Offer `c close` for the selected eligible Work; `r reject` remains a real human
  rejection. Use the same selected-ID guard as other actions. Because Ctrl+C
  quits `watch` and `a` acts without a prompt, `c` asks for the reason and a
  confirmation, as `r` does, before anything is recorded.
- Consolidate the existing removed-workspace close path into this operation;
  remove obsolete duplicate mechanics and update its misleading reject hints.
  Today `reject` on unfinished Work whose workspace is gone closes it (`decide`
  → `close_removed`). After consolidation, `reject` on unfinished Work refuses
  and names `dispatch close <id>`, in the CLI and in `watch`'s `r`.
- Do not invent before/after interaction observations or include unobserved
  external landings in the existing precision measurements.

Session exit alone remains resumable. When end evidence is recorded, watch/status
say “session ended — finish to freeze its changes, or resume”; offer close as an
explicit alternative. Preserve automatic finishing on exact workspace removal
with valid consent. No new process scanning or auto-finish on ordinary exit.

Repair recovery is explicit: reattach and finish the repaired workspace as new
Work; confirm its new Δ was retained; close old Work with reason `superseded` and
the new ID; then review the new result. New capture failure leaves old Work open.
Until closure, ID-less decisions refuse the two pending results under contract A.
Preserve both original patches and the relationship. Use close and existing
events, not a new automatic recovery/orchestration engine. A source-only repair
requires only rechecking the same frozen Work.

**Regressions:** close idle/unowned Work with changes and no changes; removed/lost
evidence retained; live owner refuses; `/exit` then resume keeps unsealed Work;
`/clear` does not finish; already-present/full versus partial patch; prior review
history and feedback counts unchanged; superseded old result is no longer pending;
failed replacement leaves old open; concurrent close/finish cannot lose Δ;
`reject` on unfinished Work refuses and names close; `c` without confirmation
records nothing.

### E. Cleanup reports ownership

Keep removal restricted to workspaces Dispatch made. Dry-run and normal output
explain terminal external/runtime worktrees retained, their Work IDs, and how to
inspect/remove them using Git or their runtime. A “nothing to clean” message must
not imply all agent worktrees were removed. Avoid destructive force-removal hints
and use caution for locked worktrees whose runtime may still own them.

A workspace Dispatch made is cleanable only when no unfinished or unreviewed
Work is attached to it. Today `cleanable` (`src/orchestrator/attach.rs`) lists a
managed workspace once its *own* Work is over, `closed` included, without
looking for other Work on the same path. Following D's repair recipe on a managed
workspace (reattach it as new Work, close the old Work as `superseded`) would
otherwise let `dispatch clean` delete the directory the new Work is using.

**Regressions:** clean removes eligible managed workspaces, preserves every
external workspace and branch, and explains retained terminal work accurately; a
managed workspace reattached as new Work survives `clean` after its old Work is
closed as `superseded`, and is cleanable once the new Work is over.

## 5. Packets and Dispatch-coordinated execution

Work serially: **P0 → P1 → P2 → P3 → R**. One explicitly chosen agent per
packet. No tournament, automatic subtask orchestration or opportunistic parallel
work. Close (D) lands in P1 because A's hints and C's STOP advice name it.

| Packet | Scope | Principal code / focused validation |
|---|---|---|
| P0 | Original evidence and minimal reproductions; inspect runtime signals for the later release | Existing CLI/PTY fixtures; `attach_cli`, `coherence_cli`, `runtime_hooks`, `review_session`, `watch_ui` |
| P1 | A + D: specific targets, ambiguity refusal, unified close, supersession recipe, ended-session hints | `main.rs`, `orchestrator.rs`, `attach.rs`, `runtime.rs` replies, review/outcome projection, watch; A/B decision, closure/history/measurement tests |
| P2 | C + E: truthful finish/check and verification scope, STOP → close, cleanup ownership | `attach.rs`, `orchestrator.rs`, `coherence/integration.rs`, presenters, clean; review journeys and clean regressions |
| P3 | B: advisory start-time source warnings without recapture or expensive registration | Existing source/world/owner paths, the Claude adapter for its advice, registration only for necessary wiring; two-wave/baseRef/churn scenarios |
| R | Integrated CLI/PTY trial, docs, version/build metadata and release review | Full standard validation; final diff, production LOC and public-claim audit |

Use [self-hosting.md](self-hosting.md) with a pinned released **0.4.11** supervisor
outside build directories, a dedicated integration clone/release branch, durable
supervision state, and separate candidate-test state. Resolve paths and branch
name in the execution log; preserve this checkout's untracked `.claude/` files.

Configure fmt, full tests and clippy as explicit host checks. With the supervisor,
retain its existing `--allow-unsafe-local` requirements. New candidate behavior
cannot be a prerequisite for building it.

Launch root-wrapped interactive workers. The wrapper provides an independent
current-source workspace and passes the terminal through, so the person can
approve the agent's permission prompts. Noninteractive `claude --print` requires
independent preauthorization of its tools; it is not the default execution recipe.
Do not add another agent `--worktree` inside Dispatch's managed workspace.
Wrapped workers need no Dispatch hooks, so the integration clone carries none for
this release: a hook session would join its wrapped Work and could show it idle
while the wrapper still runs.

```sh
cd "$integration"
"$supervisor" --state-dir "$supervision_state" start --root "$integration"
"$supervisor" --state-dir "$supervision_state" attach \
  --root "$integration" --task "0.4.12 P1: safe review" \
  --agent claude --allow-unsafe-local -- claude

# Give the interactive worker the approved packet instructions.
# After it exits, use its returned full Work ID throughout the execution log.
"$supervisor" --state-dir "$supervision_state" status "$work_id"
"$supervisor" --state-dir "$supervision_state" diff "$work_id"
"$supervisor" --state-dir "$supervision_state" check "$work_id" --json
# After human review/approval:
"$supervisor" --state-dir "$supervision_state" accept "$work_id"
# Inspect/stage only the reviewed files; explicitly commit before the next packet.
```

Use independent Cargo target directories outside source. Keep handoff reports
outside candidate source so they do not enter Δ. Review each frozen contribution
and its logs; a human decides accept/revise/reject. Recheck outstanding Work after
each landing. No auto-apply; no rejection used merely to retire landed Work.
Test candidate binaries only with separate state. Preserve failures and raw data;
unknown usage/cost stays unknown and no savings or agent-quality claims are added.

## 6. Release acceptance

Demonstrate the specified regressions and these complete journeys:

1. Two pending results: every hint/review acts on the intended one; ID-less
   accept/reject refuses ambiguity without changing source or human feedback.
2. Accepted dirty source and unpushed commits: external divergence warnings are
   accurate; managed launches inherit current source; original S0 stays intact.
3. Finish on stale work: scope/verdict/hints are truthful; accept runs or explains
   skipped merged checks; failed merged checks and source movement prevent apply.
4. Source repair versus workspace repair: recheck works; explicit replacement
   closes old Work while retaining both deliveries and their history.
5. Recorded end, resume, close and removed workspace: no ordinary-exit auto-finish,
   no synthetic rejection, no lost patch and no foreign process/worktree deletion.
6. CLI and `watch → d` review journeys preserve target, source and evidence.
7. Cleanup explains external worktrees retained and only removes owned ones.

Run `cargo fmt --check`, `cargo test`, and
`cargo clippy --all-targets -- -D warnings` on the integrated result. Inspect final
diff, production LOC growth, migrations/fields and docs. Reuse existing code and
delete replaced close mechanics. No unrelated refactors or synthetic quality
labels. Wrong-Work decisions, lost Δ, dishonest S0, unauthorized host checks,
source modification after a blocked apply, or `clean` removing a workspace that
live Work uses are release blockers.

Upgrade a real 0.4.11 state: build `v0.4.11` from `git archive`, make a state
with a removed-workspace `work.closed`, pending results and an applied result,
then open it with the candidate. History, outcomes and feedback counts must read
unchanged after the close consolidation.

The release notes disclose that before 0.4.12, an ID-less `accept` or `reject`
with several results pending could record a human decision on Work nobody chose.
Existing records are not rewritten.

## 7. 0.4.13 follow-up: out-of-box observation and setup

This preserves the user's original goal; it is not a claim that 0.4.12 delivers it.
Dispatch must not become Claude-only: "out of the box" covers both discovering
worktrees a runtime makes itself and making one, through `dispatch attach --
<agent>`, for a runtime that makes none. Write its implementation contract from
P0 evidence and explicit product choices:

- Hook-independent observation must solve startup/discovery friction through the
  same registration/publication/evaluation core. Hooks enrich exact start and
  removal evidence, rather than create a competing execution path.
- Where a runtime makes its worktrees, and how to recognize one (for Claude,
  `.claude/worktrees/` and its marker), belongs to that runtime's adapter beside
  its hooks. Discovery in the core asks the adapters; a second runtime adds a few
  lines to its own adapter, not a plugin framework. The `.claude/worktrees`
  ownership test in `attach.rs` and the shared-checkout notice in `runtime.rs`
  move behind the same boundary.
- Dispatch-made workspaces are built like `git stash create`, so ignored files
  such as `.env` or local harness configuration do not reach them. Claude solves
  this with `.worktreeinclude`. Decide Dispatch's equivalent from real friction
  before building one.
- Record worktrees already present at first watch and offer explicit attach hints;
  do not auto-create idle rows for old directories. Persist that initial inventory
  across owner restarts. Newly appearing eligible worktrees may become Work.
  Define behavior when work starts in an existing workspace after watching begins.
- Decide whether independent subagent/background worktrees are separate Work.
  Proposed default: list ambiguous/nested subagent worktrees as attachable, without
  guessing independent task attribution from an `agent-*` directory name.
- Inspect marker/lock signals first. A marker establishes a hint of provenance,
  never S0; a stale/manual lock cannot establish liveness. Add process association
  only if observed failures require it, with unknown states kept unknown.
- Build an active-workspace set once from the project's loaded runs per scan;
  do not call the all-runs `find_active_attachment` for every worktree on every
  tick. Keep snapshots, deduplication and warning work bounded.
- Show observed live file counts and elapsed tracking time without presenting
  unset delivery fields as zero files or zero runtime. Unobserved process runtime
  remains unknown.
- Do not assume scanning a late workspace captures its true start. Explain the
  merge-base recovery scope and confidence. No automatic rebaseline, repeated
  rediscovery of finished directories or loss of exact-versus-last-seen evidence.
- Separate project setup from funding/model setup. Non-TTY setup can save exact
  verification commands but **cannot grant standing host authority**. Retain the
  existing explicit terminal consent, exact-command binding, state storage and
  revocation. Say checks execute project code, including code the agent wrote.
- Reuse valid human consent where appropriate rather than repeat flags, preserve
  source/settings, and maintain only necessary local excludes. No implicit Git
  init, initial commit or dependency installation; managed directory/unborn-source
  support remains available.

## 8. Opus feedback disposition and remaining decisions

Adopted in this revision: expanded baseRef/remote evidence; the reproduced missing
ID safety fix; unified non-preference close; visible merged-check scope; bounded
warnings outside registration; interactive wrapped workers; no ordinary-exit
auto-finish; no finish-time merged checks; no non-TTY standing-consent grant;
superseded Work closed during repair; a narrower product promise.

Qualified two review points: S0 can legitimately differ from creation HEAD when
preexisting dirty source was visible; Git markers/locks can outlive a process.
Neither alone proves snapshot corruption or running-agent identity.

Adopted from the second review: close moves to P1 because A and C name it; a
managed workspace with live Work attached is never cleanable; the source warning
is a single start-time observation in both directions, "not observed" without an
owner; `reject` stops closing unfinished Work; eligibility for ID-less decisions
is defined; `c` confirms; a 0.4.11 state upgrade test; release-note disclosure of
past ID-less decisions; no Dispatch hooks in the integration clone. Harness
neutrality: the warning leads with the managed launch, runtime-specific advice
and worktree recognition live in the runtime's adapter, and 0.4.13's goal covers
runtimes that make no worktrees.

The user selected the **0.4.12/0.4.13 split**. Discovery/setup stays in the follow-up
release, with the open attribution and observation contracts in §7. Remaining P0
unknowns include original versions/settings, whether merged checks ran, and the
`d` reproduction. Lack of old artifacts does not block fixes already confirmed
on current code.

Deliberately not built in 0.4.12: a worktree-creation replacement hook, automatic
source transplantation, process discovery, host-consent redesign, persistent
indexes, runtime plugin framework, automatic foreign cleanup, cloud services,
uploads, new language analyzers or speculative orchestration.

## 9. Execution log

### Setup (2026-10-12)

| Item | Value |
|---|---|
| Supervisor | `~/dispatch-dev/dispatch-0.4.11/dispatch`, `dispatch 0.4.11`, read-only; archive checked against `SHA256SUMS`; binary SHA-256 `1adc057028cca0c7d0a0dbe41a2c356f585c83ee900f190d6aa3645b502a6260` equals `BUILD.json`; `BUILD.json` head `18a543b` |
| Supervision state | `~/dispatch-dev/state-0412` (mode 700) |
| Integration clone | `~/dispatch-dev/integration-0412`, branch `release-0.4.12` from `origin/main` at `18a543b`, no upstream set; `dispatch.yml` and `.claude/` excluded through `.git/info/exclude` |
| Hooks | None in the clone. User-wide hooks in `~/.claude/settings.json` report to `~/.dispatch`, which does not watch the clone, so they stay silent for workers |
| P0 fixtures | `~/dispatch-dev/p0-0412/repro.sh`, output `repro.log`, own state `p0-0412/state` |

### P0 results

The affected project's original versions, settings and artifacts were not
available to P0. Items 1–4 and 6 of §3 remain unknown until the reporter supplies
them; item 7 (runtime signals) is for 0.4.13. Every reproduction below ran against
the pinned 0.4.11 with deterministic shell edits and no agent.

| # | Scenario | Observed on 0.4.11 | Contract |
|---|---|---|---|
| R1 | Attach A then B; finish B, then A; run A's suggested `dispatch accept` | A's summary prints `Then dispatch accept / dispatch reject` without an ID, then `Next: dispatch check <A>`. The ID-less accept applied **B** (`b.txt`), recorded acceptance for B, and left A `ready` | A |
| R2 | Agent and root both change `c.txt` line 1; finish | finish prints `Ready for review … Then dispatch accept`; `check` says REFRESH `patch_conflict` | C |
| R3 | Accept with checks and an unmoved source; with checks and an unrelated source change; with no checks and a moved source | All three print the same `Applied … Review recorded`. Only the second ran merged checks (a `coherence-checks/` directory exists). Nothing says whether merged checks ran or why not | C |
| R4 | Wrapped Work W; repair its managed workspace; attach the same workspace as new Work N; reject W | `clean --dry-run` lists W's workspace for removal (`rejected`) while N is `working` in it | E |
| R5 | The Work's change is applied to the root by hand | `check` says STOP `already_applied`, `Next: dispatch reject <id>` | C, D |

Correction to the first Opus review: attached Work cannot reach accept without
recorded host authority when checks are configured, because `finish` refuses to
run configured checks without `--allow-unsafe-local` (`finish_locked`). The
likelier reasons merged verification "appeared absent" are an unmoved source or
no configured checks (`verify: []`), both indistinguishable in today's accept
output (R3). C's skip reasons stay as written.

### Close: how it is recorded (fixed for P1)

`ReviewState::NotRequested` is already set by native, auto-apply and attach
paths, so closure cannot be inferred from outcome fields. Record it explicitly,
following the `applied_by` pattern: an optional `RunOutcome.closed` with reason,
time, optional replacement ID and optional explanation, stored in the outcome
JSON (`serde(default, skip_serializing_if = "Option::is_none")`), so no table
migration is needed.

- Closing unfinished Work: freeze its Δ first (as `finish` does, without checks,
  or the kept exact/last-seen patch when the workspace is gone); `lifecycle:
  finished`, `work_result: cancelled`, `review: not_requested`, as `close_removed`
  does today.
- Closing a delivered result: Δ unchanged; `work_result` stays `ready`;
  `review: not_requested`; `application` unchanged.
- Either way: one `work.closed` event with the reason (and replacement), no
  `review.*` event, and Work with `closed` set is terminal for `clean`, absent
  from pending review and from ID-less eligibility, and shown as "closed:
  <reason>".
- Existing `work.closed` events (`reason: workspace_removed`) and the runs they
  closed read exactly as before.

CLI reasons: `unfinished`, `workspace-removed`, `already-present`, `superseded`
(stored snake_case). `already-present` requires the existing reverse-apply check
to pass in current source; `superseded` requires `--replacement <id>` naming
different, finished Work whose Δ was retained.

### Execution arrangement as run

- `dispatch.yml` in the clone: `cargo fmt --check`, `cargo clippy --all-targets
  -- -D warnings`, `cargo test` (the full suite, per §5), `coherence.poll_secs: 5`.
- Build caches: each managed workspace keeps the default `target/`. That changes
  §5's "outside source" wording on purpose. Managed workspaces live under the
  supervision state, outside the integration root, so nothing walks them; `/target/` is
  ignored by the tracked `.gitignore`, so it never enters S0 or Δ
  (`validate_candidate_tree` and `collect_diff` list with `--exclude-standard`);
  and `finish` reuses the worker's own build in the same workspace. Merged-tree
  checks at accept build from nothing in their scratch tree.
- The clone's `.claude/settings.local.json` holds only a permission allowlist
  (cargo, read-only git, search, the prompt/handoff directories); no hooks, no
  `worktree` setting. Claude Code applied it in the managed workspace (a linked
  worktree of the clone).
- Workers: `herdr pane run` of the supervisor's
  `attach --root <clone> --agent claude --allow-unsafe-local -- claude
  --permission-mode acceptEdits`, so edits inside the workspace are accepted and
  every other command not on the allowlist asks; the integrator approves each
  after reading it. Prompts: `~/dispatch-dev/prompts-0412/`; handoffs:
  `~/dispatch-dev/handoffs-0412/`.
- Plan edits during a packet are kept outside the clone and committed between
  packets, so the integration source does not move under a running worker.

### P1

| Item | Value |
|---|---|
| Work | `01M4RH6MSGJH5TS4QZJC96V0QA`, wrapped, workspace `state-0412/workspaces/01M4RH6MSGJH5TS4QZJC96V0QA`, branch `dispatch/01m4rh6msgjh5ts4qzjc96v0qa` |
| Agent | Claude Code (interactive), model and effort the user's defaults (Opus 5.5, xhigh); usage unknown unless reported |
| Prompt | `prompts-0412/p1.md` |
| Started | 2026-10-12 11:0x local |
| Handoff | `handoffs-0412/p1.md` |
| Approvals | Each non-allowlisted command was read and approved once. From about 11:30 to 11:35 the worker ran in Claude Code's auto mode: approving a three-option dialog with Enter selected "switch to auto mode". 23 commands ran unapproved in that window, all cargo fmt/clippy/test, read-only git/sed/grep/ls, and the candidate's `close --help`; none wrote outside the workspace or its scratchpad. Mode restored to accept-edits; approvals since are sent as the key `1`, after confirming the dialog is on screen |
| Integrator review | Two changes requested and made: `close` refuses while a watching owner has not yet kept a vanished workspace's last-seen changes (unwatched, it closes and says nothing was kept); `--replacement` refuses closed or rejected Work |
| Worker validation | fmt, clippy pass; `cargo test` 634 passed, 0 failed, 5 ignored (run 2; run 1 before review changes: 633/0/5); R1's test fails on a `git archive` export of the old code |
| Independent validation | Workspace diff applied to a `git archive` of the clone's HEAD, own `CARGO_TARGET_DIR`: fmt, clippy pass; `cargo test` 634 passed, 0 failed, 5 ignored. P0 script against this build: R1's summary names A in every command and the ID-less accept refuses, listing A and B, source untouched; R5's STOP names `dispatch close <id> --reason already-present`; closing R5 records one `work.closed` and no `review.*` event, and `reject` then refuses. R2 and R4 unchanged (P2) |
| Production LOC | `src/` +611 / −144, net +467 (worker's count, excluding `#[cfg(test)]`) |
| Dispatch finish checks | fmt, clippy passed; `cargo test` **failed** once: `runtime_hooks::an_empty_worktree_that_vanishes_unannounced_closes_with_no_changes` timed out waiting for the owner (suite took 34.8 s against about 7 s normally). The test predates P1 and P1 does not change it; it sleeps a fixed 3 s and assumes the owner followed the worktree by then. Reruns on P1's build: the test alone 6/6 passed, the `runtime_hooks` suite 3/3 passed. Recorded as a timing flake; its fix is in P2 |
| Frozen Δ | Applied to a fresh `git archive` of HEAD, its tree (`3e5751d`) equals the reviewed and independently validated tree |
| Decision | Accepted by the human with the flaky failure on record (2026-10-12); `check` CONTINUE, world unchanged; applied 16 files; committed as `b1f97d7` |
