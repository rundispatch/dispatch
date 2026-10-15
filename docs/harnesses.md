# How the agent harnesses behave

What Dispatch observed of each coding agent's CLI while building 0.4.14
(2026-10-14/15, macOS). Dispatch's adapters and recognizers rely on these facts,
so re-check them when a harness changes version. The conformance results are in
[`harness-conformance.md`](harness-conformance.md).

| | Claude Code 2.1.296 | Codex 0.155.1 | Cursor Agent 2026.10.14 | Hermes Agent 0.18.2 |
|---|---|---|---|---|
| One-shot mode | `claude -p` | `codex exec` | `cursor-agent -p` | `hermes -z` |
| Output | `--output-format stream-json` | `--json` events | `--output-format stream-json`, plus a plain-text "Using worktree:" line in worktree mode | final text on stdout; usage in a `--usage-file` |
| Own worktree | `--worktree <name>`: `<repo>/.claude/worktrees/<name>`, branch `worktree-<name>` | `exec --enable worktrees --worktree` (feature flag): `~/.codex/worktrees/<4 hex>/<repo>`, detached HEAD | `-p --worktree <name>`: `~/.cursor/worktrees/<repo folder, lowercased>/<name>`, branch `<name>` | only `hermes chat --worktree` (`-z` ignores it): `<repo>/.worktrees/hermes-<8 hex>`, branch `hermes/<same>` |
| Mark in `.git/worktrees/<id>/` | `CLAUDE_BASE` (start commit) | `codex-thread.json` (`ownerThreadId`) | none | not checked |
| After the session | worktree kept; `-p` leaves its lock | kept | kept; a `worker-server` process outlives the agent | worktree and branch **removed with the agent's changes**; `.worktrees/` added to the checkout's `.gitignore` |
| Running process | Git lock names pid and start time | not observable | not observable | not observed |
| Lifecycle hooks | `SessionStart`, `SessionEnd`, `WorktreeRemove`, … (Dispatch installs some) | `SessionStart`, `Stop`, … (`~/.codex/hooks.json`) | not checked | shell hooks (`hermes hooks`) |
| Usage report | input, output, cache creation, cache read | input, cached input, cache write, output, reasoning | `inputTokens`, `outputTokens`, `cacheReadTokens`, `cacheWriteTokens` (camelCase) | usage file: input, output, cache, reasoning, total; null when unknown |
| A refused call | not observed | `turn.failed` with the provider's message; exit 1 | usage limit: `ActionRequiredError` on stderr, exit 1 | **exit 0**, message on stdout, `failed: true` in the usage file without a message |
| Credentials in a container | `ANTHROPIC_API_KEY`, or `claude setup-token` | `codex login --with-api-key` | `CURSOR_API_KEY` | provider key, e.g. `OPENROUTER_API_KEY` |
| Self-update | `DISABLE_AUTOUPDATER=1` | notice only | updates itself mid-run unless `channel: static` | notice only |

## Things that caught us out

- **Codex models.** A configured default can be refused under a ChatGPT login
  (`gpt-6.1-sol` was: HTTP 400), so the trials pass a model explicitly.
- **Cursor's free plan** ran out after about seven minutes of agent work, and
  resets monthly.
- **Hermes through a Claude subscription** is refused for third-party apps
  unless the account has "extra usage" credit. Its OpenAI Codex provider worked.
- **Hermes changes the checkout.** Its worktree mode writes `.worktrees/` into
  the checkout's `.gitignore`.
- **Git metadata names Cursor's path in lowercase,** so compare the folder
  name lowercased, on macOS and Linux alike.
