#!/usr/bin/env bash
# Brings the node up on Azure, checks it, and deletes everything (work item 1.13; system design
# 1E and 1F): the steps of kernel/node/AZURE.md, in order, each the same command.
#
#   kernel/node/tools/azure-up.sh --vhd OUT/moruna-node.vhd --run RUN --from ADDRESS
#       [--group moruna-node-NAME] [--subscription griot-dev] [--data-disk] [--job] [--dry-run]
#
# - --vhd: package.py's fixed-size VHD (system design 1A).
# - --run: a new directory for the run's files (the token key pair, node config, the IDs, the
#   console log and the times). It is made with mode 700: it holds the run's private key and the
#   storage account's key. Keep it outside the repository.
# - --from: the operator's public IPv4 address. The machine admits TCP 443 from it and from
#   nothing else.
# - --group: the resource group, `moruna-node-<yyyymmddHHMM>` when not given. Everything is made
#   in it, and it is deleted when the script ends, however it ends.
# - --data-disk: attaches an empty 4 GiB StandardSSD disk at LUN 0 when the machine is made, so
#   the node serves it as `lun-0` (a check image's storvsc-check reads, writes and tests it), and
#   prints the console's storvsc lines at the end.
# - --job: once the node is ready, runs a job through the API (3.2's kinds): a `rows` source of
#   10,000,000 rows into a `count` sink, with a token for `submit`. It validates the job, submits
#   it, follows its events over SSE to its `exit`, and checks the exit code 0 and the report's
#   `sinks[0].rows` 10,000,000, and prints the job's wall time from the report.
# - --dry-run: prints every command, one to a line, and runs none of them.
#
# A guard refuses, before anything runs, any subscription but griot-dev and any group not named
# `moruna-node-*`, and refuses a group that already exists: the script deletes only the group it
# made. Every `az` command names the subscription with --subscription; none sets a default.
#
# It exits 0 when every check of 1E passes on Azure: the node answers on 443 within 10 minutes,
# its console shows `moruna: ready` and `moruna: cloud azure`, 443 accepts and 22 does not, the
# served certificate is the one the console printed, and GET /v1/node answers; with --job, the job
# ends with exit 0 and counts its 10,000,000 rows. It prints create-to-ready and boot_ms, and with
# --job the job's wall_ms. Any failure exits 1, after the console log is read into RUN and the
# group deleted; a refusal exits 2.
set -euo pipefail

subscription=griot-dev
location=centralindia
group=""
vhd=""
run=""
from=""
dry=0
data_disk=0
job=0
openssl="${MORUNA_OPENSSL:-}"
if [ -z "$openssl" ]; then
  # macOS's own openssl is LibreSSL, which has no Ed25519 (kernel/node/OPENSTACK.md, step 1).
  if [ -x /opt/homebrew/bin/openssl ]; then openssl=/opt/homebrew/bin/openssl; else openssl=openssl; fi
fi

refuse() {
  echo "azure-up.sh: refused: $*" >&2
  exit 2
}

usage() {
  echo "usage: azure-up.sh --vhd VHD --run RUN --from ADDRESS [--group moruna-node-NAME] [--subscription griot-dev] [--data-disk] [--job] [--dry-run]" >&2
  exit 2
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --vhd) [ "$#" -ge 2 ] || usage; vhd="$2"; shift 2 ;;
    --run) [ "$#" -ge 2 ] || usage; run="$2"; shift 2 ;;
    --from) [ "$#" -ge 2 ] || usage; from="$2"; shift 2 ;;
    --group) [ "$#" -ge 2 ] || usage; group="$2"; shift 2 ;;
    --subscription) [ "$#" -ge 2 ] || usage; subscription="$2"; shift 2 ;;
    --data-disk) data_disk=1; shift ;;
    --job) job=1; shift ;;
    --dry-run) dry=1; shift ;;
    *) usage ;;
  esac
done
[ -n "$vhd" ] && [ -n "$run" ] && [ -n "$from" ] || usage
[ -n "$group" ] || group="moruna-node-$(date -u +%Y%m%d%H%M)"

# The guard: nothing runs, not even in a dry run, outside subscription griot-dev and a group of
# this script's own name.
[ "$subscription" = griot-dev ] || refuse "subscription '$subscription' is not griot-dev"
case "$group" in
  moruna-node-*) ;;
  *) refuse "group '$group' is not named moruna-node-*" ;;
