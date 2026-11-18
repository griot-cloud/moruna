# The twelve normalised months of 2024, made inside Azure on nyc-blob.sh's machine (work item
# 3.8b): run once, as root, by the machine's managed run command, never on the Mac.
#
# nyc-blob.sh sends this file as the run command's script, after three lines of its own:
#
#   #!/bin/bash
#   CODE='<base64 of a tar.gz of data-linux.sh, dataprep.py, store.py, normalize.py and the plan>'
#   export BENCH_ACCOUNT=<account> BENCH_CONTAINER=data
#
# The account key arrives as BENCH_KEY, a protected parameter the handler puts in the script's
# environment (bench/node/azure.py, run_command_create): never in the script's text. 6.2b's
# data-linux.sh and dataprep.py then fetch each month from NYC TLC's public address, normalise it
# with bench/vm/normalize.py under the plan's pyarrow, check it against the plan's SHA-256, and
# only when all twelve match upload them to the container under `nyc/`. The last line is
# dataprep.py's `DATA {json}`, which nyc-blob.sh reads from the run command's output. The code goes
# to /opt/nyc (NYC_DATA_ROOT names another place, as the tests do).
set -euo pipefail
: "${CODE:?nyc-blob.sh puts the code before this script}"
: "${BENCH_ACCOUNT:?the storage account}" "${BENCH_CONTAINER:?the container}"
: "${BENCH_KEY:?the account key, a protected parameter}"
root="${NYC_DATA_ROOT:-/opt/nyc}"
mkdir -p "$root"
printf '%s' "$CODE" | base64 -d | tar -xz -C "$root"
plan="$root/bench/node/plans/nyc-2024.json"
pyarrow="$(python3 -I -c 'import json, sys; print(json.load(open(sys.argv[1]))["dataset"]["pyarrow"])' "$plan")"
spec="$(python3 -I -c 'import json, sys; d = json.load(open(sys.argv[1]))["dataset"]; print(json.dumps({"source_url": d["source_url"], "sha256": d["sha256"], "prefix": "nyc/", "normalize": sys.argv[2]}, separators=(",", ":")))' "$plan" "$root/bench/vm/normalize.py")"
exec bash "$root/bench/node/engines/data-linux.sh" "$pyarrow" "$spec"
