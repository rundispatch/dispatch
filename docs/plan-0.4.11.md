# Plan: Dispatch 0.4.11, a clearer `dispatch watch`

## Context

The herdr fleet demo (three Claude Code agents in their own worktrees, watched
in the background) showed that the behavior is right but the view is hard to
read:

- **One sentence per row.** Each Work item is one sentence of 8–10 `·`-separated
  fields. It wraps to 2–4 lines at 80–120 columns, and the continuation lines
  start left of the row text, so they look like new rows.
- **No color.** The only color is slate on the hint row. `styled_body` colors
  whole lines by substring, so a row whose text contains "failed" turns red.
- **Nothing a reader can recognize.** Work is named only by ULID prefixes that
  differ in one character (`01M4DRX9`, `01M4DRXH`, `01M4DRXS`). The worktree
  folder that herdr shows (`auth-ctx`) is on the record but never shown.
- **S0 misleads.** `S0 workspace at start 5e3cb514` is a Dispatch snapshot
  commit. Three worktrees at the same `HEAD` show three different hashes.
- **Vocabulary is inconsistent.** `unmoved` and `CONTINUE` mean the same
  verdict. "discovered" reads like process scanning.
- **Selection is a row index.** When a row above the selection leaves the list,
  the selection silently moves to other Work, and `a` would accept it.
- **The hint row lies.** It advertises `Shift+Tab`, which `watch` ignores.
- **Notices are poor.** They never clear and name no Work.

This release redesigns **only the interactive `dispatch watch`** (a terminal on
stdin and stdout, not `--json`, not `--plain`).

## 1. Decisions (agreed 2026-10-08)

1. **Layout A:** a table with a detail pane for the selected Work at 90 or more
   columns, and two-line cards below 90.
2. **Landed Work** shows `–` in the verdict column. The verdict it had when it
   landed is in the details.
3. **Work names**, in order of preference:
   - the workspace folder name;
   - else the task text;
   - else the shortest unambiguous Work ID.

   Duplicates are disambiguated.
4. **S0 leaves the rows.** The details state its provenance, and a Dispatch
   snapshot commit is called that, never a plain commit.
5. **Work that needs a decision is listed first,** if selection stays stable
   (§3.4).
6. **Semantic color from existing `Theme` tokens only.** Color reinforces text
   and never replaces it.

### Preserved, byte for byte

- Plain `serve`, piped `watch` and `watch --json` (`serve::watch` and `View`).
- `status` text and JSON, `WorkLine`'s `Display` text, and every JSON contract.
- Every CLI command's output and error text, including `orchestrator::decide`'s
  refusal text. `watch` composes its own feedback (§3.9).
- `coherence::interactions::explain()` (used by `status`). `watch` gets its own
  named wording (§3.7).
- Coherence and interaction semantics, the Work model, persistence, and every
  event.

**Not built:**
- no new TUI framework, widget library or dependency;
- no new actions or confirmations (accept stays unconfirmed, as in 0.4.10);
- no mouse support, filtering, search or scrolling of the details;
- no change to auto-apply.

## 2. Files

| File | Change |
|---|---|
| `src/presenter/watch.rs` | Rewritten: the view model, ordering and selection, rendering, keys and feedback. Unit tests live here |
| `src/orchestrator/serve.rs` | `project_rows` (only caller: `watch`) is replaced by a function that returns what the view model needs: the `RunRecord`s in view and the projection. `view_rows`' filter (active, awaiting review, or finished within the hour) is unchanged. Nothing else in this file changes |
| `src/presenter.rs` | The minimum `Ui` support: draw a caller-built frame with the same coalescing (content key plus terminal size), and a per-`Ui` switch that drops the auto-apply mode segment and ignores Shift+Tab. The goal session keeps both |
| `src/orchestrator/background.rs` | At most one small `pub(crate)` accessor if the header needs the watcher record without the `(pid …)` text. `describe` and `project_line` are unchanged |
| `tests/fixtures/watch_session.py`, `tests/watch_ui.rs` | Journeys updated to the new view. New PTY journeys (§4) |
| `docs/` | README and product-guide wording for `watch`; release notes come in stage R |

