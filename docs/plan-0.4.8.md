# Plan: Dispatch 0.4.8, Work that notices Work

## Context

v0.4.7 completed the lifecycle for a single piece of Work: it appears, is
isolated, is followed, frozen, verified, reviewed, applied or cleaned. Coherence
answers one question: is this Work still valid against the integrated project
(the World)?

0.4.8 adds a second, advisory question: which pieces of Work that are not yet
integrated already touch each other's changes or assumptions? The answer is
concrete evidence, never a score.

Stage 1 first fixes a correctness bug the 0.4.7 runs kept hitting: a
`claude --version` probe that times out refuses a valid profile, and the refusal
sticks.

All facts below were verified on `main` at `241d65c`.

## 1. Assessment of the milestone

I agree, and the timing is right. The pipeline now follows every kind of Work
Dispatch knows about, so what is missing is a view across Work items.

It is also cheap to add. Everything needed already exists: `parse_patch`,
`derive_facts`, symbol extraction, the owner's per-tick Δ following, and the
`watch` view. The layer can therefore be about 300 lines of pure code plus the
wiring.

The real risk is precision, not machinery. Symbol facts cover only Rust and
Python, so everything else is file-level. The promise has to say so.

## 2. Changes to the proposed scope (agreed 2026-09-28)

1. **The version-probe bug is one case of a wider bug.** Any preflight error
   becomes a sticky refusal:
   - profile selection calls `db.record_funding_refusal` on every
     `adapter.preflight` error (`src/orchestrator.rs:959-969`);
   - a launch that ends in `PreflightRefused` does the same
     (`src/orchestrator/native.rs:771`).

   So a 5 s timeout of `claude auth status` ("Claude authentication status
   unavailable") is also sticky. The fix: only *contradicting* evidence is
   sticky. An inconclusive observation refuses this launch only. Nothing
   becomes less strict.
2. **No new probe type is needed.** `probe_version -> Result<Option<String>>`
   already means "observed" or "not observed". The bug is the comparison
   `probe.as_deref() == Some(expected)` (`src/harness/claude.rs:334`), which reads
   `None` as "different".
3. **An unknown version should not block the launch.**
   `validate_executable` runs first and strictly compares the SHA-256 of the
   resolved executable's bytes. Claude's native install is a symlink into
   `versions/<x.y.z>`, and `fs::read` follows it. Identical bytes cannot report
   a different version, so the hash is the stronger evidence of the two.
4. **Add a textual-overlap rule.** The most common real collision is two edits
   of the same region, such as import lines or adjacent functions, and the
   symbol rules alone would miss it.
   - With `--full-index`, each patch carries the pre-image blob id of every file.
   - When two pieces of Work changed the same blob, whether one's changed lines
     fall inside the other's hunk (context included) is plain arithmetic.
   - It is evidence that the edits overlap as text. It is **not** an exact
     prediction that `git apply` will fail, because `git apply` can find
     context at an offset.
   - With different pre-images the rule does not fire, so nothing is assumed
     across different S0s.
5. **Interactions do not depend on the World.** They depend only on each Work's
   (S0, Δ). They are recomputed when the set of participating Work or one
   of their Δs changes, never because the World moved. The one exception: an
   apply removes Work from the set.
6. **A small, disposable projection file is needed.** `watch` is a read-only
   client, and only the owner follows live Δs. The owner therefore writes
   `watchers/<key>.interactions.json` (see §12).
7. **Interactions need a watched project.** `status` shows them from the
   projection while the project is watched, and otherwise says it is not
   watched. No on-demand computation in `status` or `check` for now.
8. **Wording.** Use a neutral "interacts with …" instead of ⚠. A warning glyph
   next to CONTINUE reads as a contradiction and implies a severity we cannot
   know.

### Review revisions (agreed 2026-09-28)

1. Only observed contradicting evidence makes a preflight refusal sticky;
   an inconclusive probe never does.
2. Symbol reads are facet-aware. A `Referenced` fact relies on the signature,
   so a change to a body alone gives no writer→reader interaction. Two changes
   to the same declaration still interact.
3. A transient parse failure of live Work never becomes a file-level
   interaction. The file keeps its last stable footprint, or its analysis is
   pending. The file-level fallback applies once Work is frozen.
4. Rule 4 is "textual overlap", not a prediction that `git apply` fails, and it
   is not shown where a shared declaration already explains the pair. The
   on-disk projection stays minimal; full footprints are recomputable.

## 3. Public promise for 0.4.8

> While Dispatch watches a project, it shows where two pieces of Work that are
> not yet integrated touch each other:
> - both change the same declaration or file, or overlapping lines;
> - one changes the signature of, or removes, a declaration the other uses;
> - one changes or deletes a file the other relies on.
>
> This is exact to the declaration for Rust and Python, and to the file
> elsewhere, and it says which. It is advisory: it changes no verdict and blocks,
> reorders or refreshes nothing. When one piece lands, the other is judged by
> the existing coherence check, as before.

## 4. Claude version-probe fix (stage 1)

**Today:**
- `probe_version` (`src/harness.rs:680`) returns `Ok(None)` on a timeout (5 s),
  a non-zero exit, or empty output.
- `claude::preflight` then reports "Claude CLI version changed".
- Selection records a funding refusal for that `(funding_key,
  authorization_revision)`, and it holds until the person authorizes again.

**Change (in `src/harness/claude.rs` `preflight`):**
- `validate_executable` (hash, settings, expiry) stays first and strict.
- The version check:
  - `Some(v)` equal to the recorded version: continue;
  - `Some(v)` different: refuse, as today (sticky);
  - `None`, or a probe error: the version is unknown, and the launch continues,
    because the hash matched. Record `"cli_version": null` in the preflight
    JSON so the unknown stays unknown.
- The account check is unchanged when the account is observed and different
  (sticky). When `auth status` does not answer (timeout or non-zero exit),
  return `PreflightInconclusive`: nothing launches, and nothing sticks.

**Mechanism:**
- A marker `pub struct PreflightInconclusive(String)` in `src/harness.rs`, next
  to `PreflightRefused`.
- `run_harness` keeps it distinguishable. Both refusal recorders
  (`orchestrator.rs:964`, `native.rs:771`) skip recording when the error
  downcasts to it.
- The message says what happened: "could not confirm Claude's account within
  5 s; nothing was launched; try again".

**Tests:**
- New in `tests/funding_safety.rs` (or `claude_profiles.rs`, next to the
  existing fake CLI): the fake `claude --version` sleeps past the 5 s timeout,
  with the same executable bytes and account.
  - The run launches.
  - No funding refusal is recorded, and a second run launches too.
- Kept: an observed different version still refuses and is sticky.
- New: a fake `auth status` that times out. The launch is refused, no refusal is
  recorded, and the next run with a fast fake succeeds.
- `claude_refusal_is_sticky_until_reauthorized` must keep passing. Its load flake
  was this exact bug.

## 5. The authoritative Δ for each state of Work (verified)

| Work state | Authoritative Δ | Participates |
|---|---|---|
| Native, Working (agent running, or waiting on a question) | The candidate workspace now. The native watcher's `<candidate dir>/delta-live.patch` is written with a plain `fs::write`, so a reader can see it half-written. | yes: the owner takes its own snapshot |
| Attached, Working, workspace present, any owner state (wrapped live, foreign, discovered, idle) | The workspace now. The owner already snapshots unowned attached Work each tick into a scratch file (`serve.rs` `reevaluate`, via `snapshot_delta_indexed`). | yes: the owner takes its own snapshot |
| Attached, Working, `workspace_removed.exact` | `candidates[0].diff_path` (`delta.patch`), written durably by `freeze_removed_workspace` | yes |
| Attached, Working, lost (`exact: false`) | `delta-last-seen.patch`, approximate | no: it can never be finished, so it can never land |
| Finished, Ready, unapplied, review pending (including blocked, or failed checks, which a person may still accept) | `candidates[0].diff_path` | yes |
| Applied | now part of the World | no |
| Rejected, closed, cancelled, failed or interrupted | none | no |

**Rule:** for Work with a live workspace, the owner snapshots that workspace. It
never reads `candidate.diff_path` or any `delta-live.patch`.

This extends the owner's existing following (a scratch file and a stat-cached
index per run) from unowned attached Work to all active Work. Verdicts are still
evaluated and stored only for unowned attached Work, as today.

