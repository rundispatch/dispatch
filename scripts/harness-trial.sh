#!/usr/bin/env bash
# Harness conformance trial (docs/harness-conformance.md, plan-0.4.14 §3 B).
#
# Runs the shared B/A/C scenario with one real agent on one path through
# Dispatch, keeps every command's raw output, and writes record.json:
#
#   scripts/harness-trial.sh <claude|codex|cursor|hermes> <attach|native|worktree>
#
# Environment: DISPATCH_BIN (default: dispatch on PATH), TRIAL_OUT (default:
# ./harness-trials), TRIAL_TIMEOUT_SECS (per agent invocation, default 600),
# TRIAL_MODEL_<HARNESS> (optional model override, e.g. TRIAL_MODEL_CODEX).
# Credentials come from the environment or the harness's existing login and
# are never written to the record.
#
# Exit status: 0 pass, 1 fail, 2 not offered, 3 skipped.
set -u

usage() {
  echo "usage: $0 <claude|codex|cursor|hermes> <attach|native|worktree>" >&2
  exit 64
}
H="${1:-}"
P="${2:-}"
case "$H" in claude | codex | cursor | hermes) ;; *) usage ;; esac
case "$P" in attach | native | worktree) ;; *) usage ;; esac

HERE="$(cd "$(dirname "$0")" && pwd -P)/harness-trial"
TRIAL="$HERE/trial.py"
TIMEOUT="${TRIAL_TIMEOUT_SECS:-600}"
WORK_WAIT_SECS=60 # bound on Work appearing, and its session ending, after a worktree agent exits
mkdir -p "${TRIAL_OUT:-./harness-trials}" || exit 1
OUT="$(cd "${TRIAL_OUT:-./harness-trials}" && pwd -P)/$H-$P"
rm -rf "$OUT"
mkdir -p "$OUT/steps" "$OUT/tasks" || exit 1
MODEL_VAR="TRIAL_MODEL_$(echo "$H" | tr '[:lower:]' '[:upper:]')"
MODEL="${!MODEL_VAR:-}"
printf '%s\n%s\n%s\n%s\n' "$H" "$P" "$TIMEOUT" "$MODEL" >"$OUT/trial.txt"
date -u +%FT%TZ >"$OUT/started-at.txt"
REPO=""
WATCHING=""

# Record a step: its command, stdout, stderr and exit code under steps/.
step() {
  local name="$1"
  shift
  printf '%q ' "$@" >"$OUT/steps/$name.cmd"
  "$@" >"$OUT/steps/$name.out" 2>"$OUT/steps/$name.err" </dev/null
  local code=$?
  echo "$code" >"$OUT/steps/$name.exit"
  return "$code"
}

# The trial ends before the scenario: write why, then the record.
early() {
  printf '%s\n%s\n' "$1" "$2" >"$OUT/early.txt"
  finish
}

finish() {
  date -u +%FT%TZ >"$OUT/ended-at.txt"
  python3 "$TRIAL" record "$OUT"
  exit $?
}

# --- Preflight: Dispatch, the path, the CLI, credentials --------------------

D="$(command -v "${DISPATCH_BIN:-dispatch}")" ||
  early fail "runner: DISPATCH_BIN '${DISPATCH_BIN:-dispatch}' is not executable"
step dispatch-version "$D" --version

if [ "$P" = native ] && ! step native-offered "$D" run --agent "$H" --help; then
  early "not offered" "$(head -n 1 "$OUT/steps/native-offered.err")"
fi

case "$H" in
  claude) CLI=claude ;;
  codex) CLI=codex ;;
  cursor) CLI=cursor-agent ;;
  hermes) CLI=hermes ;;
esac
command -v "$CLI" >/dev/null || early skipped "no CLI: $CLI is not on PATH"
step harness-version "$CLI" --version