## 3. The contract

### 3.1 The view model (pure, unit-testable)

One `Item` per Work in view, built from its `RunRecord`, its `WorkLine` (the
existing `orchestrator::work_line`, unchanged) and the interaction projection.
Building it reads nothing else and writes nothing. It holds:
- the ID, name, agent, state, verdict column text and kind, and checks text;
- `touches`: the names of other Work;
- the detail lines (§3.7), the next action (§3.8), the available keys, and the
  group (§3.4).

### 3.2 Names

1. **Base name:**
   - The attached workspace's folder name (`attachment.workspace` file name),
     unless Dispatch made the workspace (`workspace_owner == Dispatch`, the
     folder is named by run ID).
   - Else the run's `task`, unless it is the generic `attached work in <dir>`.
   - Else the Work's short ID (`state::short_ids` over the items in view).
2. **Duplicates** (after truncation to the column or card width): every holder
   of the duplicate gets ` <short id>`, with the short ID long enough to tell
   that group apart, at least 4 characters. The name part is truncated to make
   room, and the ID part never is.
3. **Truncation** uses a single `…` (`...` in ASCII mode) at the end. Width is
   measured in terminal cells (unicode width), never bytes.

### 3.3 Columns and words

| Column | Width (wide) | Content |
|---|---|---|
| marker | 2 | `› ` for the selection (`> ` in ASCII), else 2 spaces |
| WORK | flexible, 12–28 | name (§3.2) |
| AGENT | 8 | `WorkLine.agent`, truncated |
| STATE | 9 | `WorkLine.state` unchanged: `working`, `question`, `idle`, `removed`, `lost`, `ready`, `blocked`, `applied`, `finished`, `rejected` |
| VERDICT | 11 | `CONTINUE` (also for `unmoved`), `REFRESH`, `STOP`, `not checked`, or `–` for applied, rejected or finished Work (`-` in ASCII). `overridden` exists only on applied Work, so it shows `–` |
| CHECKS | 12 | `passed`, `failed`, `not run`, `none`, `inconclusive` |
| TOUCHES | the rest, at least 10 | other Work's names, comma-separated, truncated with `… +N`. `?` while the analysis is pending, `–` when none |

- A header row with the column titles uses `secondary`.
- No column wraps. A row is exactly one terminal line.

### 3.4 Order and selection

**Groups**, in this order; inside a group, by `created_at`, then ID:
1. **Needs you:** `question`, `ready`, `blocked`, `removed`, `lost`.
2. **In progress:** `working`, `idle`.
3. **Done:** `applied`, `rejected`, `finished`.

There are no group headings.

**Selection is a Work ID** (`Option<String>`), never an index.
- **On every refresh:**
  - If the selected ID is still listed, it stays selected, wherever it moved.
  - If it left the list, the selection goes to the item now at the old
    selection's position (clamped to the last). The view sets `moved`, and the
    notice says `<old name> left the list; now on <new name>.`
  - If the list is empty, nothing is selected.
- **The first draw** selects the first item.
- **Navigation:** `↑`/`k` and `↓`/`j` move by one item in display order and
  clear `moved`.
- **Every action key** (`a`, `r`, `f`, `d`, Enter) acts on the selected ID as
  last drawn.
  - If `moved` is set, the key does nothing but clear `moved` and notice
    `Selection moved to <name>. Press <key> again to act on it.`
  - Before acting, the run is reloaded by ID. If it is no longer in view, the
    notice is `That Work is no longer listed; nothing changed.` and nothing
    runs.
- The list scrolls to keep the selection visible. It never scrolls the
  selection off screen.

### 3.5 Wide layout (90 columns or more)