## 6. `Footprint` (new `src/coherence/interactions.rs`)

```rust
pub enum Target { Symbol { path: String, name: String }, File { path: String } }
pub enum Write { Changes { contract_changed: bool }, Removes, Adds, Deletes }
pub struct Footprint {
    pub writes: Vec<(Target, Write)>,
    pub reads: Vec<Target>,
    pub hunks: Vec<TextHunks>,  // per path: pre-image blob id, changed S0 lines, hunk S0 spans (context included)
    pub unresolved: u32,        // referenced names left unbound (ambiguous or common)
    pub state: Analysis,        // Analyzed | LastSeen { paths } | Pending { paths }
}
```

- An identity is always the root-relative path plus the qualified declaration
  name (for example `Point::new`, `Point.norm`), never display text.
- Display text (`decl.display`) is kept only for explanations.

## 7. Reusing the existing machinery

Footprints come from `derive_facts(work, parse_patch(Δ))` against that Work's
own S0.

**What it already gives:**
- `Modified` facts: symbol writes, with their S0 form relied on.
- `Referenced` facts: symbol reads.
- `FileFallback` facts:
  - for a *delta* path, a file-level write;
  - for any other path (`mentioned_files`), a file-level read.

  The two are told apart by path membership in the Δ, so `MustHold` does not
  change.
