#!/usr/bin/env bash
# The proof's data in Azure Blob, for a node outside Azure (work item 3.8b: the proof on
# Servercore, which reads it over the internet through `moruna-net`, system design 3D). It makes
# a storage account with two private containers in subscription griot-dev, fills them inside
# Azure, and leaves them for openstack-up.sh --proof until --delete:
#
#   kernel/node/tools/nyc-blob.sh --run RUN --tasks DIR [--group moruna-nyc-NAME]
#       [--subscription griot-dev] [--dry-run]
#   kernel/node/tools/nyc-blob.sh --delete --run RUN [--subscription griot-dev] [--dry-run]
#
# - `data` holds the twelve normalised months of 2024 under `nyc/`, made in Azure, with no upload
#   from the Mac (about 0.3 MB/s): one small Linux machine (a Standard_D2s_v4 in zone 3, Ubuntu
#   24.04, no inbound rule at all) runs 6.2b's bench/node/engines/data-linux.sh and dataprep.py
#   once, by a managed run command (v2) whose one protected parameter is the account key
#   (bench/node/azure.py, run_command_create). They fetch each month from NYC TLC's public
#   address, normalise it, check it against bench/node/plans/nyc-2024.json's SHA-256 and upload
#   it (kernel/node/tools/nyc-data.sh, which carries the code in its script). Once its `DATA`
#   line says ok and the container holds the twelve, the machine is deleted with everything
#   made for it. The jobs read `nyc/` and write `proof/<stamp>/` there.
# - `functions` holds the four gate tasks of --tasks (gate-score, gate-occupancy, gate-enrich,
#   gate-audit; about 6 MB from the Mac), which the node fetches by address (3E).
#
# Then two container SASes, made on this machine from the account key, https only, expiring in
# 6 hours: `data`'s reads, adds, creates, writes and lists (racwl), and `functions`' reads (r),
# so no function's address carries a write SAS. They are written, never shown, to RUN/blob, mode
# 600, beside the account and the two containers' names, for openstack-up.sh --proof --blob:
#
#   account=<account>
#   container=data
#   functions=functions
#   data_sas=<SAS>
#   functions_sas=<SAS>
#   expires=<yyyy-mm-ddTHH:MMZ>
#
# The account key stays in RUN/account.key (mode 600); it leaves this machine only as the run
# command's protected parameter, and the node and proof.py are given the SASes alone. The last
# line printed names the account, the containers, the expiry and RUN/blob.
#
# - --run: a new directory for the run's files, made with mode 700 (the key, the SASes, the
#   machine's throwaway SSH key, which no rule lets anyone use, and the group's name for
#   --delete). Keep it outside the repository.
# - --group: the resource group, `moruna-nyc-<yyyymmddHHMM>` when not given; everything is made
#   in it. A run that fails part-way deletes it, and all in it. One that succeeds leaves it, with
#   the storage account alone in it, until --delete.
# - --delete: deletes the group RUN/group names, which this script made, and shows it gone; the
#   SASes are void with the account.
# - --dry-run: prints every command, one to a line, and runs none of them; last, the deletion a
#   run that fails runs.
#
# A guard refuses, before anything runs, any subscription but griot-dev, any group not named
# `moruna-nyc-*`, and a group that already exists. Every `az` command names the subscription with
# --subscription and acts only in the run's group; none sets a default. The run command, the one
# long call, is bounded by perl's alarm (4,000 s; its own timeout is 3,600 s).
#
# It exits 0 when the data and the tasks are in their containers and RUN/blob is written (with
# --delete: when the group is gone); 1 on any failure, after the group is deleted; 2 on a refusal.
set -euo pipefail

subscription=griot-dev
location=centralindia
group=""
run=""
tasks=""
delete=0
dry=0
# The repository this script is in, whose bench code the machine runs.
repo="$(cd "$(dirname "$0")/../../.." && pwd)"
jobs="score occupancy enrich audit"

refuse() {
  echo "nyc-blob.sh: refused: $*" >&2
  exit 2
}

usage() {
  echo "usage: nyc-blob.sh --run RUN --tasks DIR [--group moruna-nyc-NAME] [--subscription griot-dev] [--dry-run]" >&2
  echo "       nyc-blob.sh --delete --run RUN [--subscription griot-dev] [--dry-run]" >&2
  exit 2
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --run) [ "$#" -ge 2 ] || usage; run="$2"; shift 2 ;;
    --tasks) [ "$#" -ge 2 ] || usage; tasks="$2"; shift 2 ;;
    --group) [ "$#" -ge 2 ] || usage; group="$2"; shift 2 ;;
    --subscription) [ "$#" -ge 2 ] || usage; subscription="$2"; shift 2 ;;
    --delete) delete=1; shift ;;
    --dry-run) dry=1; shift ;;
    *) usage ;;
  esac
done
[ -n "$run" ] || usage

