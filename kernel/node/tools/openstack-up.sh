#!/usr/bin/env bash
# Brings the node up on Servercore, checks it, and deletes everything it made (work item 1.5b;
# system design 1E): the steps of kernel/node/OPENSTACK.md, in order, each the same command.
# With --proof it runs the proof on the node too (work item 3.8b), and every run ends with the
# API's checks from outside the cloud (work item 2.6).
#
#   kernel/node/tools/openstack-up.sh --qcow2 OUT/moruna-node.qcow2 --run RUN --region REGION
#       --boot image [--name moruna-node-NAME] [--cloud servercore]
#       [--proof --blob FILE --tasks DIR --nyc DIR --gate-canon BIN] [--dry-run]
#   kernel/node/tools/openstack-up.sh --qcow2 OUT/moruna-node.qcow2 --run RUN --region REGION
#       --boot volume --zone ZONE [--name moruna-node-NAME] [--cloud servercore] [--dry-run]
#
# - --qcow2: package.py's qcow2 image (system design 1A).
# - --run: a new directory for the run's files (the token key pair, node config, the IDs, the
#   console log and its errors, the times). It is made with mode 700: it holds the run's private
#   key. Keep it outside the repository.
# - --region: the Servercore region everything is made in, for example ru-3.
# - --boot: `image` boots the server from the image, on flavor BL1.1-2048-16's own disk; `volume`
#   boots it from a 2 GB volume made from the image, on BL1.1-2048, which has no disk (Servercore
#   refuses a volume boot on a flavor with one).
# - --zone: with --boot volume only, and then required: the availability zone the volume and the
#   server are made in, for example ru-3b. The volume's type is basic.ZONE.
# - --name: the one name of everything the run makes, `moruna-node-<yyyymmddHHMM>` when not given.
# - --proof: the four NYC jobs through the node's API (work item 3.8b), on flavor SL1.4-16384-128
#   (4 vCPUs and 16 GiB, as Azure's D4s_v4) in place of BL1.1-2048-16; with --boot image only, as
#   that flavor has its own disk. Node config also holds a function key the run makes (system
#   design 3E). Once the node is ready and checked, bench/node/proof.py registers each gate task
#   at its address in nyc-blob.sh's `functions` container, with that container's read-only SAS;
#   runs Score, Occupancy, Enrich and Audit on 2024-01 and on all of 2024, reading `nyc/` and
#   writing `proof/<stamp>/` in nyc-blob.sh's data container with that container's SAS (by secret
#   name, 3B), from Azure over the internet; and checks every output against 0.6.6's references.
#   Its results are kept in RUN/proof. Both SASes reach proof.py in its environment only. It needs:
#   - --blob: nyc-blob.sh's mode-600 file, which names the storage account and its two containers
#     and holds their SASes;
#   - --tasks: the gate tasks built for the core (images/gate/tasks/build.sh's core/bin), at least
#     gate-score, gate-occupancy, gate-enrich and gate-audit: the ones nyc-blob.sh uploaded;
#   - --nyc: the directory of 0.6.6's references, reference-JOB-PERIOD.json (~/moruna-data/nyc);
#   - --gate-canon: gate-canon built for this machine (images/gate), which checks the year.
# - --dry-run: prints every command, one to a line, and runs none of them.
#
# Every run ends with the API's checks (work item 2.6, kernel/node/tools/api-checks.py), after the
# proof when there is one, whether or not it passed: health, the node document over HTTPS and
# over MCP, and a token from another key and one for another instance each refused, every
# connection pinned to the certificate the console printed. Their results are kept in
# RUN/api-checks.json.
#
# A guard refuses, before anything runs, any cloud but servercore and any name not
# `moruna-node-*`, and refuses a name that the project's images, security groups, networks,
# subnets, routers, volumes or servers already have: the script deletes only what it made, by the
# IDs it kept. Every `openstack` call names the cloud and the region, and is bounded: `perl`'s
# alarm ends it after 120 s (the image's upload after 600 s). Whatever happens, everything the run
# made is deleted when it ends, in the reverse of the order it was made, and the project is shown
# to hold nothing of the name.
#
# It exits 0 when every check of 1E passes: the server is ACTIVE, the console shows
# `moruna: ready` and `moruna: cloud openstack`, the floating IP accepts on 443 and not on 22, and
# nothing of the run is left; and when the API's checks pass and, with --proof, all eight runs of
# the proof. It prints create-to-ready and boot_ms. Any failure exits 1, after the
# console is read into RUN once more and everything is deleted; a refusal exits 2.
set -euo pipefail