- `derived.uncertain`: file-level writes.
- `derived.unbound`: counted into `unresolved`.

**Small additions to `facts.rs`:** `Derived` is widened; no second parser.
- **`introduced`:** `(path, qualified name)` for new declarations. The base names
  are already computed; keep the qualified names too.
- **`contract_changed` and `Removes`:** for each Modified declaration, find the
  post-image declaration of the same qualified name. `extract(post)` is already
  called.
  - The contract has changed exactly when a `Referenced` fact on that
    declaration would break in `evaluate_facts`: the `sig_fp` differs, and for
    Python `python_call_compatible` does not hold.
  - This reuses that test rather than restating it.
  - With no declaration of that name in the post-image, it is `Removes`.
- **Paths `parse_patch` skips** (binary, symlink, mode-only): read them from the
  `diff --git` headers as file-level writes.
- **Added and deleted files:** from `DeltaStatus`, as file-level `Adds` and
  `Deletes`.
- **`TextHunks`:** the `index <old>..<new>` blob id plus hunk spans. Both are
  already parsed into `Hunk`; they are only exposed.

**Cache:** in owner memory, keyed by `(run id, sha256(Δ))`. The cost of
`derive_facts` (one `git cat-file --batch`, plus `git grep` for names) is paid
once per change of Δ.

## 8. Interaction rules

Each rule is evaluated for an unordered pair {A, B}. Every edge has a direction
where there is one, and carries one piece of evidence.

1. **Both change the same declaration:** a symbol `Changes` or `Removes` of the
   same `(path, name)` on both sides, or both `Adds` of it.
   → "both change `auth::validate`"
2. **One changes the contract of what the other uses:** A has `Changes {
   contract_changed: true }` or `Removes` on a symbol target in B's reads.
   → from B's side: "01ABC changes the signature of `auth::validate`, which this
   Work uses" (or "removes")
   - A change to the body alone is not reported. A `Referenced` fact relies on
     the signature only, so the World check would not act on it either.
     Behaviour stays the job of the merged-tree checks.
