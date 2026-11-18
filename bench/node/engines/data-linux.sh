#!/bin/bash
# The plan's data, made on the benchmark's Linux machine (work item 6.2b): the first step of the
# setup's batch (batch.py), before the engines are installed, so the node's runs can start while
# the setup goes on. dataprep.py fetches the source files, normalises them with bench/vm/normalize.py
# in an environment with the plan's pyarrow (the version that wrote the Mac's copies), checks each
# against the plan's SHA-256 and uploads them to the run's container, within Azure.
#
#     data-linux.sh PYARROW SPEC
#
# Its settings arrive as environment variables: BENCH_ACCOUNT, BENCH_KEY, BENCH_CONTAINER (store.py).
# Its last line is dataprep.py's `DATA {json}`.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive HOME=/root
pyarrow=${1:?PYARROW, the version that wrote the copies on the Mac}
spec=${2:?SPEC, the data as JSON}
here=$(cd "$(dirname "$0")" && pwd)
# A first boot's apt runs may hold dpkg's lock for a while: wait for it rather than fail.
apt-get -o DPkg::Lock::Timeout=600 update -q >/dev/null
apt-get -o DPkg::Lock::Timeout=600 install -y -q python3-venv >/dev/null
[ -x /opt/dataprep/bin/python ] || python3 -m venv /opt/dataprep
/opt/dataprep/bin/pip install -q "pyarrow==$pyarrow"
mkdir -p /data/nyc
exec /opt/dataprep/bin/python "$here/dataprep.py" --spec "$spec" --work /data/nyc --python /opt/dataprep/bin/python