cloud=servercore
region=""
zone=""
boot=""
name=""
qcow2=""
run=""
dry=0
proof=0
blob=""
tasks=""
nyc=""
gate_canon=""
# The repository this script is in, whose bench/node/proof.py and api-checks.py run.
repo="$(cd "$(dirname "$0")/../../.." && pwd)"
openssl="${MORUNA_OPENSSL:-}"
if [ -z "$openssl" ]; then
  # macOS's own openssl is LibreSSL, which has no Ed25519 (kernel/node/OPENSTACK.md, step 1).
  if [ -x /opt/homebrew/bin/openssl ]; then openssl=/opt/homebrew/bin/openssl; else openssl=openssl; fi
fi

refuse() {
  echo "openstack-up.sh: refused: $*" >&2
  exit 2
}

usage() {
  echo "usage: openstack-up.sh --qcow2 QCOW2 --run RUN --region REGION --boot image|volume [--zone ZONE] [--name moruna-node-NAME] [--cloud servercore] [--proof --blob FILE --tasks DIR --nyc DIR --gate-canon BIN] [--dry-run]" >&2
  exit 2
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --qcow2) [ "$#" -ge 2 ] || usage; qcow2="$2"; shift 2 ;;
    --run) [ "$#" -ge 2 ] || usage; run="$2"; shift 2 ;;
    --region) [ "$#" -ge 2 ] || usage; region="$2"; shift 2 ;;
    --zone) [ "$#" -ge 2 ] || usage; zone="$2"; shift 2 ;;
    --boot) [ "$#" -ge 2 ] || usage; boot="$2"; shift 2 ;;
    --name) [ "$#" -ge 2 ] || usage; name="$2"; shift 2 ;;
    --cloud) [ "$#" -ge 2 ] || usage; cloud="$2"; shift 2 ;;
    --proof) proof=1; shift ;;
    --blob) [ "$#" -ge 2 ] || usage; blob="$2"; shift 2 ;;
    --tasks) [ "$#" -ge 2 ] || usage; tasks="$2"; shift 2 ;;
    --nyc) [ "$#" -ge 2 ] || usage; nyc="$2"; shift 2 ;;
    --gate-canon) [ "$#" -ge 2 ] || usage; gate_canon="$2"; shift 2 ;;
    --dry-run) dry=1; shift ;;
    *) usage ;;
  esac
done
[ -n "$qcow2" ] && [ -n "$run" ] && [ -n "$region" ] && [ -n "$boot" ] || usage
[ -n "$name" ] || name="moruna-node-$(date -u +%Y%m%d%H%M)"

# The guard: nothing runs, not even in a dry run, outside cloud servercore and a name of this
# script's own.
[ "$cloud" = servercore ] || refuse "cloud '$cloud' is not servercore"
case "$name" in
  moruna-node-*) ;;
  *) refuse "name '$name' is not moruna-node-*" ;;
esac
case "${name#moruna-node-}" in
  '' | -* | *- | *[!a-z0-9-]*) refuse "name '$name': after moruna-node- come lowercase letters, digits and inner dashes" ;;
esac
case "$region" in
  -* | *- | *[!a-z0-9-]*) refuse "region '$region' is not lowercase letters, digits and inner dashes" ;;
esac
case "$boot" in
  image) [ -z "$zone" ] || refuse "--zone goes with --boot volume" ;;
  volume)
    [ -n "$zone" ] || refuse "--boot volume needs --zone"
    case "$zone" in
      -* | *- | *[!a-z0-9-]*) refuse "zone '$zone' is not lowercase letters, digits and inner dashes" ;;
    esac ;;
  *) refuse "--boot is image or volume, not '$boot'" ;;
esac

[ -f "$qcow2" ] || refuse "no qcow2 image at $qcow2"