3. **The same file, at file level:** a file-level write on path P meets any read
   or write on P, of either granularity. This covers an unsupported language,
   an uncertain parse, binary files, and add, add or delete conflicts.
   → "both add `src/new.rs`", "01ABC deletes `util.py`, which this Work changes",
   "both change `config.yml` (file-level: no symbol analysis for this file)"
4. **Textual overlap:** a supported file changed by both, from the *same*
   pre-image blob, where A's changed S0 lines or insertion anchors fall inside
   B's hunk span, or the other way round.
   → "the edits overlap as text at lines 40–52 of `src/auth.rs`"
   - It is not evaluated when the pre-images differ.
   - It is skipped for any path where rule 1 already fired for the pair, so
     weaker evidence never repeats what a shared declaration explains.

**Never reported:**
- read/read;
- the same supported file with different declarations and non-overlapping
  hunks;
- a change to the body alone of a declaration the other uses;
- unrelated files.

**Directionality:** rule 2 edges point from the writer to the reader. Rules 1, 3
and 4 are symmetric. For each pair, report every rule that fires; within a rule,
remove duplicate targets. Rule 1 takes precedence over rule 2 for the same
target.

## 9. Unsupported and uncertain code

- Any file without symbol support, a file whose S0 or post-image cannot be
  analyzed, a file that fails to parse, and binary, symlink and mode-only files
  all contribute *file-level* targets. Their explanations always say
  "file-level".
- This matches the engine: a `File` fact breaks on any change to the file, so
  the World check will re-examine that Work anyway once the other lands.
- **A parse failure while the Work is live is transient**, not a reason to go
  file-level. An agent mid-edit often leaves a file that does not parse, which
  would make warnings come and go.
  - When a live Work's post-image of a supported file does not parse, or its
    identifiers cannot be read, that file keeps its last successfully derived
    footprint, marked `LastSeen`.
  - A file that has never parsed is `Pending`: it claims nothing and is shown as
    "not yet analyzable".
  - The same holds for a half-typed signature: the contract is judged only
    from a parse without errors.
- **Frozen Work falls back to file level.** Once the Work is frozen (finished,
  or removed with the exact Δ kept), a parse failure makes the file file-level,
  as the engine does.
- Unsupported languages and an unreadable S0 do not change while Work runs, so
  file level is right for them live or frozen.
- Unresolved references (`unbound`) are never an interaction. The Work's detail
  shows their count ("3 references could not be resolved") so a missed
  interaction is explainable.

## 10. Which Work participates

Only the states in the "yes" rows of §5, and only Work with exactly one
candidate and the same integration root as the owner (`load_source_runs`).

Each Work's footprint is computed against its own S0.

## 11. Algorithm and complexity

- Recompute when the participant set changes or any participant's Δ digest
  changes. Otherwise reuse the last result.
- Pairwise over n participants: O(n²) pairs. Each pair is set intersection over
  at most about 200 facts plus writes, using `BTreeSet` targets per footprint.
- No inverted index or transitive closure. An index is added only if §15 shows
  the pair phase matters, which is unlikely next to one `git add` per tick.

## 12. Persistence

- Nothing goes in the DB, no events, and no migration. S0, Δ, runs and events
  stay canonical.
- The owner writes `watchers/<key>.interactions.json` atomically when the edges
  or participants change. It holds only
  `{version, computed_at, participants: [{run_id, delta_sha256, analysis, unresolved}], edges: [...]}`.
- Full footprints are not stored. They can always be recomputed from (S0, Δ).
  For diagnosis and the trial, the owner logs each footprint summary at `-vv`.
- The owner removes the file on a clean exit, alongside its watcher record, and
  rewrites it when it starts.
- Readers trust the file only while the serve lock is held, the same rule as the
  watcher record. Otherwise it is ignored.
- It is fully recomputable.

## 13. UX