# The guard: nothing runs, not even in a dry run, outside subscription griot-dev and a group of
# this script's own name.
[ "$subscription" = griot-dev ] || refuse "subscription '$subscription' is not griot-dev"
if [ "$delete" -eq 1 ]; then
  [ -z "$tasks$group" ] || refuse "--delete takes the group RUN/group names, and no --tasks or --group"
  [ -f "$run/group" ] || refuse "no group named in $run/group: --delete deletes only a group this script made"
  group="$(head -1 "$run/group")"
else
  [ -n "$tasks" ] || usage
  [ -n "$group" ] || group="moruna-nyc-$(date -u +%Y%m%d%H%M)"
fi
case "$group" in
  moruna-nyc-*) ;;
  *) refuse "group '$group' is not named moruna-nyc-*" ;;
esac
suffix="${group#moruna-nyc-}"
case "$suffix" in
  '' | -* | *- | *[!a-z0-9-]*) refuse "group '$group': after moruna-nyc- come lowercase letters, digits and inner dashes" ;;
esac
[ "${#group}" -le 64 ] || refuse "group '$group' is longer than 64 characters"
# The storage account's name: 3 to 24 lowercase letters and digits, unique in Azure.
account="mrnnyc$(printf '%s' "$suffix" | tr -d -)"
account="${account:0:24}"

if [ "$delete" -eq 0 ]; then
  for j in $jobs; do
    [ -f "$tasks/gate-$j" ] || refuse "no gate task at $tasks/gate-$j"
  done
  if [ "$dry" -eq 0 ] && [ -e "$run" ]; then
    refuse "$run exists: the run's directory must be new"
  fi
fi

# A word as the shell reads it: as it is when it is plain, else in single quotes.
q() {
  case "$1" in
    '' | *[!A-Za-z0-9_./:=@,+%-]*) printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")" ;;
    *) printf '%s' "$1" ;;
  esac
}

R="$(q "$run")"
T="$(q "$tasks")"
REPO="$(q "$repo")"
S="--subscription $subscription"
G="$group"
A="$account"
KEY="AZURE_STORAGE_KEY=\"\$(cat $R/account.key)\""
ON="--account-name $A --auth-mode key $S"
VM=nyc-data
# The code the machine runs, carried in its script.
CODE="bench/node/engines/data-linux.sh bench/node/engines/dataprep.py bench/node/engines/store.py bench/vm/normalize.py bench/node/plans/nyc-2024.json"

# Runs one command, as it is written, or in a dry run prints it. Every `az` command must name
# subscription griot-dev, and every group it names must be the run's.
step() {
  case "$1" in
    az\ * | *\ az\ * | *\(az\ *)
      case "$1" in
        *" --subscription griot-dev"*) ;;
        *) refuse "a command without --subscription griot-dev: $1" ;;
      esac ;;
  esac
  local rest="$1" named='(-g|--resource-group|az group (create|delete|exists) -n) ([^ ]+)(.*)'
  while [[ "$rest" =~ $named ]]; do
    [ "${BASH_REMATCH[3]}" = "$G" ] || refuse "a command on a group not the run's: $1"
    rest="${BASH_REMATCH[4]}"
  done
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

gone() {
  step "az group delete -n $G --yes $S"
  step "[ \"\$(az group exists -n $G $S)\" = false ]"
  step "az group list $S --query \"[?starts_with(name, 'moruna-nyc-')].name\" -o tsv"
}

if [ "$delete" -eq 1 ]; then
  say "deleting group $G"
  gone
  [ "$dry" -eq 1 ] || echo "nyc-blob: group $G is gone, with account $A, its data, its tasks and its SASes"
  exit 0
fi

# 1. The run's directory, the group's name for --delete, and the machine's SSH key, which no
# rule lets anyone use (az makes no Linux machine without one).
say "the run's directory"
step "mkdir -m 700 $R"
step "printf '%s\\n' $G > $R/group"
step "ssh-keygen -q -t ed25519 -N '' -C $VM -f $R/vm-key"

# 2. The group, which must be new: from here on, a failure deletes it.
say "group $G"
if ! step "[ \"\$(az group exists -n $G $S)\" = false ]"; then
  refuse "group $G exists already, or az could not say: this script deletes only a group it made"
fi

finish() {
  status=$?
  trap - EXIT
  set +e
  if [ "$status" -ne 0 ] || [ "$dry" -eq 1 ]; then
    [ "$status" -eq 0 ] || status=1
    say "the run failed: deleting group $G"
    gone
  fi
  exit "$status"
}
trap finish EXIT

step "az group create -n $G -l $location --tags purpose=moruna-nyc item=3.8b $S -o none"

# 3. The storage account, its key, and its two private containers; the tasks into `functions`.
say "storage account $A"
step "az storage account create -n $A -g $G -l $location --sku Standard_LRS --kind StorageV2 --min-tls-version TLS1_2 --allow-blob-public-access false $S -o none"
step "(umask 077 && az storage account keys list -n $A -g $G --query '[0].value' -o tsv $S > $R/account.key)"
step "$KEY az storage container create -n data $ON -o none"
step "$KEY az storage container create -n functions $ON -o none"
for j in $jobs; do
  step "$KEY az storage blob upload -c functions -n gate-$j -f $T/gate-$j $ON -o none"
