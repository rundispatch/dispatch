## Dispatch 0.4.8 — Work that notices Work

Dispatch has checked whether Work still holds against the project as it moves.
0.4.8 also shows where two pieces of Work that are not yet integrated already
touch each other, before either lands.

- **Where Work touches Work.** While a project is watched (`dispatch start`),
  Dispatch compares every piece of Work not yet integrated and reports when:
  - both change the same declaration or whole file, or edit overlapping lines;
  - one changes the signature of, or removes, a declaration the other uses;
  - one changes or deletes a file the other relies on.

  This is exact to the declaration for Rust and Python, and to the file
  elsewhere, and it says which.
- **What is not reported:** reading the same thing; different declarations of
  one file whose edits keep apart; and a change to a body alone that the other
  only calls. Work mid-edit whose file does not parse for a moment keeps its
  last clean analysis instead of raising a whole-file warning.
- **Where it shows:**
  - a `dispatch watch` row ends with `interacts with <id>`;
  - the selected row's `Concurrent` lines say how, for example "it changes the
    signature of validate (auth.py), which this Work uses";
  - `dispatch status` has a `Concurrent` section;
  - `watch --json` and `status --json` carry a structured `interactions` list.

  It sits apart from the verdict: Work can be `CONTINUE` and still interact.
- **Advisory only.** Nothing is blocked, reordered, refreshed or stopped. When
  one piece lands, it leaves the comparison and the other is judged by the
  coherence check, as before.

**Fixed.**
- **A Claude check that timed out no longer refuses the profile for good.**
  Under load, `claude --version` could take longer than its 5 s. Dispatch read
  the missing answer as "Claude CLI version changed" and refused the profile
  until you authorized it again. A `claude auth status` that did not answer
  was recorded the same way.
  - Now only a contradicting observation is a lasting refusal: a different
    version, a different account, a changed executable, or an expired
    approval.
  - A version check that gives no answer lets the run go ahead: the
    executable's hash, checked first, already fixes its version.
  - An account check that gives no answer refuses that one launch.
- **Work launched together is told apart.** IDs are shortened to 8 characters,
  which Work created within a quarter of a second shares. The view and
  interactions now show as much of each ID as tells them apart, and the removal
  hook's suggested commands carry the full ID.

**Evidence.** A real-agent trial with Claude Code 2.1.280 and Cursor Agent
started five pieces of Work together on one small Python project:
- A changed the signature of `auth.validate`;
- B added a caller of it;
- C changed only its body;
- E changed another function in the same file;
- D changed an unrelated module.

Before anything landed, Dispatch reported A and C changing the same
declaration, and A's signature change against B's new caller. The A-to-B
interaction was already there while B was still running under `dispatch
attach`. It reported nothing for C against B, E, or D.

After A was accepted, it left the comparison within two seconds. The
coherence check then refreshed B (its call no longer matches the signature)
and C (its patch no longer applies), and kept D and E at CONTINUE.

On a 2,000-file repository, comparing 50 pieces of Work costs 1 ms a tick.
Analysing one piece's changes costs about 150–260 ms, and is repeated only
when those changes do.

**Not built.** Interactions are not stored, not used by any policy, and not
known without a watching owner. There is no ordering advice, no automatic
refresh or stop, no transitive analysis, and no measured precision yet on real
repositories.

**Upgrading.** No migration; the schema stays at 24. Run `dispatch stop &&
dispatch start` so the project owner is 0.4.8.
