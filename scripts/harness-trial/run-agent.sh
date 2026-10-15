#!/bin/sh
# Run one agent command under a wall-clock limit. Writes <prefix>.stdout and
# <prefix>.stderr, and <prefix>.status: the agent's exit code, or "timeout"
# when the limit stopped it. harness-trial.sh runs every agent through this,
# so the agent's own output and outcome are kept whether or not Dispatch is
# its parent.
#
# Usage: run-agent.sh <prefix> <seconds> <command>...
prefix=$1
secs=$2
shift 2
# alarm() survives exec, so the limit applies to the agent itself, with no
# watchdog process left in its process group.
perl -e 'alarm shift; exec {$ARGV[0]} @ARGV or die "cannot run $ARGV[0]: $!\n"' "$secs" "$@" \
  </dev/null >"$prefix.stdout" 2>"$prefix.stderr"
code=$?
if [ "$code" -eq 142 ]; then echo timeout >"$prefix.status"; else echo "$code" >"$prefix.status"; fi
exit "$code"