```
 tinyauth · watched in the background since 06:48            1 needs you · 2 done
 ────────────────────────────────────────────────────────────────────────────────────
   WORK          AGENT    STATE     VERDICT     CHECKS      TOUCHES
 › me-endpoint   claude   blocked   REFRESH     passed      –
   auth-ctx      claude   applied   –           passed      –
   slugify       claude   applied   –           passed      –
 ────────────────────────────────────────────────────────────────────────────────────
 me-endpoint · claude session in its own worktree · .claude/worktrees/me-endpoint · Work 01M4DRXH
 Verdict  REFRESH · def validate(token): => def validate(ctx, token):
 Checks   passed
 Began    a Dispatch snapshot (e4a15cd9) of its worktree when its first session started,
          not a commit on any branch
 Next     It can't be accepted as is. r reject it, then run the agent again from the
          current source.
 ────────────────────────────────────────────────────────────────────────────────────
 me-endpoint: not applied. Stale (REFRESH): def validate(token): => def validate(ctx, …
 r reject · ↑↓ select · q leave
```

- **Header, line 1:** the root folder's name (bold), ` · `, then the watcher
  status:
  - `watched in the background since HH:MM`;
  - `watched by dispatch serve since HH:MM`;
  - `not watched · start watching: dispatch start` (`warning`);
  - `watching unknown: …`.

  Then the version mismatch, if any, and the consent description as
  `project_line` gives them, but **without the pid**. Right-aligned when it
  fits: the counts `N need you`, `N in progress`, `N done`, with zero counts
  omitted.
- **Rules** are full-width lines (`─`, `-` in ASCII) in `secondary`.
- **The details** take at most 8 lines (§3.7 lists what is dropped first).
- **The notice line** (§3.9) appears above the hint only while a notice is
  shown, and wraps to at most 2 lines.
- **The hint** (§3.8) is the last line.

### 3.6 Narrow layout (fewer than 90 columns)

```
 tinyauth · watched                 1 needs you
 ──────────────────────────────────────────────
 › me-endpoint                         REFRESH
     blocked · checks passed
   auth-ctx                                  –
     applied · checks passed
 ──────────────────────────────────────────────
 Verdict  def validate(token): => def
          validate(ctx, token):
 Next     r reject it, then run the agent
          again from the current source.
 r reject · ↑↓ · q leave
```

- **Header:** the name and a short status (`watched`, `not watched`, or
  `watched by serve`), then the counts if they fit.
- **Each card is two lines:**
  - Line 1: the marker, the name (truncated to leave the verdict room), and the
    verdict word right-aligned. It is never truncated.
  - Line 2: 4 spaces, `state · checks <checks>`, then
    ` · touches <names>` if it fits, else ` · touches N`.
  - State and checks come first and are never truncated at 40 columns or more.
- **Details:** `Verdict` and `Next` always, then `Checks`, `Touches` and
  `Began` as height allows. Each value wraps with a hanging indent under its
  label, so the reason is visible, not truncated.
- **Below 40 columns:** only `Widen the terminal to at least 40 columns.` and
  `q leave`. Keys still work.

### 3.7 Details (both layouts)

**Line 1:** the name (bold), the agent, then how the Work came to be listed:
| `WorkLine.origin` | Wording |
|---|---|
| `native` | `launched by Dispatch` |
| `isolated` | `attached; Dispatch made its workspace` |
| `discovered` | `<agent> session in its own worktree` |
| `attached` | `attached workspace` |

The workspace path, relative to the root when inside it, else with `$HOME` as
`~`, follows when it fits. The line ends with `Work <short id>`.

Labels are `secondary`, padded to 9 cells, and values wrap with a hanging indent.

- **Verdict:**
  - `CONTINUE · the source has not moved since it began` (`unmoved`)
  - `CONTINUE · the source moved; nothing this Work relies on changed`
  - `REFRESH · <reason>` (each stored reason's full `detail`, up to 3, then
    `+N more: dispatch check <short id>`)
  - `STOP · its changes are already in the source`
  - `not checked yet`
  - For a `working` or `idle` item with REFRESH or STOP, add the line
    `Advisory while the agent works: nothing is stopped.`
