# Work with Dispatch

Start from your project directory using the candidate's **absolute binary path**.
A goal is one supervised agent invocation, plus one continuation if the agent asks
an essential clarification question.

## Setup and resources

`dispatch setup` and the session's `/resources` action share the same handlers.
Every step is a menu: ↑/↓ (or j/k) move, a digit moves to that row, Enter chooses,
Esc goes back. `dispatch setup codex` or `dispatch setup claude` starts a new
resource; choosing an existing profile's row revalidates it without retyping
anything. Each profile row shows whether it is ready and, for Claude, when its
authorization expires. `dispatch resources` is noninteractive, does not probe
accounts, and reports configuration eligibility separately from launch-time
validation.

1. Install the provider CLI yourself. Setup resolves the named tool on PATH, not
   a repository-supplied harness override. Existing advanced overrides still work
   through the execution core; guided setup does not silently run them. A
   provider that is not on PATH is shown but cannot be chosen.
2. Choose a provider. Read-only discovery uses Codex account/read and rate-limit
   data, or Claude's controlled `auth status --json`. Unknown auth fails closed;
   unknown quota is not an invented balance. No model prompt is sent.
3. If login is needed, choose *Provider login…*, the provider, then *Open login*.
   Codex uses `login --device-auth`; Claude uses `auth login`. These can require a
   browser and can change the provider's saved account. Terminal ownership is
   restored on return. No OAuth token is extracted or stored by Dispatch.
4. Choose the model and effort. Codex lists its models itself (`model/list`), and
   setup offers only the efforts both Codex and Dispatch accept. Claude Code cannot
   list models, so setup suggests fixed model IDs. Models your profiles already use
   come first. *Other model ID…* takes an exact ID, which is validated and shown
   verbatim before you authorize. A listed model is never proof that your plan
   includes it.
5. Authorize. The authorization screen lists the CLI, the account fingerprint, the
   model, the effort, the service and every funding assertion you are making.
   Focus starts on *Cancel*, so Enter alone never authorizes; move to *Authorize
   and save* to save.
6. Claude additionally requires you to confirm that controlled print mode is
   included, usage credits are disabled and the account is unmanaged. Its
   authorization expires within 24 hours. Revalidate its row after expiry;
   cancelling leaves it expired.

The service writes `resources.yml` atomically, with mode 600, under a short
cooperating-writer lock. A changed file invalidates the displayed proposal.
Discovery/cancellation cannot renew a rejected epoch. A confirmed revalidation
advances its epoch; retained conflicts and actual account/funding evidence are
still checked by core at launch. Disabled profiles, explicit pool mappings,
unsupported service modes and changed Claude account scope require deliberate
advanced configuration or a separately confirmed new resource. Setup does not
change global provider settings.

`--plain`, `--ascii`, `--no-color` and `TERM=dumb` apply to setup too. In plain mode
each menu is a numbered list; type a number, or press Enter for the focused row
(Cancel on the authorization screen). Non-TTY setup refuses to prompt. There is no Dispatch login for ordinary local work.

## Checks and direct work

After intent, approve local execution for that goal. It uses a separate workspace
but is **not a security sandbox**. If no checks are configured, Dispatch offers the
checks it can detect: `verify.sh`, `Cargo.toml`, `Makefile`, `package.json`
(`npm test`), `go.mod` (`go test ./...`), and Python (`python3 -m pytest` when pytest
is configured, otherwise `python3 -m unittest` when `test_*.py` files exist). *Other
command…* takes one command line you type. Choosing or typing a command explicitly
approves saving and later executing it. The command may execute project
code and is not proof of task-specific correctness. No script or tool is installed.
Use `/checks` or `dispatch setup --checks` to do this independently. Advanced
`checks.verify` configuration remains supported.

`dispatch setup --checks` can also let Work in the project run its checks by
themselves: Work that a runtime reports then has them run when its worktree is
removed, and accept runs them on the merged tree.
- The screen shows the exact commands, and focus starts on Cancel.
- The permission lives in Dispatch's state, never in the repository, and names
  those exact commands. If `checks.verify` changes, it no longer holds until you
  approve the new commands; `dispatch status` and `dispatch watch` say which.
- It runs checks only. Applying still needs your review.
- Stop it on the same screen.

