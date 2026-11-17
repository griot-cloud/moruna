#!/bin/bash
# vm_owner_check.sh: a VM never outlives its `moruna run` (stream E, for D's launcher). Three
# cases on one-month Score, each started in the background as a non-interactive shell starts
# it (SIGINT inherited ignored):
#   kill:  SIGKILL the CLI once QEMU runs; no QEMU or vsock back end of that run may be left.
#   term:  SIGTERM once the job runs; the CLI exits cancelled (130) within GRACE_S seconds and
#          leaves nothing.
#   int:   the same with SIGINT.
# Prints one "CHECK name PASS|FAIL ..." line a case and exits non-zero when any fails.
#
#   MORUNA=/home/bench/gate/bin/moruna KIT=/data/boot-gate STACK=/data/gate/stack8s \
#     bench/vm/vm_owner_check.sh [WORK_DIR]
set -u
MORUNA=${MORUNA:-/home/bench/gate/bin/moruna}
KIT=${KIT:-/data/boot-gate}
STACK=${STACK:-/data/gate/stack8s}
INPUT=${INPUT:-/data/nyc/norm/yellow_tripdata_2024-01.parquet}
GRACE_S=${GRACE_S:-20}
HERE="$(cd "$(dirname "$0")" && pwd)"
WORK=${1:-/data/tmp/vm-owner-check}
rm -rf "$WORK"; mkdir -p "$WORK"
failed=0

# The QEMU and vsock back end processes of a run: those naming a node directory made after
# the run's job file.
run_procs() {
  for n in $(find "${TMPDIR:-/tmp}" -maxdepth 1 -name "moruna-node-*" -newer "$1" 2> /dev/null); do
    pgrep -f "qemu-system-x86_64.*$n|moruna-vhost-vsock.*$n"
  done
}

start() { # NAME: a run in the background; sets JOB, OUT and PID
  local d="$WORK/$1"; mkdir -p "$d/out"
  JOB="$d/job.json"; OUT="$d/out"
  python3 -I - "$JOB" "$OUT" "$(cat "$STACK/image.digest")" "$INPUT" "$HERE" <<'PY'
import sys
job, out, digest, inp, here = sys.argv[1:]
sys.path.insert(0, here)
import gate_jobs
gate_jobs.write(job, "score", [inp], out, {"digest": digest, "location": "/images/gate"}, cpus=8, mem_gib=2)
PY
  env PATH=/opt/qemu-kvm/bin:/usr/bin:/bin MORUNA_NODE_KIT="$KIT" "$MORUNA" run "$JOB" --output "$OUT" \
    --report "$d/report.json" --memory-gib 24 --image-stack "$STACK" --scratch-gib 1 > "$d/stdout" 2> "$d/stderr" &
  PID=$!
}

wait_for() { # SECONDS COMMAND...: until the command succeeds
  local until=$((SECONDS + $1)); shift
  until "$@"; do [ $SECONDS -ge $until ] && return 1; sleep 0.2; done
}

qemu_up() { [ -n "$(run_procs "$JOB")" ]; }
job_up() { grep -q '"type":"heartbeat"' "$(dirname "$JOB")/stdout" 2> /dev/null; }

check() { # NAME VERDICT DETAIL
  echo "CHECK $1 $2 $3"
  [ "$2" = PASS ] || failed=1
}

# kill: the owner dies at once.
start kill
if wait_for 60 qemu_up; then
  kill -KILL $PID; wait $PID 2> /dev/null
  sleep 3
  left=$(run_procs "$JOB" | tr '\n' ' ')
  if [ -z "$left" ]; then check kill PASS "no QEMU left 3 s after SIGKILL"
  else check kill FAIL "left: $left"; for p in $left; do kill -KILL $p; done; fi
else
  check kill FAIL "QEMU never started"; kill -KILL $PID 2> /dev/null
fi

for sig in TERM INT; do
  name=$(echo $sig | tr A-Z a-z)
  start $name
  if wait_for 90 job_up; then
    t0=$SECONDS
    kill -$sig $PID
    if wait_for $GRACE_S bash -c "! kill -0 $PID 2> /dev/null"; then
      wait $PID; code=$?; took=$((SECONDS - t0))
      sleep 1
      left=$(run_procs "$JOB" | tr '\n' ' ')
      # The cancel must end the run through the guest (its report written), not on the
      # signal's interrupted read.
      report="$(dirname "$JOB")/report.json"
      said=$(python3 -I -c "import json,sys; r=json.load(open(sys.argv[1])); print(str(r.get('diagnostic'))[:120], '| engine report:', 'yes' if r.get('notes') else 'no')" "$report" 2> /dev/null)
      if [ $code -eq 130 ] && [ -z "$left" ] && ! echo "$said" | grep -q "Interrupted system call"; then
        check $name PASS "exit $code in ${took} s, no QEMU left; $said"
      else check $name FAIL "exit $code in ${took} s, left: ${left:-none}; $said"; fi
      for p in $left; do kill -KILL $p; done
    else
      check $name FAIL "still running ${GRACE_S} s after SIG$sig"
      kill -KILL $PID; sleep 3; for p in $(run_procs "$JOB"); do kill -KILL $p; done
    fi
  else
    check $name FAIL "the job never started"; kill -KILL $PID 2> /dev/null
  fi
done
exit $failed