- **Landed** (applied Work, instead of Verdict): `applied by you` or
  `applied by auto-apply`, then:
  - ` · it was CONTINUE (the source had not moved)`;
  - ` · it was CONTINUE`;
  - ` · applied over REFRESH by your override: <first overridden reason>`.

  Rejected Work: `Rejected · nothing was applied`. Finished without a result:
  `Ended without a result`.
- **Checks:**
  - `passed`
  - `failed · d review the output`, when it can be reviewed
  - `failed`
  - `not run yet`
  - `none configured`
  - `inconclusive`
- **Touches:** one line per interaction. Both pieces of Work are named, with no
  pronoun standing for Work, built from `Interaction` (rule, writer, change,
  target, lines) with the verbs `explain()` uses:
  - `<writer> changes the signature of validate (tinyauth/auth.py), which <other> uses`
  - `<this> and <other> both change <what>`
  - `<writer> deletes <file>, which <other> relies on (whole file)`
  - `The edits of <this> and <other> overlap as text at lines 4–9 of <what>`

  The last line is always `Advisory: nothing is held back.` Pending analysis:
  `not known yet: its files don't parse while it's being edited`. Last-seen
  analysis and unresolved names are added as in `details()`. With no
  interaction, the line is omitted.
- **Began:** from `WorkLine`'s inputs. The hash, when shown, is 8 characters:
  - Git merge base: `the Git merge base <hash> of its worktree and the project`
  - Snapshot at attach: `a Dispatch snapshot of its folder taken at attach; earlier edits are not in its changes`
  - Workspace at start: `a Dispatch snapshot (<hash>) of its worktree when its first session started, not a commit on any branch`
  - Native with a Git head: `a Dispatch snapshot of the project's working tree at <head>`
  - Native, plain folder: `a Dispatch snapshot of the project folder`
- **Next:** §3.8.

**Under limited height,** lines are dropped in this order: Began first, then
Touches, Checks, then Landed or Verdict's extra lines. Line 1, the verdict line
and Next are never dropped.

### 3.8 Next action and keys

| Item | Next (exact text) | Keys offered |
|---|---|---|
| `question` | `It is waiting for an answer: dispatch status <short id> shows the question.` | – |
| `working`, attached | `When the agent is done, f finish: freezes its changes and runs <checks joined with ", ">.` (`… freezes its changes.` when no checks) | `f finish` |
| `working`, native | `Dispatch is running it. It is listed as ready when it is done.` | – |
| `idle` | `No session is open. f finish to freeze its changes, or resume the session in its worktree.` | `f finish` |
| `removed` | `Its worktree was removed and its exact changes were kept. f finish to check them.` | `f finish` |
| `lost` | `Its worktree is gone; only its last-seen changes were kept. f finish to freeze them, then review.` | `f finish` |
| `ready`, CONTINUE, unmoved | `a accept: applies it to <root name>.` | `a accept`, `d review`, `r reject` |
| `ready`, CONTINUE, moved | `a accept: runs your checks on the merged result, then applies it to <root name>.` (without "runs your checks …" when there are no checks or `integration_checks: false`) | `a accept`, `d review`, `r reject` |
| `ready`, not checked | `a accept: Dispatch checks it against the source first.` | `a accept`, `d review`, `r reject` |
| `ready` or `blocked`, REFRESH | `It can't be accepted as is. r reject it, then run the agent again from the current source.` Native runs add ` Or: dispatch refresh <short id>.` | `r reject`, `d review` |
| `ready` or `blocked`, STOP | `Its changes are already in the source. r reject it.` | `r reject` |
| `ready`, checks failed (any verdict but REFRESH or STOP) | `Its checks failed. d review the output, then a accept or r reject.` | `d review`, `a accept`, `r reject` |
| `applied` | `Nothing to do: it is in <root name>.` | – |
| `rejected`, `finished` | `Nothing to do.` | – |

