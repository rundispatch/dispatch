<p align="center"><img src="assets/dispatch-logo.svg" width="96" alt="Dispatch"></p>

# Dispatch

**Keep autonomous software work valid while the code moves.**

**Dispatch 0.4.17 — experimental developer preview**

<p align="center"><a href="https://rundispatch.sh/#explainer"><img src="https://rundispatch.sh/media/how-dispatch-works-short.gif" width="720" alt="How Dispatch works, in 30 seconds: two agents start from the same code; Dispatch sees where their changes touch; once one lands, the other no longer fits, and dispatch accept refuses it as REFRESH, leaving the source unchanged."></a></p>
<p align="center"><sub>30 seconds, no sound. The full minute and its transcript are at <a href="https://rundispatch.sh/docs/concepts#transcript">rundispatch.sh/docs/concepts</a>.</sub></p>

A coding agent works from a snapshot of your source while the real source keeps
changing: you edit files, another result is accepted, a teammate merges. Dispatch
keeps the snapshot the work began from (S0) and the patch it produced (Δ), and
before anything lands it checks whether the work still holds against the source as
it is now. The answer is a verdict with reasons, not a merge conflict:
**CONTINUE**, **REFRESH** or **STOP**. A stale result is never applied.

Everything Dispatch does is local. Provider execution requires the provider’s
service; Dispatch itself needs no account, no network service and no upload.

- **Your agents, as they are.** In a watched project, a Claude Code or Hermes session
  in its own worktree becomes Work by itself, Codex and Cursor Agent worktrees are
  found too, and any other agent works through `dispatch attach -- <agent>`.
  [Attach and watching →](docs/attach.md)
- **A verdict with reasons.** From S0 and Δ, Dispatch derives the facts the work relied
  on, declaration by declaration for Rust and Python and file by file elsewhere, and
  checks them against the source now. [Coherence →](docs/coherence.md)
- **Nothing lands unless it still holds.** When the source moved, `dispatch accept`
  runs your checks on the merged result first. A REFRESH or STOP is refused, and the
  source is left unchanged. [Review and accept →](docs/product-guide.md#decisions-and-review)
- **Checks you allow run by themselves.** Your consent names the exact commands. When
  a Claude Code or Hermes session ends, Dispatch freezes its change and runs them, and
  that finish never applies anything. A tree that passed is not checked again.
- **It shows what needs you.** While a project is watched, every Work's verdict stays
  current as the code moves. `dispatch watch` shows them, lists Claude Code sessions
  waiting for a permission or your next prompt first, and says, as advice, where two
  pieces of Work already touch.

## Install

```sh
curl -fsSL https://rundispatch.sh/install | sh
```

It installs `dispatch` into `~/.local/bin` for macOS (Apple silicon or Intel) or Linux
(x86_64 or aarch64), checked against the release's `SHA256SUMS`. Until the website
redirects that address, use
`https://github.com/rundispatch/dispatch/releases/latest/download/install.sh`. Manual
download, upgrades and uninstalling: [install, upgrade, uninstall](docs/release-install.md).

## Quick start

```sh
cd your-project
dispatch setup       # on a terminal: checks, consent, hooks, watching
claude --worktree    # work: your agent in its own worktree, or dispatch attach -- <agent>
dispatch accept      # once the result is ready for review
```

`dispatch setup` is one pass, and each step can be skipped. To let Dispatch launch the
agent instead, `dispatch run "<task>"` runs it from a frozen snapshot in a workspace of
its own. [Quick start →](https://rundispatch.sh/docs/quick-start)

## Safety

> **Local execution is not a security sandbox.**

Agents and project checks run with your user's permissions. Dispatch tracks an agent
only in a workspace of its own, never in your checkout, and applies only a result that
still holds against your source. [Safety and source behavior →](docs/reference.md#safety-and-source-behavior)

## Docs

- [rundispatch.sh/docs](https://rundispatch.sh/docs): quick start, concepts, guides,
  commands, configuration and verdicts.
- In this repository, versioned with the code:
  - [reference](docs/reference.md): commands, configuration and current boundaries;
  - [attach and watching](docs/attach.md): sessions, worktrees, hooks and the
    background owner;
  - [coherence](docs/coherence.md): facts, verdicts, merged checks and the fixture
    matrix;
  - [product guide](docs/product-guide.md): setup, checks, review and auto-apply;
  - [install, upgrade, uninstall](docs/release-install.md);
  - [coherence validation](docs/coherence-validation.md): what is claimed and measured,
    and what would show it wrong;
  - [building Dispatch with Dispatch](docs/self-hosting.md).

## For coding agents

Read [AGENTS.md](AGENTS.md) before changing this repository. To operate Dispatch for
someone, see the [agent guide](https://rundispatch.sh/docs/agents).

## Development

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

The tests need only Rust, `git`, `python3` and `sh`. `scripts/test-in-docker.sh` runs
them in CI's Linux image, and with `DISPATCH_DOCKER_STRESS=1` on a loaded machine, as CI
does on every pull request. [Testing and CI →](docs/reference.md#testing-and-ci)

## License

[MIT](LICENSE).