# The proof's inputs, every one there before anything runs, not even in a dry run.
flavor=BL1.1-2048-16
jobs="score occupancy enrich audit"
account=""
container=""
functions=""
if [ "$proof" -eq 1 ]; then
  flavor=SL1.4-16384-128
  [ "$boot" = image ] || refuse "--proof boots from the image: its flavor SL1.4-16384-128 has a disk of its own"
  { [ -n "$blob" ] && [ -n "$tasks" ] && [ -n "$nyc" ] && [ -n "$gate_canon" ]; } \
    || refuse "--proof needs --blob, --tasks, --nyc and --gate-canon"
  [ -f "$blob" ] || refuse "no blob file at $blob (nyc-blob.sh's)"
  # It holds the SASes: its owner's alone.
  case "$(ls -l "$blob")" in
    -r[w-]-------*) ;;
    *) refuse "$blob may be read by others than its owner, and it holds the SASes: make it mode 600" ;;
  esac
  value() { sed -n "s/^$1=//p" "$blob" | head -1; }
  account="$(value account)"
  container="$(value container)"
  functions="$(value functions)"
  case "$account" in
    '' | *[!a-z0-9]*) refuse "$blob: account '$account' is not 3 to 24 lowercase letters and digits" ;;
  esac
  { [ "${#account}" -ge 3 ] && [ "${#account}" -le 24 ]; } \
    || refuse "$blob: account '$account' is not 3 to 24 lowercase letters and digits"
  for name_of in "$container" "$functions"; do
    case "$name_of" in
      '' | -* | *- | *[!a-z0-9-]*) refuse "$blob: container '$name_of' is not lowercase letters, digits and inner dashes" ;;
    esac
  done
  for sas in data_sas functions_sas; do
    [ -n "$(value "$sas")" ] || refuse "$blob holds no $sas"
  done
  for j in $jobs; do
    [ -f "$tasks/gate-$j" ] || refuse "no gate task at $tasks/gate-$j"
    for period in 2024-01 2024; do
      [ -f "$nyc/reference-$j-$period.json" ] || refuse "no reference at $nyc/reference-$j-$period.json"
    done
  done
  { [ -f "$gate_canon" ] && [ -x "$gate_canon" ]; } || refuse "no gate-canon program at $gate_canon"
elif [ -n "$blob$tasks$nyc$gate_canon" ]; then
  refuse "--blob, --tasks, --nyc and --gate-canon go with --proof"
fi

if [ "$dry" -eq 0 ] && [ -e "$run" ]; then
  refuse "$run exists: the run's directory must be new"
fi

# A word as the shell reads it: as it is when it is plain, else in single quotes.
q() {
  case "$1" in
    '' | *[!A-Za-z0-9_./:=@,+%-]*) printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")" ;;
    *) printf '%s' "$1" ;;
  esac
}

R="$(q "$run")"
Q="$(q "$qcow2")"
O="$(q "$openssl")"
N="$name"
B="$(q "$blob")"
T="$(q "$tasks")"
Y="$(q "$nyc")"
C="$(q "$gate_canon")"
PROOF="$(q "$repo/bench/node/proof.py")"
CHECKS="$(q "$repo/kernel/node/tools/api-checks.py")"
# Every openstack call: bounded by perl's alarm, which the call keeps across exec and which ends
# it; `exit 127` when it cannot be run at all. Each names the cloud and the region.
BOUND="perl -e 'alarm shift; exec @ARGV; exit 127'"
OS="$BOUND 120 openstack --os-cloud $cloud --os-region-name $region"
UPLOAD="$BOUND 600 openstack --os-cloud $cloud --os-region-name $region"
IMAGE="\"\$(cat $R/image.id)\""
GROUP="\"\$(cat $R/group.id)\""
NETWORK="\"\$(cat $R/network.id)\""
SUBNET="\"\$(cat $R/subnet.id)\""
ROUTER="\"\$(cat $R/router.id)\""
VOLUME="\"\$(cat $R/volume.id)\""
SERVER="\"\$(cat $R/server.id)\""
FIP="\"\$(cat $R/fip.id)\""
# One read of the console: its errors kept in console.err, and console.log replaced only by a
# longer log (a server that has stopped may answer with none).
CONSOLE="$OS console log show $SERVER > $R/console.now 2> $R/console.now.err; cat $R/console.now.err >> $R/console.err; [ \"\$(wc -c < $R/console.now)\" -le \"\$(wc -c < $R/console.log)\" ] || cp $R/console.now $R/console.log"
STATUS="$OS server show $SERVER -f value -c status > $R/status"
ROUND="printf '%s %s, console %s bytes %s\\n' \"\$(date -u +%H:%M:%S)\" \"\$(cat $R/status)\" \"\$(( \$(wc -c < $R/console.now) ))\" \"\$(head -c 160 $R/console.now.err | tr '\\n' ' ')\""
# What the project holds of the name: each succeeds when it holds nothing of it.
NAMED=(
  "ids=\"\$($OS image list --name $N -f value -c ID)\" && [ -z \"\$ids\" ]"
  "names=\"\$($OS security group list -f value -c Name)\" && ! printf '%s\\n' \"\$names\" | grep -q -x -e $N"
  "ids=\"\$($OS network list --name $N -f value -c ID)\" && [ -z \"\$ids\" ]"
  "ids=\"\$($OS subnet list --name $N -f value -c ID)\" && [ -z \"\$ids\" ]"
  "ids=\"\$($OS router list --name $N -f value -c ID)\" && [ -z \"\$ids\" ]"
  "ids=\"\$($OS volume list --name $N -f value -c ID)\" && [ -z \"\$ids\" ]"
  "ids=\"\$($OS server list --name '^$N\$' -f value -c ID)\" && [ -z \"\$ids\" ]"
)