# Credentials from the environment; on a developer machine an existing login
# counts. Hermes names its provider in its own config, so a missing one shows
# up as Hermes's own failure, recorded with its message.
case "$H" in
  claude)
    [ -n "${ANTHROPIC_API_KEY:-}${CLAUDE_CODE_OAUTH_TOKEN:-}" ] ||
      { step credentials claude auth status && grep -q '"loggedIn": *true' "$OUT/steps/credentials.out"; } ;;
  codex)
    if [ -n "${OPENAI_API_KEY:-}" ]; then
      export CODEX_HOME="$OUT/codex-home"
      mkdir -p "$CODEX_HOME"
      printenv OPENAI_API_KEY | codex login --with-api-key >"$OUT/steps/credentials.out" 2>&1
    else
      step credentials codex login status
    fi ;;
  cursor)
    [ -n "${CURSOR_API_KEY:-}" ] ||
      { step credentials cursor-agent status && grep -q 'Logged in as' "$OUT/steps/credentials.out"; } ;;
  hermes) true ;;
esac || early skipped "no credentials"

# --- The fixture: a fresh repository with its own DISPATCH_HOME --------------

# Cursor names its worktree directory after the project directory, so every
# trial's project directory name is unique.
REPO="$OUT/trial-$H-$P-$(date -u +%Y%m%dT%H%M%SZ)-$$"
export DISPATCH_HOME="$OUT/state"
cp -R "$HERE/fixture" "$REPO" || early fail "runner: cannot copy the fixture"
# Written here, not kept in the fixture: checkouts often ignore dispatch.yml.
printf 'checks:\n  verify:\n    - python3 -B -m unittest -q\nexecution:\n  timeout_secs: %s\n' \
  "$TIMEOUT" >"$REPO/dispatch.yml"
if [ "$P" = native ] && [ -n "$MODEL" ]; then
  printf 'harnesses:\n  %s:\n    model: %s\n' "$H" "$MODEL" >>"$REPO/dispatch.yml"
fi
if ! { git -C "$REPO" init -q -b main && git -C "$REPO" add -A &&
  git -C "$REPO" -c user.name=harness-trial -c user.email=harness-trial@localhost \
    -c commit.gpgsign=false commit -qm "fixture S0"; }; then
  early fail "runner: cannot commit the fixture"
fi
echo "$REPO" >"$OUT/repo.txt"
git -C "$REPO" rev-parse HEAD 'HEAD^{tree}' >"$OUT/fixture-s0.txt"
FIXTURE_TREE="$(git -C "$REPO" rev-parse 'HEAD^{tree}')"

cleanup() {
  [ -n "$WATCHING" ] && step stop "$D" stop --root "$REPO"
  WATCHING=""
  # Remove every worktree the trial made (Dispatch's, the harness's, wherever
  # it is) through the fixture repository, never by glob.
  [ -d "$REPO/.git" ] || return 0
  git -C "$REPO" worktree list --porcelain | sed -n 's/^worktree //p' | sed 1d |
    while IFS= read -r wt; do
      echo "remove $wt"
      git -C "$REPO" worktree remove --force --force "$wt" 2>&1
      rmdir "$(dirname "$wt")" 2>/dev/null # its parent too, only when left empty
    done >>"$OUT/cleanup.txt"
  git -C "$REPO" worktree prune
}
trap cleanup EXIT
trap 'exit 130' INT TERM

step start "$D" start --root "$REPO" || early fail "dispatch start failed: $(tail -n 1 "$OUT/steps/start.err")"
WATCHING=1

# --- One task: run the agent on the path, find its Work, keep the facts ------