esac
suffix="${group#moruna-node-}"
case "$suffix" in
  '' | -* | *- | *[!a-z0-9-]*) refuse "group '$group': after moruna-node- come lowercase letters, digits and inner dashes" ;;
esac
[ "${#group}" -le 64 ] || refuse "group '$group' is longer than 64 characters"
# The storage account's name: 3 to 24 lowercase letters and digits, unique in Azure.
account="mrnnode$(printf '%s' "$suffix" | tr -d -)"
account="${account:0:24}"

case "$from" in
  *[!0-9.]* | .* | *. | *..*) refuse "--from '$from' is not an IPv4 address" ;;
esac
IFS=. read -r a b c d extra <<EOF
$from
EOF
for octet in "$a" "$b" "$c" "$d"; do
  case "$octet" in
    '' | 0?* | *[!0-9]*) refuse "--from '$from' is not an IPv4 address" ;;
  esac
  [ "$octet" -le 255 ] || refuse "--from '$from' is not an IPv4 address"
done
[ -z "$extra" ] || refuse "--from '$from' is not an IPv4 address"

[ -f "$vhd" ] || refuse "no VHD at $vhd"
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
V="$(q "$vhd")"
O="$(q "$openssl")"
S="--subscription $subscription"
G="$group"
A="$account"
KEY="AZURE_STORAGE_KEY=\"\$(cat $R/account.key)\""
ADDRESS="\"\$(cat $R/address)\""
# Base64url without padding, as a JWT's parts are written (2C). The second set of tr begins with
# `_`, not `-`, so that no tr reads it as an option.
B64URL="$O base64 -A | tr '/+' '_-' | tr -d '='"

# Runs one command, as it is written, or in a dry run prints it. Every `az` command must name
# the subscription.
step() {
  case "$1" in
    az\ * | *\ az\ *)
      case "$1" in
        *" --subscription griot-dev"*) ;;
        *) refuse "a command without --subscription griot-dev: $1" ;;
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

# 1. The run's directory, the token key pair and node config (1D).
say "node config"
step "mkdir -m 700 $R"
step "$O genpkey -algorithm ed25519 -out $R/token-key.pem"
step "$O pkey -in $R/token-key.pem -pubout -outform DER | tail -c 32 | base64 > $R/token-key.pub"
step "printf '{\"moruna\":1,\"api\":{\"token_keys\":[\"%s\"]}}\\n' \"\$(cat $R/token-key.pub)\" > $R/node-config.json"

# 2. The group, which must be new: from here on, whatever happens, it is deleted at the end.
say "group $G"
if ! step "[ \"\$(az group exists -n $G $S)\" = false ]"; then
  refuse "group $G exists already, or az could not say: this script deletes only a group it made"
fi

finish() {
  status=$?
  trap - EXIT
  set +e
  say "deleting group $G"
  if [ "$dry" -eq 1 ] || [ -d "$run" ]; then
    step "az vm boot-diagnostics get-boot-log -n moruna-node -g $G $S > $R/console.log"
    if [ "$data_disk" -eq 1 ]; then
      step "tr -d '\\r' < $R/console.log | grep -a -e 'storvsc: ' -e 'storvsc-check: '"
    fi
  fi
  step "az group delete -n $G --yes $S"
  step "az group exists -n $G $S"
  step "az group list $S --query \"[?starts_with(name, 'moruna-node-')].name\" -o tsv"
  step "az network watcher list $S -o table"
  if [ "$dry" -eq 0 ] && [ "$status" -eq 0 ]; then
    printf 'create-to-ready %s s, boot_ms %s' \
      "$(( $(cat "$run/ready.at") - $(cat "$run/created.at") ))" \
      "$(tr -d '\r' < "$run/console.log" | grep -a -o 'moruna: ready .*boot_ms=[0-9]*' | head -1 | sed 's/.*boot_ms=//')"
    if [ -s "$run/job.wall" ]; then
      printf ', job wall_ms %s' "$(cat "$run/job.wall")"
    fi
    printf '\n'
  fi
  exit "$status"
}
trap finish EXIT

step "az group create -n $G -l $location --tags purpose=moruna-node item=1.13 $S -o none"