# Runs one command, as it is written, or in a dry run prints it. Every openstack call must be
# bounded and name cloud servercore and the region.
step() {
  local rest="$1"
  rest="${rest//"$BOUND 120 openstack --os-cloud servercore --os-region-name $region "/}"
  rest="${rest//"$BOUND 600 openstack --os-cloud servercore --os-region-name $region "/}"
  case "$rest" in
    *"openstack -"* | *"openstack "[a-z]*" "[a-z]*) case "$rest" in
        *"moruna: cloud openstack instance"*) ;;
        *) refuse "an openstack call not bounded, or not on cloud servercore and region $region: $1" ;;
      esac ;;
  esac
  if [ "$dry" -eq 1 ]; then
    printf '%s\n' "$1"
    return 0
  fi
  printf '[%s] + %s\n' "$(date -u +%H:%M:%S)" "$1" >&2
  bash -o pipefail -c "$1"
}

say() {
  [ "$dry" -eq 1 ] || printf '[%s] == %s\n' "$(date -u +%H:%M:%S)" "$*" >&2
}

# Whether a step of the deletion applies: in a dry run every one does; in a run, the ones whose
# files in RUN hold an ID.
made() {
  [ "$dry" -eq 1 ] && return 0
  local file
  for file in "$@"; do
    [ -s "$run/$file" ] || return 1
  done
}

# 1. The run's directory, the token key pair and node config (1D).
say "node config"
step "mkdir -m 700 $R"
step "$O genpkey -algorithm ed25519 -out $R/token-key.pem"
step "$O pkey -in $R/token-key.pem -pubout -outform DER | tail -c 32 | base64 > $R/token-key.pub"
if [ "$proof" -eq 1 ]; then
  # With --proof, a function key pair too: node config's one function key (3E), whose private
  # half signs the gate tasks proof.py registers.
  step "$O genpkey -algorithm ed25519 -out $R/function-key.pem"
  step "$O pkey -in $R/function-key.pem -pubout -outform DER | tail -c 32 | base64 > $R/function-key.pub"
  step "printf '{\"moruna\":1,\"api\":{\"token_keys\":[\"%s\"]},\"functions\":{\"keys\":[\"%s\"]}}\\n' \"\$(cat $R/token-key.pub)\" \"\$(cat $R/function-key.pub)\" > $R/node-config.json"
else
  step "printf '{\"moruna\":1,\"api\":{\"token_keys\":[\"%s\"]}}\\n' \"\$(cat $R/token-key.pub)\" > $R/node-config.json"
fi

# 2. The name, which must be new: from here on, whatever happens, what the run makes is deleted.
say "the name $N"
for check in "${NAMED[@]}"; do
  if ! step "$check"; then
    refuse "the project has something named $N already, or openstack could not say: this script deletes only what it made"
  fi
done

