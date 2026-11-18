# The node on Azure

How to put the node on Azure: upload its disk image as a page blob, make a Generation 2 managed
disk of it, create one machine from that disk with node config as user-data and a network security
group that admits port 443 from the operator alone, read its console, check it, and delete
everything (system design `architecture/moruna-os-system.md`, sections 1E and 1F; work item 1.13).
It is written for subscription `griot-dev`, region Central India (`centralindia`), and follows
`kernel/node/OPENSTACK.md`, the same recipe for Servercore.

- **Written 2026-11-18 and not run.** The PM runs it (item 1.13), and corrects it here where it is
  wrong.
- **Every `az` option below was checked without a cloud:** each appears in the help of its command
  in Azure CLI 2.92.0, the version on the project's Mac (`az version`), with no command sent to
  Azure. What Azure answers is item 1.13's to find.
- **Creating anything in Azure needs the PM's go-ahead each time** (1E, 1F).
- **Every `az` command names the subscription,** `--subscription griot-dev`. None sets a default:
  `az account set` is never run, so a command can't reach another subscription by the shell's
  state.
- **Everything is made in one resource group,** `<NAME>`, which is new, and step 10 deletes the
  group and with it all of it.
- **Every command stands alone:** it names its subscription and group, and reads what Azure gave
  earlier steps from files in the run's directory. The steps may be run from separate shells.
- **`kernel/node/tools/azure-up.sh` runs these steps, in order, each the same command.** With
  `--dry-run` it prints them, one to a line, and runs none. It deletes the group when it ends,
  however it ends, and it refuses, before anything runs, any subscription but `griot-dev`, any
  group not named `moruna-node-*`, and a group that already exists. Its tests
  (`kernel/node/tools/test_azure_up.py`) check that this file holds every command it runs. It
  prints each command with the time before running it, so its output is the run's transcript.
  Two options add steps, each marked below: `--data-disk` attaches an empty 4 GiB data disk at
  LUN 0 (steps 4, 6 and 10), and `--job` runs a job through the API once the node is ready
  (step 9); either goes after `--group` below. A run:

  ```
  kernel/node/tools/azure-up.sh --vhd <OUT>/moruna-node.vhd --run <RUN> --from <FROM> \
    --group <NAME> 2>&1 | tee <RUN>.log
  ```

## The values the operator fills in

Each is written `<LIKE_THIS>` below. Replace every one before running a command. Nothing else in
a command changes.

| Value | What it is | Where it comes from |
| --- | --- | --- |
| `<OUT>` | The directory `package.py` wrote, holding `moruna-node.vhd` | The build, below |
| `<RUN>` | A new directory for this run's files: node config, the IDs, the console log, the times. Outside the repository, since a private key and the storage account's key are kept there | The operator chooses it. Step 1 makes it |
| `<NAME>` | The run's resource group: `moruna-node-` and then lowercase letters, digits and inner dashes, for example `moruna-node-202611181530` | The operator chooses it. No group of the subscription has it yet (step 2) |
| `<ACCOUNT>` | The run's storage account: `mrnnode` and `<NAME>`'s part after `moruna-node-` without its dashes, at most 24 characters, for example `mrnnode202611181530` | Made from `<NAME>`. A storage account's name is unique in all of Azure, so a taken one stops the run at step 3 |
| `<FROM>` | The operator's public IPv4 address: the only one the machine admits, on port 443 | Where the operator's traffic leaves for the internet, as a "what is my IP" service shows it |

The machine's own address and its instance ID are read from Azure and from its console into
`<RUN>`, and the commands read them from there.

## What it needs

- **The Azure command line,** `az`, version 2.92.0 or later, logged in (`az login`) with access to
  `griot-dev`. This shows it, and only reads:

  ```
  az account show --subscription griot-dev --query name -o tsv
  ```

- **OpenSSL 3, `curl` and `nc`.** macOS's own `openssl` is LibreSSL, which has no Ed25519; on the
  project's Mac name Homebrew's, `/opt/homebrew/bin/openssl`, wherever `openssl` stands below
  (`azure-up.sh` does, and `MORUNA_OPENSSL` names another).
