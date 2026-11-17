#!/usr/bin/env bash
# Stops the node node-up.sh left running from OUT: the QEMU whose process ID is in OUT/qemu.pid,
# asked to end, and killed when it has not within 5 s. Its console log, OUT/console.log, and its
# frames, OUT/net.pcap, stay.
#
#   kernel/node/tools/node-down.sh OUT
#
# Exits 0 once it has ended, or when nothing was running; 2 on a usage error.
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: node-down.sh OUT" >&2
  exit 2
fi
out="$(cd "$1" && pwd)"
pidfile="$out/qemu.pid"
if [ ! -f "$pidfile" ]; then
  echo "node-down: no node up from $out" >&2
  exit 0
fi
pid="$(cat "$pidfile")"
case "$pid" in
  '' | *[!0-9]*) echo "node-down.sh: $pidfile does not hold a process ID" >&2; exit 2 ;;
esac
if kill -0 "$pid" 2>/dev/null; then
  kill -TERM "$pid" 2>/dev/null || true
  for _ in $(seq 50); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.1
  done
  if kill -0 "$pid" 2>/dev/null; then
    kill -KILL "$pid" 2>/dev/null || true
    echo "node-down: process $pid killed" >&2
  else
    echo "node-down: process $pid ended" >&2
  fi
else
  echo "node-down: process $pid had already ended" >&2
fi
rm -f "$pidfile"
