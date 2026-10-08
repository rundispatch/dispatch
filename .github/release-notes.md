## Dispatch 0.4.11 — A clearer `dispatch watch`

A herdr demo with three Claude Code agents, each in its own worktree, showed that
`dispatch watch` judged the work correctly but was hard to read. Every Work item was one
sentence of eight to ten fields that wrapped across several lines, with no color. Work was
named only by IDs that differed in one character. The S0 hash shown was a Dispatch snapshot
commit that looked like an ordinary Git commit. If Work left the list, the selection could
silently move to other Work. 0.4.11 redesigns the interactive view only.

- **A table you can read.** At 90 columns or more, `watch` shows one line per Work, with the
  selected Work's details below:

  ```text
     WORK          AGENT    STATE     VERDICT     CHECKS      TOUCHES
   › me-endpoint   claude   blocked   REFRESH     passed      –
     auth-ctx      claude   applied   –           passed      –
  ```

  Below 90 columns, each Work is a two-line card. The selected Work's reason and next step
  stay visible down to 40 columns.
- **Work has a name.** A row shows the Work's worktree folder (`auth-ctx`), else its task,
  else a short ID. Duplicate names are told apart.
- **The details explain.** The selected Work's details show:
  - its verdict with the full reason, and, for Work still running, that the verdict is
    advisory;
  - its checks;
  - what it touches, naming both pieces of Work ("auth-ctx changes the signature of
    validate (tinyauth/auth.py), which me-endpoint uses");
  - what it began from, saying plainly when that is a Dispatch snapshot and not a commit
    on any branch;
  - what to do next.
- **The selection is the Work, not the row.** It stays on the same Work as the list
  changes. If the selected Work leaves the list, the next key only says where the selection
  went. Every action reloads the Work by ID first.
- **Feedback names the Work.** For example, "me-endpoint: not applied: stale (REFRESH). The
  source is unchanged. Next: r reject it." Messages age out. Confirmations name the Work
  they act on.
- **Restrained color.** Color marks only the selection, an actionable CONTINUE, REFRESH and
  STOP, failed checks, refusals, and landed Work, which is dimmed. The words always carry
  the meaning, and `NO_COLOR`, `--no-color` and `--ascii` work as before.
- **No phantom keys.** `watch` has no auto-apply mode, so it no longer shows `Shift+Tab` or
  reacts to it, including on its review screen. The hint lists only the keys that apply to
  the selected Work.

Unchanged: piped `watch`, `watch --plain`, `watch --json`, `serve`, `status` and every JSON
contract. Every CLI message and all coherence and interaction semantics are also unchanged.

Built with Dispatch: one implementation worker, an independent review and acceptance-test
worker, and a fix packet, each a Claude Code session in its own worktree, supervised by a
pinned Dispatch 0.4.10 and integrated one at a time after human approval.