# 3. The VHD as a page blob, in a storage account that also keeps the boot diagnostics.
say "upload"
step "az storage account create -n $A -g $G -l $location --sku Standard_LRS --kind StorageV2 --min-tls-version TLS1_2 --allow-blob-public-access false $S -o none"
step "(umask 077 && az storage account keys list -n $A -g $G --query '[0].value' -o tsv $S > $R/account.key)"
step "$KEY az storage container create -n vhds --account-name $A --auth-mode key $S -o none"
step "$KEY az storage blob upload -c vhds -n moruna-node.vhd -f $V --type page --account-name $A --auth-mode key $S -o none"
step "$KEY az storage blob url -c vhds -n moruna-node.vhd --account-name $A --auth-mode key $S -o tsv > $R/vhd.url"
step "az storage account show -n $A -g $G --query id -o tsv $S > $R/account.id"

# 4. A Generation 2 managed disk from it, in zone 3.
say "disk"
step "az disk create -n moruna-node-os -g $G -l $location --zone 3 --source \"\$(cat $R/vhd.url)\" --source-storage-account-id \"\$(cat $R/account.id)\" --hyper-v-generation V2 --os-type Linux --sku Standard_LRS $S -o none"
# With --data-disk, an empty data disk in the same zone, which the machine takes at LUN 0.
data=""
if [ "$data_disk" -eq 1 ]; then
  say "data disk"
  step "az disk create -n moruna-node-data -g $G -l $location --zone 3 --size-gb 4 --sku StandardSSD_LRS $S -o none"
  data=" --attach-data-disks moruna-node-data"
fi

# 5. The address, and the network security group: TCP 443 from the operator only.
say "address and network security group"
step "az network public-ip create -n moruna-node-ip -g $G -l $location --zone 3 --sku Standard --allocation-method Static --version IPv4 $S -o none"
step "az network public-ip show -n moruna-node-ip -g $G --query ipAddress -o tsv $S > $R/address"
step "az network nsg create -n moruna-node-nsg -g $G -l $location $S -o none"
step "az network nsg rule create --nsg-name moruna-node-nsg -n https-from-operator -g $G --priority 100 --direction Inbound --access Allow --protocol Tcp --source-address-prefixes $from/32 --source-port-ranges '*' --destination-address-prefixes '*' --destination-port-ranges 443 $S -o none"
step "az network nsg rule list --nsg-name moruna-node-nsg -g $G --include-default -o table $S"

# 6. The machine. The clock starts when Azure has taken the request.
say "machine"
step "az vm create -n moruna-node -g $G -l $location --zone 3 --size Standard_D2s_v4 --attach-os-disk moruna-node-os --os-type linux --security-type Standard --accelerated-networking false --public-ip-address moruna-node-ip --nsg moruna-node-nsg --boot-diagnostics-storage $A --user-data $R/node-config.json$data --no-wait $S -o none && date -u +%s > $R/created.at"

# 7. Ready: the node answers GET /v1/health on 443, which needs no token; at most 10 minutes.
say "waiting for the node to answer on 443"
step "deadline=\$(( \$(date -u +%s) + 600 )); while [ \"\$(date -u +%s)\" -lt \"\$deadline\" ]; do [ \"\$(curl -sk --max-time 5 -o /dev/null -w '%{http_code}' \"https://\$(cat $R/address)/v1/health\")\" = 200 ] && { date -u +%s > $R/ready.at; break; }; sleep 2; done; [ -s $R/ready.at ]"

# 8. The console log: Azure copies the serial port to the storage account a little behind it.
say "console"
step "for _ in \$(seq 30); do az vm boot-diagnostics get-boot-log -n moruna-node -g $G $S > $R/console.log; tr -d '\\r' < $R/console.log | grep -a -q -e 'moruna: ready' -e 'moruna: fatal' && break; sleep 10; done; tr -d '\\r' < $R/console.log | grep -a 'moruna: '"
step "tr -d '\\r' < $R/console.log | grep -a -q 'moruna: ready api='"
step "tr -d '\\r' < $R/console.log | grep -a -o 'moruna: cloud azure instance [^ ]*' | head -1 | cut -d' ' -f5 > $R/instance && [ -s $R/instance ]"