# The non-interactive command for this harness; $2 names its own worktree.
agent_command() {
  local task wt
  task="$(cat "$HERE/tasks/$1.txt")"
  wt="${2:-}"
  case "$H" in
    claude)
      # No user settings: the agent's hooks must not reach another Dispatch state.
      CMD=(claude -p --output-format stream-json --verbose --permission-mode acceptEdits
        --setting-sources "" --settings '{"disableAllHooks":true}')
      [ -n "$MODEL" ] && CMD+=(--model "$MODEL")
      [ -n "$wt" ] && CMD+=(--worktree "$wt") ;;
    codex)
      CMD=(codex exec --json --sandbox workspace-write)
      [ -n "$MODEL" ] && CMD+=(-m "$MODEL")
      [ -n "$wt" ] && CMD+=(--enable worktrees --worktree) ;;
    cursor)
      CMD=(cursor-agent -p --force --trust --output-format stream-json)
      [ -n "$MODEL" ] && CMD+=(--model "$MODEL")
      [ -n "$wt" ] && CMD+=(--worktree "$wt") ;;
    hermes)
      if [ -n "$wt" ]; then
        # One-shot mode (-z) ignores --worktree (Hermes 0.18.2, run_oneshot);
        # only chat makes one: <repo>/.worktrees/<name>, branch hermes/<name>.
        CMD=(hermes chat -Q --yolo --worktree)
        [ -n "$MODEL" ] && CMD+=(-m "$MODEL")
        CMD+=(-q)
      else
        CMD=(hermes --yolo --usage-file "$OUT/tasks/$1/usage.json")
        [ -n "$MODEL" ] && CMD+=(-m "$MODEL")
        CMD+=(-z)
      fi ;;
  esac
  CMD+=("$task")
}

runs() { [ -d "$DISPATCH_HOME/runs" ] && ls "$DISPATCH_HOME/runs"; }

# The tree of a directory's files as Git would commit them, ignored files left
# out, written into the fixture repository only.
dir_tree() {
  local idx="$OUT/tasks/tmp.index"
  rm -f "$idx"
  (cd "$1" && GIT_DIR="$REPO/.git" GIT_WORK_TREE="$1" GIT_INDEX_FILE="$idx" git add -A . &&
    GIT_DIR="$REPO/.git" GIT_INDEX_FILE="$idx" git write-tree)
  rm -f "$idx"
}

# A tree with a patch applied.
patched_tree() {
  local idx="$OUT/tasks/tmp.index"
  rm -f "$idx"
  GIT_INDEX_FILE="$idx" git -C "$REPO" read-tree "$1" &&
    { [ ! -s "$2" ] || GIT_INDEX_FILE="$idx" git -C "$REPO" apply --cached "$2"; } &&
    GIT_INDEX_FILE="$idx" git -C "$REPO" write-tree
  rm -f "$idx"
}