Work shows the actual selected resource, committed phase, elapsed time and launch
accounting. Recorded launches, uncertain spawn and configured limits are distinct;
counts are read from the committed per-attempt launch record. The elapsed indicator covers
this foreground work call, not full submission latency or only provider time.
Tool output remains in private logs; Dispatch does not invent file-reading activity
or turn a provider's tool success into authoritative verification.

Enter submits a goal; Alt+Enter adds a newline; bracketed paste never submits.
During work, input is paused and drained; Ctrl+C cancels with process cleanup.
No live steering or hidden next-goal queue exists. A goal interrupted by setup
returns as an editable draft and requires explicit submission. Plain mode prints
that preserved goal because cooked line input cannot prefill an editor.

## Decisions and review

A durable clarification is answered inside the same goal, targeting its exact
question/revision/generation. No agent process runs while a question waits. Ctrl+C
cancels; answers never reset limits or authorize spending. A question ends at the
goal's original deadline.

Review separates configured verification, human acceptance, and application:
`Unverified — no checks configured` is attention, not success. Enter/d opens native
changes; it never accepts. Choose `a` then Enter to accept and safely apply, `r`
then Enter to reject, `n` to leave pending, or `i` for evidence and full paths.
If the source moved and the result no longer holds, accepting it applies nothing,
records no review and leaves the result pending, so you can refresh or reject it.

While a result waits for review the source may keep changing. A Ready run's
status shows one `Coherence:` line (`CONTINUE`, `REFRESH` or `STOP`), computed
fresh from the run's baseline, its patch and the source as it is now; it is
display only and is never stored. `dispatch explain` adds a Coherence section
when the source moved or the run is a refresh.

- `dispatch check [run] [--json]` prints that verdict, the analysis level, how
  many files changed underneath the work, the reasons, and the next command
  (`dispatch accept`, `dispatch refresh` or `dispatch reject`). It changes
  nothing and exits 0 whenever the evaluation succeeds, whatever the verdict.
- `dispatch refresh [run]` starts a new run of the same task against the
  current source. The task gets a fixed addendum naming the earlier run and up to
  ten reasons it went stale. The old run is untouched. It repeats the original
  launch choices (fixed agent/model/effort, or the same harnesses) and asks again
  for `--allow-unsafe-local` / `--allow-forwarded-env` if the original run needed
  them. It is always a new, human-typed launch; nothing refreshes automatically.

`check`, `refresh` and the `Coherence:` line act only on a finished, ready result
that is not yet applied (`check` and the line also need exactly one candidate,
which any single-agent run has). The verdict rules, limits and events
are in the [coherence reference](coherence.md); the README has the short version.

### Auto-apply

A session mode applies eligible results automatically instead of waiting for your
review. It is off by default in every new session and is never persisted between
sessions. Application is not acceptance: an auto-applied result never records a
human review, so `outcome.review` stays `pending` until you look at it.

Toggle it with **Shift+Tab** on any screen (the goal prompt, while working, or the
review menu), or with `/auto-apply on|off` (bare `/auto-apply` toggles) — the path
for `--plain` mode, where raw keys are not readable. The current mode is always the
first segment of the hint row:

| Mode | Unicode | ASCII |
|---|---|---|
| Off | `⏸ review before apply · Shift+Tab` | `[review before apply] Shift+Tab` |
| On | `⏵⏵ auto-apply on · Shift+Tab to pause` | `>> AUTO-APPLY ON - Shift+Tab to pause` |

Toggling prints one line: "Auto-apply on: eligible results will be applied without
review." or "Auto-apply off: results wait for your review." In `--plain` mode, the
goal prompt also gets `(auto-apply on)` appended while the mode is on.

The review menu carries one extra action, `[aa] Accept & apply, then auto-apply the
next results`: it records your acceptance exactly like `a`, applies the result, and
only then turns the mode on for later results. It never reinterprets a result you
have not looked at.

When a goal reaches a Ready, review-pending result under this mode, Dispatch
validates it against the current source and either applies it or drops into the
ordinary review menu with a notice explaining why:

- **Applied**: the session shows "Auto-applied · review not performed" plus the
  `Coherence:` line for a moved world, and returns to the goal prompt. No accept/reject
  attest offer follows, because no human review happened.
- **Skipped or blocked**: the session enters the review menu with a notice above the
  actions — "Auto-apply skipped: `<reason>`" (for example "no checks configured",
  "verification failed", "checks cannot run on the merged tree") or "Auto-apply
  blocked: `<Coherence: REFRESH/STOP line>`" (or the bare reason when no verdict was
  computed, for example a strict-mode drift).