- **The hint** is the offered keys, then `↑↓ select · q leave` (`↑↓ · q leave`
  below 90 columns). There is no auto-apply segment and no `Shift+Tab` in
  `watch`.
- **Every key keeps working as in 0.4.10,** whether offered or not: `a`, `r`,
  `f`, `d`, Enter, `j`/`k`, `↑`/`↓`, `q`, Esc and Ctrl+C. A key that doesn't
  apply gives a notice, as in §3.9.
- **Shift+Tab** does nothing in `watch`.
- **Confirmations keep their choices and Cancel as the default.** Only the
  question text changes, and it names the Work:
  - Reject: `Reject <name> (Work <short id>)? Nothing is applied, and a workspace Dispatch made is kept.`
  - Finish: `Finish <name> (Work <short id>)? Finishing runs this project's checks on your machine, with your permissions:` followed by the checks.

### 3.9 Feedback

One notice at a time. It records the Work's ID and name, a kind (`done`,
`refused` or `info`), its text, and when it was made. It is shown as
`<name>: <text>` on the notice line. It ages out after 10 seconds, and is
replaced by the next notice. `refused` uses `error`; the others use
`foreground`.

| Event | Text |
|---|---|
| accept applied | `accepted and applied.` |
| accept refused, REFRESH | `not applied. Stale (REFRESH): <first reason>. The source is unchanged. Next: r reject it.` |
| accept refused, STOP | `not applied. STOP: its changes are already in the source. Next: r reject it.` |
| accept refused, other | `not applied: <first sentence of the error>.` |
| reject done | `rejected; nothing was applied.` |
| finish done | `finished; checks passed. It now waits for your review.` / `finished; checks failed.` / `finished. It now waits for your review.` |
| finish refused | `not finished: <first sentence of the error>.` |
| prompt cancelled | `nothing changed.` |
| `d` on Work that can't be reviewed | `only a result waiting for review can be reviewed.` |
| `f` on Work that can't be finished | `only attached Work still in progress can be finished.` |
| selection moved (§3.4) | as in §3.4 |

- **The refusal kind** comes from reloading the run after the refusal: its
  stored validity's decision and first reason. It never comes from parsing the
  error string. The "other" case takes the error text up to the first `. `.
- **Long reasons** are truncated to fit the notice's 2 lines. The full reason is
  in the details.

### 3.10 Color, NO_COLOR and ASCII

Only these existing `Theme` styles:

| What | Style |
|---|---|
| selection marker | `focus` |
| selected name | bold |
| CONTINUE, only on `ready` Work with checks not failed | `success` |
| REFRESH and STOP words | `warning` |
| `failed` checks | `error` |
| `refused` notice | `error` |
| every cell of an applied, rejected or finished row (its card in narrow) | `inactive` |
| labels, column titles, rules, hint | `secondary` |
| `not watched` | `warning` |

Everything else uses `foreground`. No backgrounds and no reverse video.
- **NO_COLOR, `--no-color`, `DISPATCH_COLOR=none` and `TERM=dumb`** give no
  color codes at all. Bold stays.
- **256 and 16 colors** come from the existing `Theme` translations.
- **`--ascii`:** every glyph has its ASCII form (`›` `>`, `–` `-`, `─` `-`,
  `…` `...`, `·` `-`, `↑↓` `up/down`). Nothing outside ASCII is drawn.

### 3.11 Empty state

`No Work in the last hour.` then:
`A Claude Code session in its own worktree of this project, or dispatch attach -- <agent>, appears here.`
With `not watched`, the second line is instead
`Nothing is watching this project: dispatch start.`

## 4. Tests

**Unit tests in `src/presenter/watch.rs`** render with ratatui's `TestBackend`
at 60×20, 80×24, 120×30 and 200×30, and assert on the buffer.
- **Layout:**
  - every item's name, state and verdict word is fully visible;
  - no table or card row wraps;
  - wide is used at 90 columns or more, narrow below;
  - at 60 columns, the selected Work's reason and Next are visible;
  - ASCII mode draws only ASCII.