finish() {
  status=$?
  trap - EXIT
  set +e
  # A failed command's own status (a bound's 142, nc's, grep's) is a failed run, 1.
  [ "$status" -eq 0 ] || status=1
  say "deleting what the run made"
  if made server.id; then
    step "touch $R/console.log $R/console.err; $CONSOLE"
  fi
  made fip.id && step "$OS floating ip delete $FIP"
  made server.id && step "$OS server delete --wait $SERVER"
  if [ "$boot" = volume ] && made volume.id; then
    step "for _ in \$(seq 24); do $OS volume show $VOLUME -f value -c status > $R/volume.status; cat $R/volume.status; case \"\$(cat $R/volume.status)\" in in-use | detaching) sleep 5 ;; *) break ;; esac; done"
    step "$OS volume delete $VOLUME"
  fi
  made router.id subnet.id && step "$OS router remove subnet $ROUTER $SUBNET"
  made router.id && step "$OS router unset --external-gateway $ROUTER"
  made router.id && step "$OS router delete $ROUTER"
  made subnet.id && step "$OS subnet delete $SUBNET"
  made network.id && step "$OS network delete $NETWORK"
  made group.id && step "$OS security group delete $GROUP"
  made image.id && step "$OS image delete $IMAGE"
  say "what is left of $N"
  left=0
  for check in "${NAMED[@]}"; do
    step "$check" || left=1
  done
  if made fip.id; then
    step "ids=\"\$($OS floating ip list -f value -c ID)\" && ! printf '%s\\n' \"\$ids\" | grep -q -x -e $FIP" || left=1
  fi
  if [ "$left" -ne 0 ]; then
    echo "openstack-up.sh: something of $N may be left, or openstack could not say: see the commands above" >&2
    [ "$status" -ne 0 ] || status=1
  fi
  if [ "$dry" -eq 0 ] && [ "$status" -eq 0 ]; then
    printf 'create-to-ready %s s, boot_ms %s\n' \
      "$(( $(cat "$run/seen.at") - $(cat "$run/created.at") ))" \
      "$(tr -d '\r' < "$run/console.log" | grep -a -o 'moruna: ready .*boot_ms=[0-9]*' | head -1 | sed 's/.*boot_ms=//')"
  fi
  exit "$status"
}
trap finish EXIT

# 3. The image, with 1E's four properties.
say "upload"
step "$UPLOAD image create --disk-format qcow2 --container-format bare --property hw_firmware_type=uefi --property hw_machine_type=q35 --property hw_disk_bus=virtio --property hw_vif_model=virtio --file $Q -f value -c id $N > $R/image.id"
step "$OS image show $IMAGE -c status -c size -c disk_format -c container_format -c properties"
step "[ \"\$($OS image show $IMAGE -f value -c status)\" = active ]"

# 4. The security group: TCP 443 inbound, from anywhere.
say "security group"
step "$OS security group create --description 'Moruna node: TCP 443 inbound only' -f value -c id $N > $R/group.id"
step "$OS security group rule create --ingress --ethertype IPv4 --protocol tcp --dst-port 443 --remote-ip 0.0.0.0/0 $GROUP -f value -c id"
step "$OS security group rule list $GROUP"

# 5. The private network, and its router to external-network: no server goes on that directly.
say "private network and router"
step "$OS network create -f value -c id $N > $R/network.id"
step "$OS subnet create --network $NETWORK --subnet-range 192.168.77.0/24 --dhcp -f value -c id $N > $R/subnet.id"
step "$OS router create -f value -c id $N > $R/router.id"
step "$OS router set --external-gateway external-network $ROUTER"
step "$OS router add subnet $ROUTER $SUBNET"

# 6. The server, from the image or from a volume made from it. The clock starts when the create
# returns.
if [ "$boot" = image ]; then
  say "server, from the image"
  step "$OS server create --image $IMAGE --flavor $flavor --network $NETWORK --security-group $GROUP --user-data $R/node-config.json -f value -c id $N > $R/server.id && date -u +%s > $R/created.at"
else
  say "volume"
  step "$OS volume create --image $IMAGE --size 2 --bootable --type basic.$zone --availability-zone $zone -f value -c id $N > $R/volume.id"
  step "for _ in \$(seq 60); do $OS volume show $VOLUME -f value -c status > $R/volume.status; cat $R/volume.status; case \"\$(cat $R/volume.status)\" in available | error) break ;; esac; sleep 5; done; [ \"\$(cat $R/volume.status)\" = available ]"
  step "$OS volume show $VOLUME -c status -c size -c bootable -c volume_image_metadata"
  say "server, from the volume"
  step "$OS server create --volume $VOLUME --flavor BL1.1-2048 --availability-zone $zone --network $NETWORK --security-group $GROUP --user-data $R/node-config.json -f value -c id $N > $R/server.id && date -u +%s > $R/created.at"
fi

