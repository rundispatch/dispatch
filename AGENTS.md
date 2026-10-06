# Dispatch contributor instructions

These instructions apply to the entire repository.

## Product thesis

Dispatch keeps autonomous software work valid while the code moves. An agent
works from a frozen snapshot while the real source keeps changing. Dispatch
records the snapshot the work began against and the patch it produced, derives
the facts the work relied on, and decides whether the result still holds against
the source as it is now:

```text
task
→ S0: frozen baseline, isolated candidate workspace
→ one selected agent executes and configured verification runs
→ Δ: the patch, retained with its evidence
→ validate(facts of (S0, Δ), source now)
→ CONTINUE / REFRESH / STOP, with reasons
→ human review, accept or reject
→ durable local data
```

Work coherence is the center of the product and the public story. Agent
selection, allocation, isolated execution, verification and cost accounting are
how Dispatch carries the work; they support the promise and are not separate
product theses. Keep selection local and explicit: one configured profile or an
explicit `--agent`, fed into the same execution core.

Public claims about coherence must match what is shipped and measured. Today
that is an accept-time gate with symbol-level facts for Rust and Python, file-level
facts elsewhere, an advisory mid-run watcher on native runs, and advisory
interactions between unintegrated Work while a project is watched. Claim
precision for interactions only from recorded outcomes (the documented queries),
quoted as counts, and only as agreement with Dispatch's own later verdict. Do not claim
tokens, minutes or money saved until a real run was stopped and the attempt
timestamps show it. The metrics that decide whether the thesis holds, and what
would falsify it, are in `docs/coherence-validation.md`.

## Architectural ownership

**Rust owns execution.** Rust owns the CLI, task/run orchestration, process supervision, harness adapters, source state, Git/worktrees/internal snapshots, execution backends, limits and timeouts, verification, diff and artifact capture, events, human review, and SQLite/local persistence.

If explicitly requested later, **Go may own networked coordination and learning**: authentication, teams, server-side ingestion, aggregate statistics, training-data processing, and server-side routing services. Do not implement Go or cloud services in this repository.

Cloud must never be required to execute a normal local Dispatch run.

Dispatch uploads nothing and fetches no benchmark data.

## Keep Dispatch small

The “1K LOC” idea is a discipline, not a hard numeric constraint. Optimize for minimal conceptual surface, not code golf.

1. Code is expensive.
2. Do not add an abstraction until the current code demonstrates the need.
3. Prefer the standard library and existing operating-system, Git, Docker, and SQLite primitives.
4. Prefer a thin adapter over an internal framework.
5. Prefer one supported path over multiple half-supported paths.
6. Delete code replaced by a new path.
7. Do not retain obsolete architecture merely for compatibility during early development.
8. Avoid dependencies that save only trivial code.
9. Treat growing production LOC as a reason to re-evaluate the design.
10. Never sacrifice correctness or clarity to reach a numeric LOC target.

Use explicit control flow and simple, inspectable data structures. A strong engineer should be able to follow the important execution path without navigating an internal platform.

## Threshold for product changes

Production changes should normally address an observed product defect, correctness problem, reliability problem, performance limitation, security issue, meaningful user friction, or data-integrity problem.

Synthetic experiments can motivate more research; they do not automatically justify features.

- Observed process leak → fix it.
- Misleading candidate state → fix it.
- An LLM suggests AST scoring → do not build it.
- A six-way tournament sounds useful → require evidence and explicit direction first.

Do not introduce speculative abstractions, duplicate execution paths, or infrastructure for hypothetical scale.

## Human evaluation is the quality signal

Do not substitute an LLM's code-quality opinion for human preference data.

Agents may mechanically analyze test/build/lint outcomes, runtime, tokens, diffs, changed files, artifacts, and process behavior. Do not encode subjective agent opinions into rankings, training labels, routing decisions, or product behavior.

Automated verification means the configured checks passed. It does not establish universal code quality. Human evaluation remains the quality/reward signal unless explicit product direction changes this policy.

## Measurement discipline

- Preserve raw observations and artifacts.
- Preserve reported token semantics; do not imply cross-harness equivalence.
- Keep unknown values unknown.
- Do not fabricate or silently estimate transaction cost.
- Retain Dispatch, harness, and model versions where available.
- Preserve failures, timeouts, and partial artifacts as data.
- Do not conflate automated checks with overall quality.
- Do not create composite quality scores without explicit product direction.

## Harness adapter discipline

Harness adapters stay thin. An adapter owns only harness-specific executable discovery, command construction, stdin behavior, environment needs, output parsing, usage extraction, and harness-specific failure semantics.

The generic execution core owns spawning, cancellation, timeout enforcement, stdout/stderr capture, lifecycle, process cleanup, and event emission. Keep harness-specific workarounds inside the adapter rather than scattering them through orchestration or execution.

Do not introduce an internal plugin framework without multiple demonstrated integrations that cannot remain simple adapters.

## Local-first and source safety

Ordinary local directories, non-Git projects, existing Git repositories, and linked worktrees are first-class inputs. GitHub, a remote repository, an account and cloud services must remain optional.

During evaluation, harnesses work in independent candidate states rather than directly in the user's original source. Preserve snapshot fidelity, candidate isolation, source-drift checks, and explicit apply semantics. Never weaken the unsafe-local acknowledgement or imply that local execution is sandboxed.

## Scope guard

Unless explicitly requested, do not add:

- web UI or dashboards;
- cloud services, authentication, billing, or team features;
- Kubernetes, queues, Redis, or premature APIs/services;
- generic plugin frameworks;
- embeddings, ML frameworks, subjective LLM judges, or routing-driven execution beyond explicitly approved experiments;
- automatic retries or speculative subtask orchestration;
- synthetic or composite quality scores;
- new execution backends or benchmark infrastructure.

Do not add any upload of source, patches, logs, task text or telemetry without a separate explicit product and consent decision. Do not put server-side cloud, routing, or learning implementation in the public core.

Do not opportunistically broaden a focused task.

**Work coherence.** Dispatch checks finished work against a source tree that moved
underneath it (see `docs/coherence.md`). tree-sitter is an approved dependency solely
for symbol extraction in that layer. Unless explicitly requested, do not add:

- automatic refresh or retry (`dispatch refresh` stays an explicit, human-typed launch);
- a lock manager or pessimistic locking of files or symbols;
- a persistent symbol or reference graph, index, or LSP integration;
- additional languages or a plugin framework for them.

Additional languages, or any graph or index, need evidence from failures observed in real
use first. Keep facts derived from (S0, Δ), the run's baseline and its patch, so that
they are always recomputable and need no stored index.

## Working method

When changing Dispatch:

1. Inspect the implementation and current tests.
2. Reproduce the issue when applicable.
3. Make the smallest coherent change.
4. Add a focused regression test.
5. Run the relevant suite.
6. Inspect the final diff and production LOC change.
7. Delete unnecessary or replaced code.
8. Report what changed, why, and what was deliberately not built.

Do not refactor unrelated modules. Preserve user changes in a dirty workspace.

Standard validation is:

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

## Definition of a good change

A good Dispatch patch makes the system more correct, reliable, understandable, efficient, or smaller without needlessly increasing conceptual surface.

If a patch adds substantial machinery, its burden of justification is high. Prefer the smallest design that fully preserves correctness, safety, and clarity.