- **States:** CONTINUE ready (unmoved and moved), not checked, REFRESH blocked
  (native and attached), STOP, failed checks, applied (`–`, inactive, Landed
  line), rejected, working with an advisory REFRESH, question, idle, removed,
  lost. Each with its Next text and keys from §3.8.
- **Names:** long names; duplicate names, including duplicates created by
  truncation; a Dispatch-made workspace falling back to the task; the generic
  task falling back to the short ID.
- **Selection:** selection survives:
  - insertion before and after it;
  - removal of other Work;
  - a group change (`ready` to `applied`);
  - resizing between 200 and 60 columns.

  When the selection itself is removed: the neighbor is selected, `moved` is set,
  the next action key does nothing, and a second press acts. An action whose ID
  has left the list does nothing.
- **Interaction wording:** both directions name both pieces of Work, and there
  is no `it`/`this Work` for Work.
- **Color:**
  - with a truecolor theme, REFRESH has the `warning` foreground, applied rows
    are `inactive`, and an actionable CONTINUE is `success`;
  - with NO_COLOR, no cell has a foreground color.

**PTY journeys** (`tests/fixtures/watch_session.py`, run by `tests/watch_ui.rs`):
- The existing journeys (reject asks, accept applies, finish asks, clean,
  interactions) still pass, with their expectations updated to names and new
  wording, not weakened.
- The same Work at 60, 80, 120 and 200 columns shows its name and verdict.
- Resize while a Work is selected, then `r`, and the confirmation names the
  same Work.
- Accepting a REFRESH Work shows the refusal naming it and `r reject`, and the
  run stays unapplied.
- `NO_COLOR=1` gives output with no color codes (`has_color`).
- An attached worktree Work is named by its folder.

**Unchanged output:** the existing `serve`, `status` and `watch --json` tests
pass unmodified. Stage R also diffs piped `watch` and `watch --json` between the
0.4.10 release and the candidate on the same state.

## 5. Packets and supervision

| Packet | Owner | Scope |
|---|---|---|
| P0 | integrator | This plan. Supervisor: Dispatch 0.4.10 (pinned), state `~/dispatch-dev/state-0411`, integration clone `~/dispatch-dev/integration-0411` on `release-0.4.11` |
| A | one Claude Code worker (`claude --worktree 0411-a`) | §2 files: implementation, unit tests, PTY journey updates |
| B | a second worker, after A is integrated (`0411-b`) | Independent review of A against this plan, and new acceptance tests. Production defects are reported, not fixed |
| A2 | the integrator, or a worker | Fixes for B's confirmed defects, if any |
| R | integrator | Version 0.4.11, release notes, unchanged-output diff, PTY runs at four widths, and the herdr demo re-recorded with the candidate build |

- Each packet is integrated only after human approval: `finish`, review the
  frozen patch, `accept`, commit.
- Workers build only with their own `CARGO_TARGET_DIR`.

## 6. Definition of done

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and
  `cargo test` all pass on the integrated branch.
- The PTY journeys pass, run on their own and in the full suite.
- Piped `watch` and `watch --json` are byte-identical to 0.4.10 on the same
  state, apart from timestamps.
- The herdr demo re-recorded with the candidate shows:
  1. three named Work items;
  2. the auth-ctx and me-endpoint interaction, named;
  3. auth-ctx integrated;
  4. me-endpoint changing to REFRESH;
  5. slugify unaffected;
  6. the stale accept refused with its reason and next action.

  Plus one narrow-terminal screenshot.

## 7. Deferred

- CLI refusal advice for hook-registered Work still says "attach it". It is CLI
  text, and this release preserves it.
- Scrolling and full display of long details.
- Group headings, and a `?` key for help.

## Progress log

- 2026-10-08: P0. Plan written. Pinned 0.4.10 verified (archive and binary
  hashes). `integration-0411` cloned on `release-0.4.11` from `main` `7c2074a`
  (includes #16). Supervisor started.