# Runs task $1 and leaves its facts in tasks/$1/. Returns non-zero when the
# scenario cannot go on.
run_task() {
  local t="$1" TD="$OUT/tasks/$1" before id ws bp bc diff s0 agent delta
  mkdir -p "$TD"
  before="$(runs)"
  git -C "$REPO" worktree list --porcelain | sed -n 's/^worktree //p' >"$TD/worktrees-before.txt"
  date -u +%FT%TZ >"$TD/started-at.txt"
  case "$P" in
    attach)
      agent_command "$t"
      (cd "$REPO" && step "$t.attach" "$D" attach --agent "$H" --task "$(cat "$HERE/tasks/$t.txt")" \
        --allow-unsafe-local -- "$HERE/run-agent.sh" "$TD/agent" "$TIMEOUT" "${CMD[@]}") ;;
    native)
      step "$t.run" "$D" run --source "$REPO" --agent "$H" --json --allow-unsafe-local \
        --task-file "$HERE/tasks/$t.txt" ;;
    worktree)
      agent_command "$t" "trial-$t"
      (cd "$REPO" && "$HERE/run-agent.sh" "$TD/agent" "$TIMEOUT" "${CMD[@]}") ;;
  esac
  date -u +%FT%TZ >"$TD/ended-at.txt"
  git -C "$REPO" worktree list --porcelain | sed -n 's/^worktree //p' >"$TD/worktrees-after.txt"

  # The Work is whatever run appeared in this trial's state since the task
  # began: one agent works at a time.
  local waited=0
  while :; do
    id="$(runs | grep -Fxv -e "$before" | head -n 1)"
    [ -n "$id" ] || [ "$P" != worktree ] || [ "$waited" -ge "$WORK_WAIT_SECS" ] || {
      sleep 2
      waited=$((waited + 2))
      continue
    }
    break
  done
  echo "$id" >"$TD/run-id.txt"
  if [ -z "$id" ]; then
    # What Dispatch showed instead.
    "$D" watch --root "$REPO" --json >"$TD/watch.json" 2>&1 </dev/null &
    local w=$!
    sleep 3
    kill "$w" 2>/dev/null
    wait "$w" 2>/dev/null
    return 1
  fi

  { read -r ws && read -r bp && read -r bc && read -r diff; } < <(python3 "$TRIAL" work "$DISPATCH_HOME" "$id")
  echo "$ws" >"$TD/workspace.txt"
  # The agent's own change, read from its workspace before Dispatch freezes or
  # applies anything; then S0 and S0 + Δ as Dispatch recorded them.
  agent="$(dir_tree "$ws")" && echo "$agent" >"$TD/agent-tree.txt"
  if [ "$P" = worktree ]; then
    # As a person would: once Dispatch shows the session ended (bounded), finish.
    waited=0
    until python3 "$TRIAL" ended "$DISPATCH_HOME" "$id" || [ "$waited" -ge "$WORK_WAIT_SECS" ]; do
      sleep 2
      waited=$((waited + 2))
    done
    step "$t.finish" "$D" finish "$id" --allow-unsafe-local
  fi
  s0="$(git -C "$bp" rev-parse "$bc^{tree}")" && echo "$s0" >"$TD/s0-tree.txt"
  if [ "$s0" = "$FIXTURE_TREE" ]; then
    delta="$(patched_tree "$s0" "$diff")" && echo "$delta" >"$TD/delta-tree.txt"
  fi
  if [ -n "$agent" ]; then
    git -C "$REPO" diff --name-only "$FIXTURE_TREE" "$agent" >"$TD/agent-files.txt"
    git -C "$REPO" diff "$FIXTURE_TREE" "$agent" >"$TD/agent.patch"
    # The project's check on the agent's own tree, independently of Dispatch.
    mkdir -p "$TD/agent-tree" && git -C "$REPO" archive "$agent" | tar -x -C "$TD/agent-tree" &&
      (cd "$TD/agent-tree" && step "$t.agent-check" python3 -B -m unittest -q)
  fi
  step "$t.status" "$D" status "$id" --json

  # Go on only when the agent worked and changed something.
  case "$P" in
    native) [ "$(cat "$OUT/steps/$t.run.exit")" = 0 ] || return 1 ;;
    *) [ "$(cat "$TD/agent.status" 2>/dev/null)" = 0 ] || return 1 ;;
  esac
  [ -s "$TD/agent-files.txt" ]
}

# --- The scenario -------------------------------------------------------------

for t in b a c; do
  run_task "$t" || finish
done
B="$(cat "$OUT/tasks/b/run-id.txt")"
A="$(cat "$OUT/tasks/a/run-id.txt")"
C="$(cat "$OUT/tasks/c/run-id.txt")"

# Before anything lands, A and B are reported as interacting. The watcher
# evaluates on its own tick, so give it a bounded while.
for _ in $(seq 1 15); do
  step a.interactions "$D" status "$A" --json
  python3 "$TRIAL" interacts "$OUT/steps/a.interactions.out" "$B" && break
  sleep 2
done
step b.interactions "$D" status "$B" --json
step c.interactions "$D" status "$C" --json

step accept-b "$D" accept "$B"
step source-after-accept git -C "$REPO" diff
step check-a "$D" check "$A" --json
step check-a.text "$D" check "$A"
step check-c "$D" check "$C" --json
step check-c.text "$D" check "$C"

"$D" watch --root "$REPO" --json >"$OUT/steps/watch.out" 2>&1 </dev/null &
WPID=$!
sleep 3
kill "$WPID" 2>/dev/null
wait "$WPID" 2>/dev/null
cleanup
finish