- **The image, `<OUT>/moruna-node.vhd`:** build it as `kernel/node/HOST.md` says ("Build the
  cloud image"), adding `--formats vhd` to its `package.py` line: a fixed-size VHD, rounded up to a
  whole MiB (1A). Give `--node-memory` the machine's memory or less: `Standard_D2s_v4` has 8 GiB,
  8589934592. The file's size and SHA-256 are in `<OUT>/inputs-and-artifacts.json`, under its
  path.
- **Quota** for one `Standard_D2s_v4` (the DSv4 family) in Central India, zone 3. `griot-dev` has
  no quota for any v5 family there (1F).

## 1. Node config

Make the run's directory, and the token key pair in it:

```
mkdir -m 700 <RUN>
```

```
openssl genpkey -algorithm ed25519 -out <RUN>/token-key.pem
```

```
openssl pkey -in <RUN>/token-key.pem -pubout -outform DER | tail -c 32 | base64 \
  > <RUN>/token-key.pub
```

The first writes the private half, which stays in `<RUN>` and is never committed. The second
writes the public half, the 32 bytes of the key in standard base64: an Ed25519 public key's DER
form is 44 bytes, the last 32 of them the key.

The machine's user-data is node config, version 1 (system design 1D), with that key as its one
token key:

```
printf '{"moruna":1,"api":{"token_keys":["%s"]}}\n' "$(cat <RUN>/token-key.pub)" \
  > <RUN>/node-config.json
```

It carries no secret. A token key is a public key, and with no `tls` the node makes its own key
and certificate at boot (1D).

## 2. The resource group

The group must be new: step 10 deletes it, and with it everything in it. This must succeed,
`az group exists` printing `false`:

```
[ "$(az group exists -n <NAME> --subscription griot-dev)" = false ]
```

```
az group create -n <NAME> -l centralindia --tags purpose=moruna-node item=1.13 \
  --subscription griot-dev -o none
```

From here on, after any step that fails, go to step 10.

## 3. Upload the VHD as a page blob

A managed disk is made from a VHD in a page blob. The storage account also keeps the machine's
boot diagnostics, its console (step 6).

```
az storage account create -n <ACCOUNT> -g <NAME> -l centralindia --sku Standard_LRS \
  --kind StorageV2 --min-tls-version TLS1_2 --allow-blob-public-access false \
  --subscription griot-dev -o none
```

The account's key, kept in `<RUN>` and readable by the operator alone. The commands below pass it
in the environment, `AZURE_STORAGE_KEY`, never on the command line, and it is gone with the
account in step 10.

```
(umask 077 && az storage account keys list -n <ACCOUNT> -g <NAME> --query '[0].value' -o tsv \
  --subscription griot-dev > <RUN>/account.key)
```

```
AZURE_STORAGE_KEY="$(cat <RUN>/account.key)" az storage container create -n vhds \
  --account-name <ACCOUNT> --auth-mode key --subscription griot-dev -o none
```

```
AZURE_STORAGE_KEY="$(cat <RUN>/account.key)" az storage blob upload -c vhds \
  -n moruna-node.vhd -f <OUT>/moruna-node.vhd --type page --account-name <ACCOUNT> \
  --auth-mode key --subscription griot-dev -o none
```

The blob's address and the account's ID, for step 4:

```
AZURE_STORAGE_KEY="$(cat <RUN>/account.key)" az storage blob url -c vhds -n moruna-node.vhd \
  --account-name <ACCOUNT> --auth-mode key --subscription griot-dev -o tsv > <RUN>/vhd.url
```

```
az storage account show -n <ACCOUNT> -g <NAME> --query id -o tsv --subscription griot-dev \
  > <RUN>/account.id
```

## 4. The managed disk, Generation 2

```
az disk create -n moruna-node-os -g <NAME> -l centralindia --zone 3 \
  --source "$(cat <RUN>/vhd.url)" --source-storage-account-id "$(cat <RUN>/account.id)" \
  --hyper-v-generation V2 --os-type Linux --sku Standard_LRS --subscription griot-dev -o none
```

- **Generation 2** is UEFI (1A, 1F). The disk is the machine's OS disk as it is, a specialized
  disk: no image is made from it, since nothing in it is generalized.
- **`--os-type Linux`** is what Azure asks of an OS disk. It changes nothing in the node.
- **Zone 3,** the machine's zone: a managed disk is attached only in its own zone.

**With `--data-disk`:** an empty data disk, 4 GiB of StandardSSD, in the same zone. The machine
takes it at LUN 0 (step 6), and the node serves it as `lun-0` (storvsc's names, 1F): a check
image's `storvsc-check` reads it, writes it, restores it and runs the block suite on it.

```
az disk create -n moruna-node-data -g <NAME> -l centralindia --zone 3 --size-gb 4 \
  --sku StandardSSD_LRS --subscription griot-dev -o none
```

## 5. The address and the network security group

A static public address in zone 3, read into `<RUN>`:

```
az network public-ip create -n moruna-node-ip -g <NAME> -l centralindia --zone 3 --sku Standard \
  --allocation-method Static --version IPv4 --subscription griot-dev -o none
```

```
az network public-ip show -n moruna-node-ip -g <NAME> --query ipAddress -o tsv \
  --subscription griot-dev > <RUN>/address
```

A network security group with one rule added: TCP 443, inbound, from `<FROM>` alone.

```
az network nsg create -n moruna-node-nsg -g <NAME> -l centralindia --subscription griot-dev \
  -o none
```

```
az network nsg rule create --nsg-name moruna-node-nsg -n https-from-operator -g <NAME> \
  --priority 100 --direction Inbound --access Allow --protocol Tcp \
  --source-address-prefixes <FROM>/32 --source-port-ranges '*' \
  --destination-address-prefixes '*' --destination-port-ranges 443 \
  --subscription griot-dev -o none
```

```
az network nsg rule list --nsg-name moruna-node-nsg -g <NAME> --include-default -o table \
  --subscription griot-dev
```

The list must show `https-from-operator`, `Inbound`, `Allow`, `Tcp`, port `443`, from `<FROM>/32`,
and Azure's default rules, which this recipe leaves:

- **inbound,** `AllowVnetInBound` and `AllowAzureLoadBalancerInBound`, which admit nothing from
  the internet, and `DenyAllInBound` after them, which refuses the rest;
- **outbound,** `AllowVnetOutBound`, `AllowInternetOutBound` and `DenyAllOutBound`. The node itself
  denies outbound traffic that is not granted (1C), and its only grants are the instance data
  (`169.254.169.254`) and the WireServer (`168.63.129.16`, the "ready" report; 1C, 1F).

## 6. The machine

```
az vm create -n moruna-node -g <NAME> -l centralindia --zone 3 --size Standard_D2s_v4 \
  --attach-os-disk moruna-node-os --os-type linux --security-type Standard \
  --accelerated-networking false --public-ip-address moruna-node-ip --nsg moruna-node-nsg \
  --boot-diagnostics-storage <ACCOUNT> --user-data <RUN>/node-config.json --no-wait \
  --subscription griot-dev -o none && date -u +%s > <RUN>/created.at
```

- **`Standard_D2s_v4` in zone 3,** the first bring-up's size (1F). Generation 2 comes from the
  disk.
- **Security type `Standard`:** Azure's default, Trusted Launch, has Secure Boot, which refuses the
  node's unsigned GRUB (1F).
- **Accelerated networking off:** the node drives the synthetic network card (netvsc), and no
  virtual function is offered (1F).
- **The address and the group of step 5.** `az` makes the virtual network and the network card,
  with the group on the card.
- **Boot diagnostics into the account of step 3,** on from the first power-on, so the console log
  holds the whole boot and the machine is never restarted (item 1.7b turned them on after the
  machine was made, and restarted it).
- **Node config as user-data:** Azure serves it in the instance data's `compute.userData`, in
  base64, where `platform` reads it (1D).
- **No key, no password, no administrator:** an attached OS disk takes no OS profile, and nobody
  logs in.
- **`--no-wait`:** the command returns when Azure has taken the request, and `created.at` is that
  time, for step 7.

**With `--data-disk`,** the same command attaches the data disk of step 4. The first data disk
named takes LUN 0:

```
az vm create -n moruna-node -g <NAME> -l centralindia --zone 3 --size Standard_D2s_v4 \
  --attach-os-disk moruna-node-os --os-type linux --security-type Standard \
  --accelerated-networking false --public-ip-address moruna-node-ip --nsg moruna-node-nsg \
  --boot-diagnostics-storage <ACCOUNT> --user-data <RUN>/node-config.json \
  --attach-data-disks moruna-node-data --no-wait --subscription griot-dev -o none \
  && date -u +%s > <RUN>/created.at
```

## 7. Ready

The node is ready when it answers on 443. `GET /v1/health` needs no token (2A). Ask every two
seconds, for at most 10 minutes:

```
deadline=$(( $(date -u +%s) + 600 )); while [ "$(date -u +%s)" -lt "$deadline" ]; do \
  [ "$(curl -sk --max-time 5 -o /dev/null -w '%{http_code}' \
  "https://$(cat <RUN>/address)/v1/health")" = 200 ] \
  && { date -u +%s > <RUN>/ready.at; break; }; sleep 2; done; [ -s <RUN>/ready.at ]
```

- **Create-to-ready** (1E) is `ready.at` less `created.at`, in seconds, to within the 2 seconds
  between two tries. It counts Azure's own work, the firmware and GRUB, and the node's boot.
- If it fails, read the console (step 8), then go to step 10 and report.

## 8. The console log

The node writes its boot lines to the first serial port, which Azure copies into the boot
diagnostics' storage a little behind the machine (1B). Read it until it holds the node's `ready`
or `fatal` line, at most 5 minutes, then show the node's lines:

```
for _ in $(seq 30); do az vm boot-diagnostics get-boot-log -n moruna-node -g <NAME> \
  --subscription griot-dev > <RUN>/console.log; tr -d '\r' < <RUN>/console.log \
  | grep -a -q -e 'moruna: ready' -e 'moruna: fatal' && break; sleep 10; done; \
  tr -d '\r' < <RUN>/console.log | grep -a 'moruna: '
```

```
tr -d '\r' < <RUN>/console.log | grep -a -q 'moruna: ready api='
```

```
tr -d '\r' < <RUN>/console.log | grep -a -o 'moruna: cloud azure instance [^ ]*' | head -1 \
  | cut -d' ' -f5 > <RUN>/instance && [ -s <RUN>/instance ]
```

- **Success** is 1B's boot lines in order, the `cloud` line naming `azure` (its instance ID goes
  to `<RUN>/instance`, a token's audience, step 9), and the last line
  `moruna: ready api=<address>:443 boot_ms=<n>`, with the machine's private address: the public
  one is Azure's, outside the machine.
- **`boot_ms`** is on the `ready` line (1B).
- **`moruna: fatal <stage>: <reason>`** is the node refusing to run. It then powers off by ACPI S5,
  and the machine shows as stopped.
- The log starts with the firmware's own output, which holds terminal escapes, and its lines end
  in a carriage return, which `tr` takes out.

## 9. Check

These are 1E's success, from the operator's machine. 443 accepts; 22 does not, since the group
drops it, so `nc` times out:

```
nc -vz -w 5 "$(cat <RUN>/address)" 443
```

```
! nc -vz -w 5 "$(cat <RUN>/address)" 22
```

The machine runs, with nobody having logged in:

```
az vm get-instance-view -n moruna-node -g <NAME> \
  --query "instanceView.statuses[?starts_with(code, 'PowerState/')].displayStatus" -o tsv \
  --subscription griot-dev | grep -q 'VM running'
```

The certificate served on 443 is the one whose fingerprint the console printed, `moruna: tls
sha256=<fingerprint>`, the SHA-256 of its DER (2B):

```
tr -d '\r' < <RUN>/console.log | grep -a -o 'moruna: tls sha256=[0-9a-f]*' | head -1 \
  | cut -d= -f2 > <RUN>/console.pin
```

```
openssl s_client -connect "$(cat <RUN>/address)":443 < /dev/null 2> /dev/null \
  | openssl x509 -outform DER | openssl dgst -sha256 -r | cut -d' ' -f1 > <RUN>/served.pin \
  && cmp <RUN>/console.pin <RUN>/served.pin
```

A token for `observe`, signed with the run's key (2C): an EdDSA JWT whose audience is the
instance ID of step 8, expiring in 10 minutes. Its three parts are base64url without padding:

```
printf '%s' '{"alg":"EdDSA","typ":"JWT"}' | openssl base64 -A | tr '/+' '_-' | tr -d '=' \
  > <RUN>/token.header
```

```
printf '{"aud":"%s","exp":%s,"scope":["observe"]}' "$(cat <RUN>/instance)" \
  "$(( $(date -u +%s) + 600 ))" | openssl base64 -A | tr '/+' '_-' | tr -d '=' \
  > <RUN>/token.claims
```

```
printf '%s.%s' "$(cat <RUN>/token.header)" "$(cat <RUN>/token.claims)" > <RUN>/token.signed
```

```
openssl pkeyutl -sign -inkey <RUN>/token-key.pem -rawin -in <RUN>/token.signed \
  | openssl base64 -A | tr '/+' '_-' | tr -d '=' > <RUN>/token.signature
```

```
printf '%s.%s\n' "$(cat <RUN>/token.signed)" "$(cat <RUN>/token.signature)" > <RUN>/token
```

`GET /v1/node` over HTTPS answers the node document, its `cloud` `azure`. `-k` stands in for the
pin, which the check above has already made:

```
curl -sS -k --max-time 10 -H "Authorization: Bearer $(cat <RUN>/token)" \
  "https://$(cat <RUN>/address)/v1/node" > <RUN>/node.json \
  && grep -q '"cloud":"azure"' <RUN>/node.json && cat <RUN>/node.json
```

### With `--job`: a job

A job through the API (3.2's kinds, 3B): a `rows` source of 10,000,000 rows into a `count` sink.
It needs a token for `submit`, which covers `observe` (2C); it is made as the one above, with
that scope and 30 minutes:

```
printf '%s' '{"v1":{"moruna_spec":1,"source":{"kind":"rows","rows":10000000},"sink":{"kind":"count"}}}' \
  > <RUN>/job.json
```

```
printf '{"aud":"%s","exp":%s,"scope":["submit"]}' "$(cat <RUN>/instance)" \
  "$(( $(date -u +%s) + 1800 ))" | openssl base64 -A | tr '/+' '_-' | tr -d '=' \
  > <RUN>/job-token.claims
```

```
printf '%s.%s' "$(cat <RUN>/token.header)" "$(cat <RUN>/job-token.claims)" \
  > <RUN>/job-token.signed
```

```
openssl pkeyutl -sign -inkey <RUN>/token-key.pem -rawin -in <RUN>/job-token.signed \
  | openssl base64 -A | tr '/+' '_-' | tr -d '=' > <RUN>/job-token.signature
```

```
printf '%s.%s\n' "$(cat <RUN>/job-token.signed)" "$(cat <RUN>/job-token.signature)" \
  > <RUN>/job-token
```

The engine checks the document first (`POST /v1/jobs/validate`, `{"ok":true}`), then runs it
(`POST /v1/jobs`, `201` with the run's `id`):

```
curl -sS -k --max-time 60 -H "Authorization: Bearer $(cat <RUN>/job-token)" \
  -H 'Content-Type: application/json' --data-binary @<RUN>/job.json \
  "https://$(cat <RUN>/address)/v1/jobs/validate" > <RUN>/job-validate.json \
  && cat <RUN>/job-validate.json && grep -q '"ok":true' <RUN>/job-validate.json
```

```
curl -sS -k --max-time 60 -o <RUN>/job-submit.json -w '%{http_code}' \
  -H "Authorization: Bearer $(cat <RUN>/job-token)" -H 'Content-Type: application/json' \
  --data-binary @<RUN>/job.json "https://$(cat <RUN>/address)/v1/jobs" > <RUN>/job-submit.code \
  && [ "$(cat <RUN>/job-submit.code)" = 201 ] \
  && grep -o '"id":"[^"]*"' <RUN>/job-submit.json | head -1 | cut -d'"' -f4 > <RUN>/job.id \
  && [ -s <RUN>/job.id ]
```

Its events over Server-Sent Events, kept in `<RUN>`, until the node closes the stream after the
`exit` event (at most 10 minutes); the exit code must be 0:

```
curl -sS -k -N --max-time 600 -H "Authorization: Bearer $(cat <RUN>/job-token)" \
  "https://$(cat <RUN>/address)/v1/jobs/$(cat <RUN>/job.id)/events" > <RUN>/job-events.txt; \
  grep -a '^data: ' <RUN>/job-events.txt | grep -a '"type":"exit"' | tail -1 | cut -c7- \
  > <RUN>/job-exit.json && cat <RUN>/job-exit.json && grep -q '"code":0[,}]' <RUN>/job-exit.json
```

The report: its one sink wrote 10,000,000 rows, and its `wall_s` is the job's wall time:

```
curl -sS -k --max-time 60 -H "Authorization: Bearer $(cat <RUN>/job-token)" \
  "https://$(cat <RUN>/address)/v1/jobs/$(cat <RUN>/job.id)/report" > <RUN>/job-report.json \
  && grep -o '"sinks":\[{[^]]*' <RUN>/job-report.json | grep -o '"rows":[0-9]*' | head -1 \
  | cut -d: -f2 > <RUN>/job.rows && [ "$(cat <RUN>/job.rows)" = 10000000 ]
```

```
grep -o '"wall_s":[0-9.eE+-]*' <RUN>/job-report.json | head -1 | cut -d: -f2 > <RUN>/job.wall \
  && [ -s <RUN>/job.wall ] \
  && printf 'job: %s rows counted, wall_s %s\n' "$(cat <RUN>/job.rows)" "$(cat <RUN>/job.wall)"
```

## 10. Delete everything

Read the console one last time, then delete the group. That deletes everything this recipe made,
all of it in the group: the machine, its OS disk, its network card and virtual network, the
public address, the security group and the storage account with the VHD, the boot diagnostics and
the key. `--yes` asks no question; the command returns when Azure has deleted it all.

```
az vm boot-diagnostics get-boot-log -n moruna-node -g <NAME> --subscription griot-dev \
  > <RUN>/console.log
```

**With `--data-disk`,** its storvsc lines: the disks served, and `storvsc-check`'s steps on
`lun-0` with how many passed, by now complete:

```
tr -d '\r' < <RUN>/console.log | grep -a -e 'storvsc: ' -e 'storvsc-check: '
```

```
az group delete -n <NAME> --yes --subscription griot-dev
```

Show that it is gone. The first must print `false`, the second must not name `<NAME>`:

```
az group exists -n <NAME> --subscription griot-dev
```

```
az group list --subscription griot-dev --query "[?starts_with(name, 'moruna-node-')].name" -o tsv
```

The last lists the subscription's network watchers. Azure enables Network Watcher in a region,
in its own group `NetworkWatcherRG`, when a subscription's first virtual network there is made.
That is the subscription's, shared by every virtual network in the region, and is left as it is;
the list shows whether this run's virtual network was the first.

```
az network watcher list --subscription griot-dev -o table
```

Keep `<RUN>` with the report: it holds the console log and the two times. Its `token-key.pem` is
the run's private key, and is never committed; its `account.key` opened an account that no longer
exists.