- **A `watch` or `serve` row:** a trailing segment, after the verdict and state:
  `01ABC123 · discovered claude · … · CONTINUE · working · interacts with 01DEF456`
  (or `· interacts with 2 Work`). Nothing is shown with no interactions.
- **Review screen (`d` or Enter) and `status <run>`:** a separate section after
  Coherence, for example:
  ```
  Coherence   CONTINUE
  Concurrent  01DEF456 changes the signature of auth::validate, which this Work uses (symbol)
              the edits overlap as text at lines 12–18 of src/auth.rs (imports)
  ```
  If the project is not watched: `Concurrent  not known: project not watched ·
  dispatch start`.
- **JSON:**
  - `watch --json` work objects gain `interactions`;
  - `status --json` gains `interactions` only when watched;
  - each entry is `{"with", "direction": "theirs_affects_this"|"this_affects_theirs"|"both",
    "rule": "same_declaration"|"uses"|"file"|"textual_overlap", "path", "symbol"|null,
    "change": "signature"|"removes"|"adds"|"deletes"|null, "evidence": "symbol"|"file"|"text"}`.
- **Accept and reject are unchanged.** An advisory line at accept is left for
  0.4.9, once precision is known.

## 14. Interaction matrix

These are pure unit tests in `interactions.rs`, using the fixture repository
style of `facts.rs` tests (a real baseline plus `collect_diff`).

| # | Case | Expected |
|---|---|---|
| 1 | A and B modify the same function | rule 1 |
| 2 | A modifies `validate`, B adds a call to it | rule 2, A→B |
| 3 | The reverse | rule 2, B→A |
| 4 | Both only call `validate` | none |
| 5 | The same `.rs` or `.py` file, different functions, non-overlapping hunks | none |
| 6 | Unrelated files | none |
| 7 | The same `.yml` or `.js` file | rule 3, file-level |
| 8 | A deletes `util.py`; B changes it, or calls into it | rule 3 or rule 2 |
| 9 | Both add `src/new.rs` | rule 3 |
| 9b | Both add `use` lines at the top of the same file (same pre-image) | rule 4 |
| 9c | Adjacent edits of different functions within the hunk context | rule 4, not rule 1 |
| 9d | A changes the body of `validate`, B calls it | none |
| 9e | A changes the signature | rule 2, "changes the signature" |
| 9f | A adds an optional Python parameter that `python_call_compatible` accepts | none |
| 9g | A and B both change `validate` and both add imports at the top of the same file | rule 1 only; rule 4 is skipped for that path |
| 14 | A and B have different S0s (B began after a commit that edited `auth.rs`) | rules 1 and 2 by name; rule 4 not evaluated |

**Owner and `watch` integration tests** (new `tests/interactions.rs`, driven
through `watch --json` and the projection):

| # | Case | Expected |
|---|---|---|
| 10 | Active attached Work edits into an overlap | the edge appears within a tick |
| 11 | It reverts the change | the edge disappears |
| 12 | Accept A | A leaves the set, B's edges to A disappear, and B's verdict comes from the existing World check (REFRESH when a signature changed) |
| 13 | Rejected, closed, lost Work | never participates |
| — | The owner stops | the projection is removed, and `status` says "not watched" |
| — | Live Work leaves a file unparseable mid-edit | that file keeps its last footprint (`LastSeen`); no file-level edge appears |
| — | The same Work is frozen with that file still unparseable | the file becomes file-level |

False warnings are a first-class failure. Cases 4, 5, 6, 9d and 9f are assertions
that *nothing* is reported where a file lock would have warned.

## 15. Performance measurement

An ignored test, `interactions_cost` (the `serve_tick_cost` pattern), on the
2,000-file fixture with 1, 5, 20 and 50 Work items, each with a small realistic
Δ. It prints:
- footprint derivation per Work, cold and cached;
- the pair phase in total;
- the added cost of following native and live-wrapped Work.

The numbers go in the plan log. An inverted index or a change of approach is
considered only if the pair phase at n=20 is visible (> 50 ms).