done

# 4. The machine's script: the code, the store's names, then nyc-data.sh.
say "the machine's script"
step "(cd $REPO && COPYFILE_DISABLE=1 tar --format ustar -czf - $CODE) | base64 | tr -d '\\n' > $R/code.b64"
step "{ printf \"#!/bin/bash\\nCODE='%s'\\nexport BENCH_ACCOUNT=%s BENCH_CONTAINER=data\\n\" \"\$(cat $R/code.b64)\" $A; cat $REPO/kernel/node/tools/nyc-data.sh; } > $R/data.sh"

# 5. The machine: a network security group with no rule of its own, so nothing comes in, and a
# public address, since a new virtual network gives a machine no way out without one.
say "machine $VM"
step "az network nsg create -n $VM-nsg -g $G -l $location $S -o none"
step "az vm create -n $VM -g $G -l $location --zone 3 --image Canonical:ubuntu-24_04-lts:server:latest --size Standard_D2s_v4 --security-type Standard --accelerated-networking false --public-ip-sku Standard --public-ip-address $VM-ip --vnet-name $VM-vnet --subnet $VM-subnet --nsg $VM-nsg --nsg-rule NONE --os-disk-name $VM-os --storage-sku StandardSSD_LRS --os-disk-delete-option Delete --nic-delete-option Delete --admin-username nyc --ssh-key-values $R/vm-key.pub $S -o none"

# 6. The data, made on the machine: one managed run command, the key its protected parameter.
say "the data, on the machine (about 20 minutes)"
step "perl -e 'alarm shift; exec @ARGV; exit 127' 4000 az vm run-command create --run-command-name $VM --vm-name $VM -g $G --location $location --script \"\$(cat $R/data.sh)\" --async-execution false --timeout-in-seconds 3600 --protected-parameters BENCH_KEY=@$R/account.key $S -o none"
step "az vm run-command show --run-command-name $VM --vm-name $VM -g $G --instance-view --query instanceView -o json $S > $R/data-view.json"
step "python3 -I -c 'import json, sys; v = json.load(open(sys.argv[1])); out = v.get(\"output\") or \"\"; print(out[-2000:]); print(\"error:\", (v.get(\"error\") or \"\")[-1000:]); print(\"state\", v.get(\"executionState\"), \"exit\", v.get(\"exitCode\")); data = [l[5:] for l in out.splitlines() if l.startswith(\"DATA \")]; d = json.loads(data[-1]) if data else {}; open(sys.argv[2], \"w\").write(json.dumps(d)); sys.exit(0 if v.get(\"exitCode\") == 0 and d.get(\"ok\") is True else 1)' $R/data-view.json $R/data.json"
step "$KEY az storage blob list -c data --prefix nyc/ --query '[].name' -o tsv $ON > $R/data.list && [ \"\$(grep -c -x 'nyc/yellow_tripdata_2024-[01][0-9]\\.parquet' $R/data.list)\" = 12 ]"

# 7. The machine deleted, with its disk, its network card, its address, its network and its
# group; the storage account is all the group keeps.
say "deleting machine $VM"
step "az vm delete -n $VM -g $G --yes $S -o none"
step "az network public-ip delete -n $VM-ip -g $G $S"
step "az network vnet delete -n $VM-vnet -g $G $S"
step "az network nsg delete -n $VM-nsg -g $G $S"
step "for _ in \$(seq 12); do az resource list -g $G --query '[].type' -o tsv $S > $R/left; [ \"\$(cat $R/left)\" = Microsoft.Storage/storageAccounts ] && break; sleep 10; done; cat $R/left; [ \"\$(cat $R/left)\" = Microsoft.Storage/storageAccounts ]"

# 8. The two SASes, made here from the key, and RUN/blob for openstack-up.sh --proof.
say "the SASes"
step "python3 -I -c 'import datetime as d; print((d.datetime.now(d.timezone.utc) + d.timedelta(hours=6)).strftime(\"%Y-%m-%dT%H:%MZ\"))' > $R/expires"
step "(umask 077 && $KEY az storage container generate-sas -n data --permissions racwl --expiry \"\$(cat $R/expires)\" --https-only $ON -o tsv > $R/data.sas)"
step "(umask 077 && $KEY az storage container generate-sas -n functions --permissions r --expiry \"\$(cat $R/expires)\" --https-only $ON -o tsv > $R/functions.sas)"
step "(umask 077 && printf 'account=%s\\ncontainer=data\\nfunctions=functions\\ndata_sas=%s\\nfunctions_sas=%s\\nexpires=%s\\n' $A \"\$(cat $R/data.sas)\" \"\$(cat $R/functions.sas)\" \"\$(cat $R/expires)\" > $R/blob)"
step "[ -s $R/data.sas ] && [ -s $R/functions.sas ]"
if [ "$dry" -eq 0 ]; then
  echo "nyc-blob: account $A, container data (nyc/, the twelve months) and functions (the four tasks), SASes until $(cat "$run/expires") in $run/blob (mode 600); delete with: nyc-blob.sh --delete --run $run"
fi