# 7. From the create on: the status and the console every few seconds, until the server leaves
# BUILD, at most 5 minutes. A node that stops shows SHUTOFF.
say "watching the server"
step "touch $R/console.log $R/console.err; deadline=\$(( \$(date -u +%s) + 300 )); while [ \"\$(date -u +%s)\" -lt \"\$deadline\" ]; do $STATUS; $CONSOLE; $ROUND; [ \"\$(cat $R/status)\" = BUILD ] || [ ! -s $R/status ] || break; sleep 2; done"
step "$OS server show $SERVER -c status -c addresses -c fault"
step "[ \"\$(cat $R/status)\" = ACTIVE ]"

# 8. The floating IP, from external-network, given to the server.
say "floating IP"
step "$OS floating ip create external-network -f value -c id > $R/fip.id"
step "$OS floating ip show $FIP -f value -c floating_ip_address > $R/address"
step "$OS server add floating ip $SERVER \"\$(cat $R/address)\""

# 9. The console, read on until the node's ready or fatal line, or the server stops; at most
# 5 minutes.
say "console"
step "deadline=\$(( \$(date -u +%s) + 300 )); while [ \"\$(date -u +%s)\" -lt \"\$deadline\" ]; do $STATUS; $CONSOLE; $ROUND; tr -d '\\r' < $R/console.log | grep -a -q -e 'moruna: ready' -e 'moruna: fatal' && break; [ \"\$(cat $R/status)\" = ACTIVE ] || [ ! -s $R/status ] || break; sleep 2; done; date -u +%s > $R/seen.at; tr -d '\\r' < $R/console.log | grep -a 'moruna: '"
step "tr -d '\\r' < $R/console.log | grep -a -q 'moruna: ready api='"
step "tr -d '\\r' < $R/console.log | grep -a -q 'moruna: cloud openstack instance '"

# 10. Check, from this machine: 443 accepts and 22 does not, and the server runs.
say "checks"
step "nc -vz -w 5 \"\$(cat $R/address)\" 443"
step "! nc -vz -w 5 \"\$(cat $R/address)\" 22"
step "[ \"\$($OS server show $SERVER -f value -c status)\" = ACTIVE ]"
# The certificate's fingerprint and the instance, from the console (1B): every call to the API
# from here on is pinned to the one, and its tokens are for the other.
step "tr -d '\\r' < $R/console.log | grep -a -o 'moruna: tls sha256=[0-9a-f]*' | head -1 | cut -d= -f2 > $R/console.pin && [ -s $R/console.pin ]"
step "tr -d '\\r' < $R/console.log | grep -a -o 'moruna: cloud openstack instance [^ ]*' | head -1 | cut -d' ' -f5 > $R/instance && [ -s $R/instance ]"

# 10, with --proof: the four NYC jobs through the API (work item 3.8b) on 2024-01 and on all of
# 2024, from and to nyc-blob.sh's data container, each output checked against 0.6.6's reference;
# the results in RUN/proof. The SASes reach proof.py in its environment, read from the blob file,
# never on a command line, and it redacts both wherever it would show them. A proof that fails
# fails the run, after the API's checks.
proof_failed=0
if [ "$proof" -eq 1 ]; then
  say "proof"
  step "BENCH_SAS=\"\$(sed -n 's/^data_sas=//p' $B)\" FUNCTIONS_SAS=\"\$(sed -n 's/^functions_sas=//p' $B)\" MORUNA_BENCH_OPENSSL=$O python3 -I $PROOF --url \"https://\$(cat $R/address):443\" --pin \"\$(cat $R/console.pin)\" --token-key $R/token-key.pem --instance \"\$(cat $R/instance)\" --function-key $R/function-key.pem --functions $T --function-base https://$account.blob.core.windows.net/$functions --data blob --blob-account $account --blob-container $container --periods 2024-01,2024 --references $Y --gate-canon $C --results $R/proof --work $R/proof-work" || proof_failed=1
fi

# 10, last: the API's checks from outside the cloud (work item 2.6), on every run.
say "the API's checks"
step "python3 -I $CHECKS --url \"https://\$(cat $R/address):443\" --pin \"\$(cat $R/console.pin)\" --token-key $R/token-key.pem --instance \"\$(cat $R/instance)\" --openssl $O --results $R/api-checks.json --cloud openstack"
[ "$proof_failed" -eq 0 ] || exit 1