## 16. Real-agent trial

This uses Claude Code and Cursor only (no Codex), `caffeinate -i`, a state directory kept
apart from the real one, and the trial repository with a small Python `auth` module plus a
separate subsystem.

**Four concurrent pieces of Work:**
- **A** (`claude -p --worktree`, discovered): change `auth.validate`'s signature.
- **B** (`attach -- cursor-agent`): add a caller of `auth.validate`.
- **C** (`claude -p --worktree`): change `auth.validate`'s body.
- C against B is the body-only case: no interaction is expected.
- **D** (`attach -- cursor-agent`): change an unrelated module.
- **E** (optional): a function in `auth.py` other than `validate`. This is the
  same-file, different-declaration case.

**Before anything lands, show:**
- A → B (uses, signature);
- A ↔ C (same declaration);
- D with none;
- C with no interaction against B (a change to the body only);
- E with none against A and C, unless their hunks overlap as text, and then
  only as textual overlap.

**Then:**
1. Finish and accept A.
2. A's edges disappear.
3. B's World verdict becomes REFRESH (signature), and C's patch conflicts or its
   facts break.
4. D stays CONTINUE.

**Record** the footprints (from the projection), the edges and explanations, the
verdicts after A lands, and the timing from edit to edge.

## 17. Stages (one commit each on `release-0.4.8`)

| # | Stage |
|---|---|
| 0 | This plan as `docs/plan-0.4.8.md` |
| 1 | Version probe and inconclusive preflight fix, with tests |
| 2 | `Footprint`: widen `Derived` (introduced qualified names, `contract_changed`, skipped header paths, hunk spans, last-seen footprints for live parse failures); `interactions::footprint`; unit tests |
| 3 | Rules 1–4 and pairwise `interactions::edges`; matrix cases 1–9e and 14 |
| 4 | Owner: participants, following of all active Work, a cache by Δ digest, the projection file (write, remove, trust rule); tests 10–13 |
| 5 | UX: the `watch`/`serve` row segment, a Concurrent section in review and `status`, JSON fields; PTY and plain tests |
| 6 | Measurement: `interactions_cost`, with numbers logged |
| 7 | Real-agent trial and fixes |
| 8 | Docs (README, `coherence.md`, `attach.md`, product guide, upgrade notes), release notes, version 0.4.8 |

## 18. Risks and likely false-positive sources

- **Wrong binding:** the unique-name rule binds a local variable or an unrelated
  method to a declaration of the same base name, giving a spurious "uses". This
  is inherited from the facts layer and bounded by `bindable` and the denylist.
- **Container granularity:** a change to a class or impl header counts as a
  change to the container, not to its members.
- **Coarse file level** for non-Rust and non-Python projects. It is honest, but
  it may be noisy, and it is labelled every time.
- **Rule 4 noise:** a context overlap on trivially shared lines, such as a blank
  line between functions. It is labelled as textual overlap, not a predicted
  conflict. This is measured in the trial.
- **Missed behavioural dependence:** a change to a body alone is deliberately
  not reported. The merged-tree checks at accept are what cover behaviour.
- **Last-seen footprints** can lag an agent's edits until the file parses
  again. They are labelled as last seen.
- **Missed interactions:** unbound names, dynamic Python, and different S0s
  (rule 4 is skipped). `unresolved` makes the first visible.
- **Cost:** following native and live-wrapped Work doubles `git add` for native
  runs, whose own watcher also snapshots. If §15 shows this matters, make
  `delta-live.patch` writes atomic and read those instead.
- **Stale projection:** it is shown only while the lock is held, and removed on
  exit.

## 19. Definition of done

- A valid Claude profile survives a slow `--version` and a slow `auth status`,
  with no sticky refusal. An observed version change or account change still
  refuses and sticks.
- Matrix cases 1–14 pass, including every "none" case.
- `watch`, review, `status` and JSON show interactions, separate from the
  verdict. Nothing about CONTINUE, REFRESH or STOP, accept, or auto-apply
  changes; the existing coherence, attach, serve and runtime suites stay green.