- **Failed** (an apply that was authorized still failed, for example a Git error):
  "Auto-apply failed: `<error>`", also into the review menu.

Ctrl+C is not honored while an auto-apply attempt is in flight: integration checks
on the merged tree are not cancellable, the same as at accept time. The attempt is
bounded by the checks' own timeout.

**Headless.** `dispatch run --auto-apply "<task>"` and `dispatch refresh --auto-apply
[run]` apply the result the same way immediately after the run returns Ready, and
print one line: "Auto-applied Candidate `<label>` to `<source>` (`<n>` file(s)
changed). Review not performed." or "Not applied automatically: `<reason>`. Review
with dispatch check `<id>` or dispatch accept `<id>`." `--json` adds an `auto_apply`
object to the result (`outcome`: `applied`/`blocked`/`skipped`/`failed`; `reason`;
`coherence`; `files_changed`); `--jsonl` streams the run's own events as usual, then
a trailing `{"type":"auto_apply", "run_id", "auto_apply": {...}}` line. Exit code
`6` means the run finished Ready but was not applied automatically (skipped or
blocked); it is used only when the run's own exit code would otherwise have been
`0` — a run that already failed for its own reason (for example failed
verification) keeps that exit code unchanged.

Eligibility, in plain words: verification must be configured and must have passed;
on a moved source, the project's own checks must pass on the merged tree; a
`REFRESH` or `STOP` verdict is never applied. Nothing is ever refreshed
automatically under this mode. There is no `dispatch.yml` key for it: the policy
lives only in the process that owns the run (the TUI session or the CLI
invocation), never on the run record and never across a restart. See
[coherence.md](coherence.md#automatic-application-auto-apply) for the full
eligibility and authorization rules.

### What `explain` shows

`dispatch explain` prints a **Coherence** section when the run's current verdict
says the source moved or is not `CONTINUE`, or when the run is a refresh of an
earlier one. It lists the decision, the analysis level (`symbols`, `files_only` or
`integration`), whether the world changed and how many files, up to ten reasons
as `code: detail`, `first invalid at`, and `refreshed from` for a refreshed run.
For a Ready, unapplied run the verdict is recomputed for display; otherwise the
last stored verdict is used. Viewing it never writes anything.

A reason such as `fact_broken` carries the old and new signature
(`old => new`). `same_symbol_edited`, `patch_conflict`, `fact_missing`,
`integration_check_failed` (with the check and its log path) and
`analysis_uncertain` (a file that does not parse, or a check that could not run)
also make the verdict `REFRESH`; `already_applied` makes it `STOP`.

The line **Agent time after the work became invalid: 4m12s of 9m40s (43%)**
measures wall-clock time only. It sums the run's attempt durations, from their
recorded start and end times, and counts the part after `first invalid at`. It is
time, not dollars, on purpose: Dispatch does not know a transaction cost for every
harness (the Claude adapter discards the nominal figure), and a dollar amount would
be invented. `first invalid at` is stamped only when an invalid verdict is stored,
by the mid-run watcher or by a blocked accept. `check`, `status` and `explain` never
store one, and a blocked accept happens after the attempts have ended, so the
figure is nonzero only for a run whose watcher saw the change while the agent worked.

### While the agent works

For runs that use included-resource allocation, a watcher runs beside each attempt.
Every `coherence.poll_secs` it checks a cheap signal (Git `HEAD`, `git status`, and
file metadata; for a plain directory, a metadata walk). Only when that signal moves
does it observe the tree, capture the work so far with a temporary Git index, and
evaluate it with the file, patch and symbol layers. Integration checks never run
mid-run, and a file that does not parse is ignored, because a person may be
mid-edit. The watcher never writes state; the run's own loop records what it
reports, as events:

- `coherence.invalidated`: the verdict became `REFRESH` or `STOP`, or is still
  invalid for a different source state. At most one message per minute.
- `coherence.checked`: the verdict is `CONTINUE` again after an invalid one.
- `coherence.stopped`: recorded right before cancellation in `stop` mode.

The payload carries the full validity object under `coherence`. In the default
`observe` mode the agent is never touched. With `mid_run: stop`, a `STOP` verdict
cancels the attempt through the normal cancellation path, and so does `REFRESH`
if `stop_on_refresh: true`. The run then ends interrupted with work result
`cancelled`, failure kind `stale_work` and exit code 1; the message reads "work
stopped: the source changed underneath it (...)". The partial patch is kept, the
result cannot be accepted, and no review is recorded, so the agent is not counted
as having failed. The deadline
still takes precedence if it had already passed.

### `status --json`

`dispatch status --json` (and `run --json`) adds a `coherence` object only when the
source moved or the verdict is not `CONTINUE`: `decision` (`continue`, `refresh`
or `stop`), `analysis`, `changed_files`, and `reasons` (at most five, each with
`code`, `fact_id`, `path` and `detail`). It is absent for a run whose source did
not move, so existing consumers are unaffected.

Small diffs have an inline preview. Large sets open a file index: arrows/j/k select,
`/` filters, Enter opens, Esc returns to files, and q returns to the same review.
Within a file, arrows/h/l pan, arrows/j/k scroll, n/p navigate hunks, brackets switch
files, and Space loads another bounded page. `>` at the right edge marks clipping;
truncated pages say so. `v` opens a full patch pager and `e` the chosen editor.
Use a wide terminal for the complete shortcut footer; these bindings also work
at 38 columns. Generated-file labels are filename/directory hints (`generated?`),
not content classification, and never exclude data. Binary, rename, deletion and
permission changes remain in the exact candidate and index.

External tools inspect disposable exact-baseline/candidate copies. Closing a tool
is not approval; edits there are not imported. Missing/failed tools return to native
inspection. Use an explicit private `reviewer.json` preference as documented in
[review adapter details](phase4-ux-refinement.md); `$EDITOR` receives only supported
file arguments. Repository-supplied shell reviewer commands are never automatic.

One-shot JSON/JSONL uses the same foreground core independently of the renderer.
It runs in the foreground and starts no background process; watching a project is
the separate, explicit `dispatch start`.

## Using your own agent

`dispatch attach` puts work an external coding agent produces — Claude Code, Codex,
Cursor, a script, anything with a terminal — under the same coherence checking as a
run Dispatch launched itself, without Dispatch ever driving that agent. Full details,
every flag and every refusal message are in [attach.md](attach.md); this is what you
see day to day.

There are two forms.

**Wrap the agent**, when you are about to start it: Dispatch becomes the thing you
type instead of the agent, stays completely silent in your terminal while the agent
runs, and does its own work only once the agent exits.

```sh
dispatch attach --auto-apply -- claude -p "add input validation to the parser"
```

Ctrl+C, terminal resize and everything else behave exactly as if you had typed
`claude -p ...` yourself — the wrapper only ignores Ctrl+C/Ctrl+\ for its own process
while the agent runs (so they reach the agent, not it) and forwards a `kill`/hangup it
receives to the agent. When the agent exits, Dispatch freezes the patch, runs your
configured checks in that same worktree, and — only with `--auto-apply` — applies it
if the checks pass and the source coherence gate allows it. Either way it prints one
line: what happened, and the next command (`dispatch check`/`dispatch accept`) if
anything is still waiting on you.

**Attach an already-running agent**, when it is already working in its own worktree
and you did not start it through Dispatch:

```sh
dispatch attach --workspace ../scratch-worktree --agent codex --auto-apply
dispatch finish <run-id>          # once you know the agent is done
```

`finish` is the only thing that decides a foreign attachment is done — there is no
timeout or idle heuristic. Until you run it, or until the agent's own driving script
does, the work stays observed but not judged.

Both forms need a **separate worktree**: attaching your own checkout in place is
refused (`attach needs a separate worktree; run git worktree add`), because Dispatch
cannot tell your edits from the agent's inside one tree. The snapshot Dispatch starts
from (S0) is the Git merge base of your worktree and the root's `HEAD` whenever both
are Git — full confidence, edits made before you ran `attach` are still counted. For a
plain (non-Git) directory, only the wrapped form works, and S0 is a snapshot taken at
the moment of attach — partial confidence: anything already changed before that moment
is invisible to the patch Dispatch judges.

**`dispatch start`** watches the project in the background, one owner per
repository, and returns your shell. The owner:
- keeps every attached run with no live owner observed: a foreign attachment, or a
  wrapped attach whose wrapper process died;
- keeps the verdict of every result waiting for your review current as the code
  moves, native or attached;
- applies the attached work you marked `--auto-apply` once it is ready and coherent.

**`dispatch watch`** shows the project view under a line saying who watches. It
redraws as things change, and leaving it does not stop watching. Piped, or with
`--plain` or `--json`, it prints one line per run,
`<id> · agent · CONTINUE/REFRESH/STOP · working/question/ready/applied/blocked · reason`.

On a terminal it is a table: WORK, AGENT, STATE, VERDICT, CHECKS and TOUCHES,
one row per Work. Work is named by its worktree folder, else its task, else its
short ID. Work that needs you (a question, ready, blocked, a removed or lost
worktree) comes first, then Work in progress, then Work that is done, which
shows `–` as its verdict. Below the table, the selected Work's details: how it
came to be listed, its verdict and reasons (or how it landed), its checks, the
Work it touches, what it began against (a Dispatch snapshot is called that, not
a commit), and the next thing to do. Below 90 columns each Work is a two-line
card. The selection follows the Work, not the row: if the selected Work leaves
the list, the next key only says where the selection went. ↑/↓ select, and a
key acts on the selected Work as the command would; the bottom line offers the
keys that apply:
- `f` finishes attached Work, asking first, naming it, before running checks
  nobody has allowed;
- `a` accepts, through the same gate as `dispatch accept`; a refusal names the
  Work, its verdict's reason and what to do instead;
- `r` rejects, after asking, naming it, with focus on Cancel;
- `d` or Enter opens the review;
- `q` leaves.

`NO_COLOR`, `--no-color` and `--ascii` work as everywhere else; color only
repeats what the words say.

The TOUCHES column names the other Work not yet integrated that this Work
already touches, and the selected Work's details say how, naming both, for
example "auth-ctx changes the signature of validate (auth.py), which me-endpoint
uses". `dispatch status` shows the same section. This is advisory and separate from
the verdict: it blocks nothing, and once one piece lands the other is judged
against the project as before. **`dispatch stop`** ends watching. `dispatch serve` is
the same owner in the foreground, with the view.

Watching needs no agent profile. It covers:
- the Work Dispatch launched;
- the Work you attached;
- sessions a runtime reports.

It never scans for agent processes. Wrapped attach and the TUI need no owner at
all.

**Work that appears by itself.** `dispatch setup` → **Runtime integrations…** installs
Claude Code's hooks into `~/.claude/settings.json`: `SessionStart`, `SessionEnd`,
`WorktreeRemove`, and `PreToolUse` for the `ExitWorktree` tool only.
- **Consent:** the screen shows exactly what is added, and focus starts on Cancel.
  A backup of the previous settings is kept, and **Remove** takes out exactly what
  was added. These hooks observe; they involve no account, model or funding.
- **What they do:** in a project you watch, a Claude Code session in its own
  worktree (`claude --worktree`) becomes Work by itself, with S0 taken before its
  first edit. When Claude Code removes the worktree, its exact changes are kept
  first. If you allowed the project's checks to run by themselves, Dispatch then
  runs them and the Work waits for your review. Otherwise it waits for you to
  `finish` or `reject` it.
- **Sessions in your checkout:** a fresh one is told that Dispatch cannot follow it.
- **Other agents:** `dispatch attach -- <agent>` from your checkout gives any agent
  CLI a workspace of its own. It is removed once the work is applied. After a
  reject it stays until `dispatch clean`:
  - `clean` lists the leftover workspaces Dispatch made and asks before removing
    them;
  - `--dry-run` only lists them;
  - `--yes` skips the question where there is no terminal.

What shows in the CLI: `dispatch status` and `dispatch check` treat an attached run
exactly like any other single-result run once it is finished — same `Coherence`
section, same accept/reject/apply commands. `dispatch explain` shows an **Attached
work** section (workspace, root, where S0 came from and with what confidence, the
agent, who owns the work, what it may do) in place of a selection explanation, then
the verdict. `dispatch refresh` has no
target for attached work (there is no Dispatch task to relaunch) and is refused;
review it and `dispatch attach` again if you want another pass. In `--json`, an
attached run appears with `mode: "attached"`; every other run is `mode: "native"`.

No foreign process is ever signaled or killed by Dispatch, and a `REFRESH`/`STOP`
verdict on attached work is only ever recorded, never enforced against the agent.