# 9. Check, from this machine: 443 accepts and 22 does not, the machine runs, the served
# certificate is the one on the console, and GET /v1/node answers with a token of the run's key.
say "checks"
step "nc -vz -w 5 $ADDRESS 443"
step "! nc -vz -w 5 $ADDRESS 22"
step "az vm get-instance-view -n moruna-node -g $G --query \"instanceView.statuses[?starts_with(code, 'PowerState/')].displayStatus\" -o tsv $S | grep -q 'VM running'"
step "tr -d '\\r' < $R/console.log | grep -a -o 'moruna: tls sha256=[0-9a-f]*' | head -1 | cut -d= -f2 > $R/console.pin"
step "$O s_client -connect $ADDRESS:443 < /dev/null 2> /dev/null | $O x509 -outform DER | $O dgst -sha256 -r | cut -d' ' -f1 > $R/served.pin && cmp $R/console.pin $R/served.pin"
step "printf '%s' '{\"alg\":\"EdDSA\",\"typ\":\"JWT\"}' | $B64URL > $R/token.header"
step "printf '{\"aud\":\"%s\",\"exp\":%s,\"scope\":[\"observe\"]}' \"\$(cat $R/instance)\" \"\$(( \$(date -u +%s) + 600 ))\" | $B64URL > $R/token.claims"
step "printf '%s.%s' \"\$(cat $R/token.header)\" \"\$(cat $R/token.claims)\" > $R/token.signed"
step "$O pkeyutl -sign -inkey $R/token-key.pem -rawin -in $R/token.signed | $B64URL > $R/token.signature"
step "printf '%s.%s\\n' \"\$(cat $R/token.signed)\" \"\$(cat $R/token.signature)\" > $R/token"
step "curl -sS -k --max-time 10 -H \"Authorization: Bearer \$(cat $R/token)\" \"https://\$(cat $R/address)/v1/node\" > $R/node.json && grep -q '\"cloud\":\"azure\"' $R/node.json && cat $R/node.json"

# 9, with --job: a job through the API (3.2), 10,000,000 rows into a count, with a token for
# submit: validated, submitted, followed over SSE to its exit, and its report checked.
if [ "$job" -eq 1 ]; then
  say "job"
  step "printf '%s' '{\"v1\":{\"moruna_spec\":1,\"source\":{\"kind\":\"rows\",\"rows\":10000000},\"sink\":{\"kind\":\"count\"}}}' > $R/job.json"
  step "printf '{\"aud\":\"%s\",\"exp\":%s,\"scope\":[\"submit\"]}' \"\$(cat $R/instance)\" \"\$(( \$(date -u +%s) + 1800 ))\" | $B64URL > $R/job-token.claims"
  step "printf '%s.%s' \"\$(cat $R/token.header)\" \"\$(cat $R/job-token.claims)\" > $R/job-token.signed"
  step "$O pkeyutl -sign -inkey $R/token-key.pem -rawin -in $R/job-token.signed | $B64URL > $R/job-token.signature"
  step "printf '%s.%s\\n' \"\$(cat $R/job-token.signed)\" \"\$(cat $R/job-token.signature)\" > $R/job-token"
  AUTH="-H \"Authorization: Bearer \$(cat $R/job-token)\""
  JSON="-H 'Content-Type: application/json' --data-binary @$R/job.json"
  step "curl -sS -k --max-time 60 $AUTH $JSON \"https://\$(cat $R/address)/v1/jobs/validate\" > $R/job-validate.json && cat $R/job-validate.json && grep -q '\"ok\":true' $R/job-validate.json"
  step "curl -sS -k --max-time 60 -o $R/job-submit.json -w '%{http_code}' $AUTH $JSON \"https://\$(cat $R/address)/v1/jobs\" > $R/job-submit.code && [ \"\$(cat $R/job-submit.code)\" = 201 ] && grep -o '\"id\":\"[^\"]*\"' $R/job-submit.json | head -1 | cut -d'\"' -f4 > $R/job.id && [ -s $R/job.id ]"
  step "curl -sS -k -N --max-time 600 $AUTH \"https://\$(cat $R/address)/v1/jobs/\$(cat $R/job.id)/events\" > $R/job-events.txt; grep -a '^data: ' $R/job-events.txt | grep -a '\"type\":\"exit\"' | tail -1 | cut -c7- > $R/job-exit.json && cat $R/job-exit.json && grep -q '\"code\":0[,}]' $R/job-exit.json"
  step "curl -sS -k --max-time 60 $AUTH \"https://\$(cat $R/address)/v1/jobs/\$(cat $R/job.id)/report\" > $R/job-report.json && python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))[\"sinks\"][0][\"rows\"])' $R/job-report.json > $R/job.rows && [ \"\$(cat $R/job.rows)\" = 10000000 ]"
  step "python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))[\"wall_ms\"])' $R/job-report.json > $R/job.wall && [ -s $R/job.wall ] && printf 'job: %s rows counted, wall_ms %s\\n' \"\$(cat $R/job.rows)\" \"\$(cat $R/job.wall)\""
fi