- The real-agent trial shows A→B, A↔C, D none and E none. After A lands, B and C
  are judged by the World check, and D stays CONTINUE.
- Costs are measured and logged. fmt, clippy and the full suite pass.

## 20. Waiting for 0.4.9 and later

- Using interactions in policy: accept hints, integration-order suggestions,
  gating auto-apply, stopping agents.
- Recording interactions durably, and history or reports.
- Interactions without an owner (on demand in `check`/`status`).
- Transitive closure, more languages, indexed invalidation.
- Shared-checkout attribution, the Codex lifecycle, process scanning,
  sockets/RPC, a global daemon.

## 21. Recommended milestone after 0.4.8

**0.4.9: "Interactions, measured."**
- When Work is applied, record one event on it (`interaction.at_apply`) naming
  the interactions it had at that moment.
- When the other side's verdict next changes, record whether it went REFRESH
  because of that landing.
- This gives the real precision and recall of interactions against the
  engine's own later verdicts (`docs/coherence-validation.md`). It is the
  evidence needed before any accept-time hint or ordering policy.

## Verification

- **Each stage:** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  and `cargo test` (`--test-threads=4`; rerun known load flakes on their own).
- **Stage 1:** the new timing tests, plus `funding_safety` and `claude_profiles`
  rerun 5× under `--test-threads=8` load.
- **Stages 3–5:** `cargo test interactions`, `cargo test --test interactions`,
  and the `watch` PTY journey.
- **Stage 6:** `cargo test --test serve interactions_cost -- --ignored --nocapture`.
- **Stage 7:** by hand in the trial repository, with state kept apart from the
  real `~/.dispatch` and hooks only in the trial's `.claude/settings.local.json`. Removal events are delivered straight to
  `dispatch hook claude`.

## Progress log

- 2026-09-28: Stage 0. Plan agreed with the four review revisions above: sticky
  refusals only on contradicting evidence; facet-aware symbol reads (a change to
  a body alone does not interact); transient parse failures of live Work keep
  the last footprint; rule 4 is textual overlap, and the projection stays
  minimal.
- Stage 1. Only contradicting evidence makes a preflight refusal sticky.
  - **Version.** `claude::preflight` compares the version only when the probe
    observed one. A probe that times out, fails or prints nothing leaves it
    unknown. The launch goes ahead, because `validate_executable` already
    matched the executable's SHA-256. The preflight evidence records
    `cli_version` (null when unknown).
  - **Account.** When `claude auth status` does not succeed (a timeout or a
    non-zero exit), the preflight returns the new `PreflightInconclusive`.
    `run_harness` passes it through instead of wrapping it in
    `PreflightRefused`, and profile selection does not record it as a funding
    refusal. The launch is refused with "Claude did not confirm its account in
    time; nothing was launched; try again". An observed different account is
    still refused and sticky, as are an executable change and expiry.
  - **Tests** (`funding_safety`, driven by fixture switches `version-sleep`,
    `version-text` and `auth-sleep`):
    - `claude_version_probe_timeout_is_not_a_change`: `--version` sleeps 7 s;
      the run launches and no refusal is recorded;
    - `claude_observed_version_change_is_refused_and_sticky`: another version
      is refused, recorded, and still refused once it is back;
    - `claude_unanswered_account_probe_refuses_that_launch_only`: `auth status`
      sleeps 7 s; that launch is refused and nothing is recorded, and the next
      launch runs.

    Before the fix, the first and third fail with the reported bug ("Claude CLI
    version changed", "authentication status unavailable").
- Test hardening found while running the suite on a loaded machine (Defender and
  a macOS update at over 150% CPU between them, under memory pressure):
  - `phase4_follow_registration_racing_completion_cannot_lose_it` hung for a
    day. The test waited for the worker before reading its piped stdout, and the
    worker was blocked printing its `--json` result into the full pipe. The
    follower, read only after the worker, was blocked the same way. Both pipes
    are now drained while waiting.
  - `malformed_hook_input_is_refused_whole_…` wrote a 70 KiB event. The hook
    stops reading at its 64 KiB bound, so the write can find the pipe closed.
    The helper accepts a closed pipe, and asserts on the reply as before.
    `hook_raw` now reuses `hook_output`.
  - Eight `serve --background` owners left from a `runtime_hooks` run killed on
    2026-09-27 (its `Drop` cleanup never ran) were stopped.
- Stage 2. `Footprint` (`src/coherence/interactions.rs`), derived from the
  facts layer. Nothing about the facts or verdicts changes.
  - **`facts.rs`** gains outputs only:
    - `DeltaFile.old_blob` (from `index <old>..`) and `DeltaFile::spans()` (hunk
      S0 spans, context included);
    - `changed_paths(patch)`: every path the patch changes, with its status,
      including the binary, symlink and mode-only files `parse_patch` skips;
    - on `Derived`: `introduced` (qualified names new in the post-image);
      `contracts` (for each Modified declaration with a clean post-image:
      `Kept`, `Changed` or `Removed`, by the same test a `Referenced` fact
      uses: `sig_fp`, or `python_call_compatible`); and `unparsed` (supported
      files whose post-image does not parse, while the baseline did).
  - **`interactions::footprint(work)`:**
    - symbol writes: Modified declarations, as `Changes { contract }` or
      `Removes`, and introduced declarations, as `Adds`;
    - symbol reads: `Referenced` facts;
    - file reads: files the code names;
    - file writes: added and deleted files, and modified files without
      declaration-level analysis (an unsupported language, skipped by the
      parser, or an unreadable baseline);
    - text edits: blob, removed lines, insertions and spans, for supported
      modified files.
  - **`interactions::settle(fresh, frozen, last)`:** a clean footprint stands.
    With an unparsed file, frozen Work makes that file file-level, and live
    Work keeps its last clean footprint (`LastSeen`) or claims nothing
    (`Pending`).
  - Tests: 5 footprint cases (body versus signature versus removal, a
    compatible Python signature, reads and adds, whole-file cases including
    binary, the unparsed live and frozen cases) and `changed_paths` (a quoted
    path with a space, binary, deleted, added). The coherence unit and
    integration suites are unchanged and green.
- Stage 3. The rules and the pairwise comparison (`interactions::between`,
  `interactions::edges`), with the matrix as unit tests.
  - **Rule 3 (file) runs first.** A path either side judges as a whole is
    explained there, so no symbol or text evidence is repeated for it. When
    both sides write the path, the entry is symmetric ("both add" or "both
    delete" when that is what they do); otherwise it names the writer and what
    it does.
  - **Rule 1 (same declaration):** any symbol written by both.
  - **Rule 2 (uses):** a `Changes { contract: true }` or `Removes` on a
    declaration the other reads and does not itself write.
  - **Rule 4 (textual overlap):** the same path from the same S0 blob, with no
    rule 1 on that path. It fires on a removed line inside the other's hunk
    span, an insertion strictly inside it, or an insertion at the same place
    the other inserts.
  - **Found while writing the matrix:** two insertions at the same S0 point
    (both at the top of a file, or both at its end) fall strictly inside
    neither hunk. The shared insertion point was added as evidence.
  - **Matrix (unit tests):** m1, m2/m3 (both directions), m2b (removal), m4,
    m5, m6, m7, m8 (a deleted file against a change and a use), m9, m9b
    (imports at the top), m9c (adjacent declarations, text only), m9d (body
    only: none), m9f (compatible Python: none), m9g (no line evidence beside a
    shared declaration), m14 (different S0s: by name, never by line), and
    `edges` (only pairs that interact).
  - **Checked by breaking the rules on purpose:** without the shared insertion
    point, m9b fails; treating every write as breaking a contract fails m9d
    and m9f.
