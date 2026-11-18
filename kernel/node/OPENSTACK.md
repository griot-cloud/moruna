# The node on OpenStack

How to put the node on Servercore: upload its image, make a private network with a router to the
external network, create one server on it, from the image or from a volume made from the image,
with node config as user-data and a security group for port 443 only, give it a floating IP, read
its console, check it, and delete everything (system design `architecture/moruna-os-system.md`,
section 1E; work items 1.5 and 1.5b). The check ends with the API's proof from outside the cloud
(section 2, work item 2.6), and a run may also run the NYC taxi proof on the node (section 3,
work item 3.8b). It is written for Servercore, cloud `servercore` in
`~/.config/openstack/clouds.yaml`.

- **Corrected 2026-11-18 (work item 1.5b) from item 1.6's runs on Servercore `ru-3`** (the PM's
  scripts and logs, `~/moruna-data/servercore/run-1` to `run-6`). What they found wrong in the
  first recipe of item 1.5:
  1. No server goes directly on `external-network`: Servercore aborts its build with "Failed to
     allocate the network(s)" (run 1). The server goes on a private network of the run's own,
     whose router has `external-network` as its gateway, and gets a floating IP (steps 5 and 8).
  2. The flavor's RAM must be at least the image's `--node-memory`, and the `cloud` profile's
     fixed accounts are 832 MiB, so the smallest flavor that works has 2 GiB (step 6).
  3. A boot from a volume needs a flavor with no disk: Servercore answers "It is restricted to
     create volume-backed server with non zero disk flavor" (step 6b).
  4. Volume types are per zone, as `basic.ru-3b`: the volume names its type and its zone, and the
     server is made in the same zone (step 6b).
  5. A call can hang: `router unset --external-gateway` once hung for 38 minutes. Every
     `openstack` call here is bounded, and step 11 deletes in the reverse of the order things
     were made.
  6. A node that stops shows `SHUTOFF` within about 30 s. The console is read every few seconds
     from the create on, and its errors are kept (steps 7 and 9).
- **Every `openstack` line below was checked without a cloud:** parsed by the argument parsers of
  python-openstackclient 10.5.0, the version on the project's Mac (`openstack --version` shows
  it), with the `openstack` program itself never run. The commands are those of item 1.6's runs
  where those worked.
- **Creating anything on a cloud needs the PM's go-ahead each time** (1E).
- **Every command stands alone:** it names its cloud and region, and reads the IDs of what was
  made from files in the run's directory. Nothing depends on a shell variable or on the shell it
  was typed in, so the steps may be run from separate shells.
- **Every `openstack` call is bounded** by `perl -e 'alarm shift; exec @ARGV; exit 127' 120`
  before it: the alarm outlives the `exec`, so `openstack` is ended after 120 s, and the call
  fails with status 142. The image's upload has 600 s. A call that cannot start at all fails
  with 127.
- **`kernel/node/tools/openstack-up.sh` runs these steps, in order, each the same command.** With
  `--dry-run` it prints them, one to a line, and runs none. It refuses, before anything runs, any
  cloud but `servercore`, any name not `moruna-node-*`, and a name the project already has
  (step 2). Whatever happens, it deletes everything it made when it ends (step 11), and fails if
  anything of the name is left. Its tests (`kernel/node/tools/test_openstack_up.py`) check that
  this file holds every command it runs. It prints each command with the time before running it,
  so its output is the run's transcript. `--boot` chooses step 6a or 6b. A run that boots from
  the image:

  ```
  kernel/node/tools/openstack-up.sh --qcow2 <OUT>/moruna-node.qcow2 --run <RUN> \
    --region <REGION> --boot image --name <NAME> 2>&1 | tee <RUN>.log
  ```

  One that boots from a volume:

  ```
  kernel/node/tools/openstack-up.sh --qcow2 <OUT>/moruna-node.qcow2 --run <RUN> \
    --region <REGION> --boot volume --zone <ZONE> --name <NAME> 2>&1 | tee <RUN>.log
  ```

- **Every run ends with the API's checks** from outside the cloud (work item 2.6; step 10), and
  fails if any of them fails.
- **With `--proof`** (work item 3.8b), the run also runs the proof on the node: the four NYC taxi
  jobs through its API, on a month and on the year, from and to Azure Blob over the internet
  (step 10). It boots from the image only, on the proof's flavor (step 6a), and needs the data in
  Blob first, made by `kernel/node/tools/nyc-blob.sh` (below, "The proof's data in Azure Blob").
  The three commands, in order: the data, the run, and the data deleted once the run is done:

  ```
  kernel/node/tools/nyc-blob.sh --run <BLOB_RUN> --tasks <TASKS> 2>&1 | tee <BLOB_RUN>.log
  ```

  ```
  kernel/node/tools/openstack-up.sh --qcow2 <OUT>/moruna-node.qcow2 --run <RUN> \
    --region <REGION> --boot image --name <NAME> --proof --blob <BLOB_RUN>/blob \
    --tasks <TASKS> --nyc <NYC> --gate-canon <GATE_CANON> 2>&1 | tee <RUN>.log
  ```

  ```
  kernel/node/tools/nyc-blob.sh --delete --run <BLOB_RUN> 2>&1 | tee -a <BLOB_RUN>.log
  ```

## The values the operator fills in

Each is written `<LIKE_THIS>` below. Replace every one before running a command. Nothing else in
a command changes.

| Value | What it is | Where it comes from |
| --- | --- | --- |
| `<REGION>` | The Servercore region everything is made in, for example `ru-3` | The project's account. `openstack --os-cloud servercore catalog list` names the regions that have a `compute` endpoint |
| `<ZONE>` | Step 6b only: the availability zone the volume and the server are both made in, for example `ru-3b`. The volume's type is `basic.<ZONE>` | `volume type list` names the types, one per zone; `availability zone list --volume` and `--compute` name the zones |
| `<OUT>` | The directory `package.py` wrote, holding `moruna-node.qcow2` | The build, below |
| `<RUN>` | A new directory for this run's files: node config, the IDs, the console log and its errors, the times. Outside the repository, since a private key is kept there | The operator chooses it. Step 1 makes it |
| `<NAME>` | One name for everything the run makes, its image, security group, network, subnet, router, volume and server: `moruna-node-` and then lowercase letters, digits and inner dashes, for example `moruna-node-202611181530` | The operator chooses it. The project has nothing of that name yet (step 2) |
| `<REPO>` | The repository's root, whose `kernel/node/tools/api-checks.py` and, with `--proof`, `bench/node/proof.py` run | The checkout `openstack-up.sh` is in; it names its own |
| `<BLOB_RUN>` | With `--proof`: `nyc-blob.sh`'s run directory, whose `blob` file (mode 600) names the storage account and its two containers and holds their SASes | The operator chooses it, new, outside the repository; `nyc-blob.sh` makes it |
| `<ACCOUNT>` | With `--proof`: the storage account `nyc-blob.sh` made, `mrnnyc` and its group's stamp | The `account=` line of `<BLOB_RUN>/blob`; `openstack-up.sh` reads it there |
| `<TASKS>` | With `--proof`: the gate tasks built for the core (`images/gate/tasks/build.sh`'s `core/bin`), `gate-score`, `gate-occupancy`, `gate-enrich` and `gate-audit`: the ones `nyc-blob.sh` uploaded | The gate's build |
| `<NYC>` | With `--proof`: the directory of 0.6.6's references, `reference-JOB-PERIOD.json` | `~/moruna-data/nyc` on the project's Mac |
| `<GATE_CANON>` | With `--proof`: `gate-canon` built for the operator's machine, which checks the year's outputs | `images/gate`'s build |

The server's floating IP and its ID are read from Servercore into `<RUN>`, and the commands read
them from there.

## What it needs

- **The `openstack` command line** with cloud `servercore` configured.
- **OpenSSL 3, `nc` and `perl`.** macOS's own `openssl` is LibreSSL, which has no Ed25519; on the
  project's Mac name Homebrew's, `/opt/homebrew/bin/openssl`, wherever `openssl` stands below
  (`openstack-up.sh` does, and `MORUNA_OPENSSL` names another).
- **The image, `<OUT>/moruna-node.qcow2`:** build it as `kernel/node/HOST.md` says ("Build the
  cloud image"), adding `--formats qcow2` to its `package.py` line. Give `--node-memory` 2 GiB,
  2147483648, or less: both flavors of step 6 have 2 GiB, and a flavor's RAM must be at least the
  image's `--node-memory`. The build checks the profile's accounts against it, and the node
  supervisor checks them again at boot against the machine's real memory
  (`kernel/node/supervisor/src/main.rs`, `Config::parse`). The file's size and SHA-256 are in
  `<OUT>/inputs-and-artifacts.json`, under its path.
- **Quota** for one server of 1 vCPU and 2 GiB, one network with its subnet, one router, one
  floating IP and, for step 6b, one 2 GB volume. With `--proof`, the server has 4 vCPUs and
  16 GiB.
- **Python 3**, for the API's checks (standard library only) and, with `--proof`, `proof.py`,
  which also reads Parquet with pyarrow where it has it (`bench/node/proof.py`).
- **The image serves the proof's flavor as it is:** the `--node-memory` the image was built with is
  the build's check of the profile's accounts; at boot the node takes the machine's memory, and
  carves its job bound from it (system design 3A).

## The proof's data in Azure Blob

With `--proof`, the node reads the twelve normalised months of 2024 from Azure Blob and writes its
outputs there, over the internet through the network server's own client (`moruna-net`, system
design 3D), and fetches the four gate tasks from Blob by their addresses (3E).
`kernel/node/tools/nyc-blob.sh` makes them, in subscription `griot-dev`, Central India, in a
resource group of its own, `moruna-nyc-<yyyymmddHHMM>`, which must be new:

- a storage account, `mrnnyc` and the group's stamp, with two private containers: `data`, which
  takes the months under `nyc/` and the outputs under `proof/<stamp>/`, and `functions`, which
  takes the four tasks of `<TASKS>` (about 6 MB, uploaded from the Mac);
- the months are made inside Azure, with no upload from the Mac (about 0.3 MB/s): one
  Standard_D2s_v4 in zone 3, Ubuntu 24.04, with no inbound rule at all, runs 6.2b's
  `bench/node/engines/data-linux.sh` and `dataprep.py` once, through one managed run command
  whose one protected parameter is the account key (`kernel/node/tools/nyc-data.sh` carries
  the code). They fetch each month from NYC TLC's public address, normalise it, check it against
  `bench/node/plans/nyc-2024.json`'s SHA-256, and upload it. Once their `DATA` line says ok and
  the container holds the twelve, the machine is deleted with all that was made for it;
- two container SASes, made on the Mac from the key, https only, expiring in 6 hours: `data`'s
  reads, adds, creates, writes and lists (`racwl`), and `functions`' reads (`r`), so no function's
  address carries a write SAS. Both are written, never shown, to `<BLOB_RUN>/blob`, mode 600,
  with the account and the two containers' names. The key stays in `<BLOB_RUN>/account.key`,
  mode 600; the node and `proof.py` are given the SASes alone, `proof.py` in its environment.

A run that fails part-way deletes the group. One that succeeds leaves the storage account alone in
it until `nyc-blob.sh --delete --run <BLOB_RUN>`, which deletes the group `<BLOB_RUN>/group`
names and shows it gone. `--dry-run` prints every command and runs none.

**Written 2026-11-18 and not run against Azure.** Every `az` option it uses was checked without a
cloud: each appears in the help of its command in Azure CLI 2.92.0, the version on the project's
Mac, with no command sent to Azure; its tests (`kernel/node/tools/test_nyc_blob.py`) run it against
a stand-in for `az`. The run command's shape (version 2, the key a protected parameter read from
its file) is the one `bench/node/run.py` used on a real machine (6.2b). What Azure answers is the
PM's run to find.

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

The server's user-data is node config, version 1 (system design 1D), with that key as its one
token key:

```
printf '{"moruna":1,"api":{"token_keys":["%s"]}}\n' "$(cat <RUN>/token-key.pub)" \
  > <RUN>/node-config.json
```

It carries no secret. A token key is a public key, and with no `tls` the node makes its own key
and certificate at boot (1D). Anyone who can read the server's instance data learns nothing
that lets them call the node.

**With `--proof`,** a function key pair too, made as the token key is: node config's one function
key (3E), whose private half signs the gate tasks `proof.py` registers. Node config then holds
both public keys, in place of the one above:

```
openssl genpkey -algorithm ed25519 -out <RUN>/function-key.pem
```

```
openssl pkey -in <RUN>/function-key.pem -pubout -outform DER | tail -c 32 | base64 \
  > <RUN>/function-key.pub
```

```
printf '{"moruna":1,"api":{"token_keys":["%s"]},"functions":{"keys":["%s"]}}\n' \
  "$(cat <RUN>/token-key.pub)" "$(cat <RUN>/function-key.pub)" > <RUN>/node-config.json
```

## 2. The name is new

Steps 3 to 6 give `<NAME>` to what they make, and step 11 deletes what was made by the IDs kept
in `<RUN>`; step 11 shows the project holds nothing of `<NAME>` at the end, which proves
something only if it held nothing at the start. Each of these must succeed: the project has no
image, security group, network, subnet, router, volume or server named `<NAME>`, and `openstack`
could say so.

```
ids="$(perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> image list --name <NAME> -f value -c ID)" && [ -z "$ids" ]
```

```
names="$(perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> security group list -f value -c Name)" \
  && ! printf '%s\n' "$names" | grep -q -x -e <NAME>
```

```
ids="$(perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> network list --name <NAME> -f value -c ID)" && [ -z "$ids" ]
```

```
ids="$(perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> subnet list --name <NAME> -f value -c ID)" && [ -z "$ids" ]
```

```
ids="$(perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> router list --name <NAME> -f value -c ID)" && [ -z "$ids" ]
```

```
ids="$(perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> volume list --name <NAME> -f value -c ID)" && [ -z "$ids" ]
```

```
ids="$(perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> server list --name '^<NAME>$' -f value -c ID)" && [ -z "$ids" ]
```

- `security group list` has no name filter, so the group's check reads every name.
- `server list --name` is a regular expression; `^` and `$` make it the name alone.

From here on, after any step that fails, go to step 11.

## 3. Upload the image

```
perl -e 'alarm shift; exec @ARGV; exit 127' 600 openstack --os-cloud servercore \
  --os-region-name <REGION> image create --disk-format qcow2 --container-format bare \
  --property hw_firmware_type=uefi --property hw_machine_type=q35 \
  --property hw_disk_bus=virtio --property hw_vif_model=virtio \
  --file <OUT>/moruna-node.qcow2 -f value -c id <NAME> > <RUN>/image.id
```

The four properties are 1E's: UEFI firmware, a q35 machine, and virtio for the disk and the
network card. Servercore took all four (1.6). Then read the image back:

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> image show "$(cat <RUN>/image.id)" \
  -c status -c size -c disk_format -c container_format -c properties
```

`size` must be the file's size in `<OUT>/inputs-and-artifacts.json`, and `properties` must hold
the four above. `status` must be `active`:

```
[ "$(perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> image show "$(cat <RUN>/image.id)" -f value -c status)" = active ]
```

## 4. The security group

One rule is added: TCP 443, inbound, from anywhere.

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> security group create \
  --description 'Moruna node: TCP 443 inbound only' -f value -c id <NAME> > <RUN>/group.id
```

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> security group rule create --ingress --ethertype IPv4 \
  --protocol tcp --dst-port 443 --remote-ip 0.0.0.0/0 "$(cat <RUN>/group.id)" -f value -c id
```

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> security group rule list "$(cat <RUN>/group.id)"
```

The list must show exactly one `ingress` rule: `tcp`, port range `443:443`, `0.0.0.0/0`. It also
shows the two `egress` rules OpenStack puts in every new group (any protocol, one for IPv4 and
one for IPv6). This recipe leaves them (PM, 2026-11-17):

- they let nothing in;
- the node itself denies outbound traffic that is not granted (1C);
- the node's request for its instance data (`169.254.169.254`, port 80; 1D) is outbound
  traffic, and so, later, is a job's reach to object storage.

## 5. The private network and its router

No server goes directly on Servercore's `external-network`: its build is aborted with "Failed to
allocate the network(s)" (1.6, run 1). The server goes on a network of the run's own, with one
subnet that Neutron serves by DHCP, and a router whose gateway is `external-network` takes its
traffic out; the floating IP of step 8 brings 443 in.

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> network create -f value -c id <NAME> > <RUN>/network.id
```

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> subnet create --network "$(cat <RUN>/network.id)" \
  --subnet-range 192.168.77.0/24 --dhcp -f value -c id <NAME> > <RUN>/subnet.id
```

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> router create -f value -c id <NAME> > <RUN>/router.id
```

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> router set --external-gateway external-network \
  "$(cat <RUN>/router.id)"
```

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> router add subnet "$(cat <RUN>/router.id)" "$(cat <RUN>/subnet.id)"
```

## 6. The server

The flavor's RAM must be at least the image's `--node-memory`, and the `cloud` profile's fixed
accounts are 832 MiB, so the smallest flavor that works has 2 GiB. Each path below has its own
flavor, both 1 vCPU and 2048 MiB:

- **6a, from the image:** `BL1.1-2048-16`, whose own 16 GB disk takes the image.
- **6b, from a volume made from the image:** `BL1.1-2048`, which has no disk. Servercore refuses a
  server booted from a volume on a flavor with a disk: "It is restricted to create volume-backed
  server with non zero disk flavor".

Either path creates the server the same way otherwise:

- On the network of step 5, with the group of step 4.
- No key pair, no password and no config drive: nobody logs in, and the node reads its config
  from the metadata service (1D).
- No `--wait`: with it, the command writes its progress to standard output, into the ID's file.
- The time the command returns is noted, for step 9.

### 6a. From the image

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> server create --image "$(cat <RUN>/image.id)" \
  --flavor BL1.1-2048-16 --network "$(cat <RUN>/network.id)" \
  --security-group "$(cat <RUN>/group.id)" --user-data <RUN>/node-config.json \
  -f value -c id <NAME> > <RUN>/server.id && date -u +%s > <RUN>/created.at
```

**With `--proof`,** on `SL1.4-16384-128` in its place: 4 vCPUs and 16 GiB, as Azure's
Standard_D4s_v4 where the proof's Azure half ran, with a 128 GB disk of its own, so the proof
boots from the image only:

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> server create --image "$(cat <RUN>/image.id)" \
  --flavor SL1.4-16384-128 --network "$(cat <RUN>/network.id)" \
  --security-group "$(cat <RUN>/group.id)" --user-data <RUN>/node-config.json \
  -f value -c id <NAME> > <RUN>/server.id && date -u +%s > <RUN>/created.at
```

### 6b. From a volume

Volume types are per zone on Servercore, `basic.<ZONE>`: the volume names its type and its zone,
and the server is made in the same zone. Make the volume from the image, 2 GB, bootable:

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> volume create --image "$(cat <RUN>/image.id)" --size 2 --bootable \
  --type basic.<ZONE> --availability-zone <ZONE> -f value -c id <NAME> > <RUN>/volume.id
```

Wait for the image to be copied onto it, at most 5 minutes. It must end at `available`:

```
for _ in $(seq 60); do perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack \
  --os-cloud servercore --os-region-name <REGION> volume show "$(cat <RUN>/volume.id)" \
  -f value -c status > <RUN>/volume.status; cat <RUN>/volume.status; \
  case "$(cat <RUN>/volume.status)" in available | error) break ;; esac; sleep 5; done; \
  [ "$(cat <RUN>/volume.status)" = available ]
```

Then read the volume back. `bootable` must be `true` and `volume_image_metadata` must hold the
image's four properties of step 3: a server booted from a volume takes its firmware, machine
type and device models from there.

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> volume show "$(cat <RUN>/volume.id)" \
  -c status -c size -c bootable -c volume_image_metadata
```

Create the server from the volume, in the volume's zone:

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> server create --volume "$(cat <RUN>/volume.id)" \
  --flavor BL1.1-2048 --availability-zone <ZONE> --network "$(cat <RUN>/network.id)" \
  --security-group "$(cat <RUN>/group.id)" --user-data <RUN>/node-config.json \
  -f value -c id <NAME> > <RUN>/server.id && date -u +%s > <RUN>/created.at
```

The volume outlives the server: step 11 deletes it.

## 7. Watch the server from its create

From the create on, read the server's status and its console every few seconds, until the server
leaves `BUILD`, at most 5 minutes. A node that stops shows `SHUTOFF` within about 30 s (item
1.6's runs), and its console may then be gone, so it is read from the first.

```
touch <RUN>/console.log <RUN>/console.err; deadline=$(( $(date -u +%s) + 300 )); \
  while [ "$(date -u +%s)" -lt "$deadline" ]; do \
  perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> server show "$(cat <RUN>/server.id)" -f value -c status \
  > <RUN>/status; \
  perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> console log show "$(cat <RUN>/server.id)" > <RUN>/console.now \
  2> <RUN>/console.now.err; cat <RUN>/console.now.err >> <RUN>/console.err; \
  [ "$(wc -c < <RUN>/console.now)" -le "$(wc -c < <RUN>/console.log)" ] \
  || cp <RUN>/console.now <RUN>/console.log; \
  printf '%s %s, console %s bytes %s\n' "$(date -u +%H:%M:%S)" "$(cat <RUN>/status)" \
  "$(( $(wc -c < <RUN>/console.now) ))" "$(head -c 160 <RUN>/console.now.err | tr '\n' ' ')"; \
  [ "$(cat <RUN>/status)" = BUILD ] || [ ! -s <RUN>/status ] || break; sleep 2; done
```

- Each round prints the time, the status, the console's size and the first of its errors.
- **The console's errors are kept,** all of them, in `<RUN>/console.err`. In item 1.6's runs 3
  to 6, Servercore answered `console log show` for this image with 404, "Guest does not have a
  console available", from the create to the end (item 1.6b looks into it).
- **`<RUN>/console.log` is the longest log read,** since a server that has stopped may answer
  with none.
- A status that could not be read, an empty `<RUN>/status`, is read again in the next round.

Then show the status, the server's private address and, after `ERROR`, the `fault` that says
why:

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> server show "$(cat <RUN>/server.id)" -c status -c addresses -c fault
```

It must have ended at `ACTIVE`:

```
[ "$(cat <RUN>/status)" = ACTIVE ]
```

## 8. The floating IP

A floating IP from `external-network`, given to the server: it is the address the node answers
on from outside, `<RUN>/address`.

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> floating ip create external-network -f value -c id > <RUN>/fip.id
```

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> floating ip show "$(cat <RUN>/fip.id)" \
  -f value -c floating_ip_address > <RUN>/address
```

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> server add floating ip "$(cat <RUN>/server.id)" \
  "$(cat <RUN>/address)"
```

## 9. The console log

The node writes its boot lines to the first serial port, which OpenStack keeps as the console
log (1B). Read on, as in step 7, until the log holds the node's `ready` or `fatal` line, or the
server leaves `ACTIVE`, at most 5 minutes; note the time and show the node's lines:

```
deadline=$(( $(date -u +%s) + 300 )); while [ "$(date -u +%s)" -lt "$deadline" ]; do \
  perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> server show "$(cat <RUN>/server.id)" -f value -c status \
  > <RUN>/status; \
  perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> console log show "$(cat <RUN>/server.id)" > <RUN>/console.now \
  2> <RUN>/console.now.err; cat <RUN>/console.now.err >> <RUN>/console.err; \
  [ "$(wc -c < <RUN>/console.now)" -le "$(wc -c < <RUN>/console.log)" ] \
  || cp <RUN>/console.now <RUN>/console.log; \
  printf '%s %s, console %s bytes %s\n' "$(date -u +%H:%M:%S)" "$(cat <RUN>/status)" \
  "$(( $(wc -c < <RUN>/console.now) ))" "$(head -c 160 <RUN>/console.now.err | tr '\n' ' ')"; \
  tr -d '\r' < <RUN>/console.log | grep -a -q -e 'moruna: ready' -e 'moruna: fatal' && break; \
  [ "$(cat <RUN>/status)" = ACTIVE ] || [ ! -s <RUN>/status ] || break; sleep 2; done; \
  date -u +%s > <RUN>/seen.at; tr -d '\r' < <RUN>/console.log | grep -a 'moruna: '
```

The node is ready, and on OpenStack:

```
tr -d '\r' < <RUN>/console.log | grep -a -q 'moruna: ready api='
```

```
tr -d '\r' < <RUN>/console.log | grep -a -q 'moruna: cloud openstack instance '
```

- **Success** is 1B's boot lines in order, the `cloud` line naming `openstack`, and the last line
  `moruna: ready api=<address>:443 boot_ms=<n>`, with the server's private address on
  `192.168.77.0/24`: the floating IP is the router's, outside the server.
- **Create-to-ready time** (1E) is `seen.at` less `created.at`, in seconds, to within a round of
  the loop. `boot_ms` is on the `ready` line.
- **`moruna: fatal <stage>: <reason>`** is the node refusing to run. It stays halted for 60 s so
  the console can still be read (Nova serves it only while the server runs), then powers off, and
  the server shows `SHUTOFF` about 15 s after that.
- **`root.ecam: the firmware describes no PCI configuration space (ACPI MCFG)`** is the core on
  a machine that is not q35 (`kernel/core/src/boot.rs`): Servercore ignored `hw_machine_type`.
  Stop and report (1E).
- **No console at all** (`<RUN>/console.log` empty, `<RUN>/console.err` holding the 404s) fails
  here, whatever the node did. The log, when there is one, starts with the firmware's own output,
  which holds terminal escapes.

## 10. Check

These are 1E's success, from a machine outside the cloud. 443 accepts; 22 does not, since the
security group drops it, so `nc` times out:

```
nc -vz -w 5 "$(cat <RUN>/address)" 443
```

```
! nc -vz -w 5 "$(cat <RUN>/address)" 22
```

The server runs, with nobody having logged in:

```
[ "$(perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> server show "$(cat <RUN>/server.id)" -f value -c status)" = ACTIVE ]
```

The certificate's fingerprint the console printed, `moruna: tls sha256=<fingerprint>` (2B), and
the instance ID: every call to the API from here on is pinned to the one, and its tokens name the
other as their `aud` (2C):

```
tr -d '\r' < <RUN>/console.log | grep -a -o 'moruna: tls sha256=[0-9a-f]*' | head -1 \
  | cut -d= -f2 > <RUN>/console.pin && [ -s <RUN>/console.pin ]
```

```
tr -d '\r' < <RUN>/console.log | grep -a -o 'moruna: cloud openstack instance [^ ]*' \
  | head -1 | cut -d' ' -f5 > <RUN>/instance && [ -s <RUN>/instance ]
```

### With `--proof`: the proof

The four NYC taxi jobs through the API, Score, Occupancy, Enrich and Audit, on January 2024 and
then on all of 2024, one at a time (work items 3.8 and 3.8b; `bench/node/proof.py`). `proof.py`:

- registers each gate task at `https://<ACCOUNT>.blob.core.windows.net/functions/gate-JOB?<SAS>`,
  `functions`' read-only SAS, signed with the run's function key (3E);
- makes its own tokens for each scope with the run's token key, for the instance above, and pins
  the certificate the console printed;
- runs each job from and to Blob: its inputs `az://data/nyc/…`, its output under
  `az://data/proof/<stamp>/…`. `data`'s SAS reaches the node by secret name, as 3B says a
  credential does: the document names `{"secret": "blob-sas"}` in `object_store.azure.sas_token`,
  and the job's body carries `"secrets": {"blob-sas": <SAS>}` beside it, which `api` passes to the
  engine outside the document and redacts everywhere. The node reaches Central India from
  Servercore over the internet;
- follows each job's events to its report, reads its output back from Blob with the same SAS
  (List Blobs and Get Blob), and checks its rows and canonical hashes against 0.6.6's references
  in `<NYC>`, with `<GATE_CANON>` for the year;
- writes each run's record and the summary, `proof.json`, to `<RUN>/proof`, and prints a table.
  It exits 0 only when all eight runs pass.

Both SASes reach it in the environment, read from `<BLOB_RUN>/blob`, never on a command line, and
it never prints or writes either: an address it shows, and a node's message that quotes one,
name them `<FUNCTIONS_SAS>` and `<BENCH_SAS>`. A proof that fails fails the run, after the API's
checks below:

```
BENCH_SAS="$(sed -n 's/^data_sas=//p' <BLOB_RUN>/blob)" \
  FUNCTIONS_SAS="$(sed -n 's/^functions_sas=//p' <BLOB_RUN>/blob)" \
  MORUNA_BENCH_OPENSSL=openssl python3 -I <REPO>/bench/node/proof.py \
  --url "https://$(cat <RUN>/address):443" --pin "$(cat <RUN>/console.pin)" \
  --token-key <RUN>/token-key.pem --instance "$(cat <RUN>/instance)" \
  --function-key <RUN>/function-key.pem --functions <TASKS> \
  --function-base https://<ACCOUNT>.blob.core.windows.net/functions --data blob \
  --blob-account <ACCOUNT> --blob-container data --periods 2024-01,2024 --references <NYC> \
  --gate-canon <GATE_CANON> --results <RUN>/proof --work <RUN>/proof-work
```

A month's run may take at most an hour and the year's six (`proof.py`'s `--timeout-s` and
`--year-timeout-s`); one past its time is cancelled and fails. The SASes last 6 hours from
`nyc-blob.sh`'s end (`<BLOB_RUN>/blob`'s `expires`), so start the run well inside them.

### The API's checks

Last, on every run, after the proof when there is one, whether or not it passed: the API's proof
from outside the cloud (work item 2.6, `kernel/node/tools/api-checks.py`). Every connection is
pinned to `<RUN>/console.pin` before anything is sent. It checks, in order:

- `GET /v1/health` with no token answers `{"status":"ok"}`;
- `GET /v1/node` with an `observe` token of the run's key answers the node document, its `cloud`
  `openstack` and its `instance` the console's;
- over MCP (`POST /mcp`, the same token): `initialize` answers as server `moruna`; `tools/list`
  names `moruna_health`, `moruna_node` and `moruna_node_audit`; `tools/call` of `moruna_node`
  answers the same node document;
- a token signed by another key, made for the check and thrown away, and one of the run's key
  for another instance are each refused with `401 unauthorized`, on `/v1/node` and on `/mcp`.

Its results are kept in `<RUN>/api-checks.json`, its tokens nowhere. The rest of 2.6, `api` killed
and the node still answering, cannot be done on a cloud node, which has no way in but its API;
item 2.2c's booted test covers it.

```
python3 -I <REPO>/kernel/node/tools/api-checks.py --url "https://$(cat <RUN>/address):443" \
  --pin "$(cat <RUN>/console.pin)" --token-key <RUN>/token-key.pem \
  --instance "$(cat <RUN>/instance)" --openssl openssl --results <RUN>/api-checks.json \
  --cloud openstack
```

## 11. Delete everything

In the reverse of the order things were made: the floating IP, the server, the volume once the
server has let go of it, the router's subnet, the router's gateway, the router, the subnet, the
network, the security group and the image. After a step that failed, delete what was made before
it: each command here applies when its file in `<RUN>` holds an ID, and `openstack-up.sh` runs
only those. Each call is bounded, as everywhere, and a call that fails or is cut short does not
stop the ones after it.

Read the console one last time first, keeping the longest log:

```
touch <RUN>/console.log <RUN>/console.err; \
  perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> console log show "$(cat <RUN>/server.id)" > <RUN>/console.now \
  2> <RUN>/console.now.err; cat <RUN>/console.now.err >> <RUN>/console.err; \
  [ "$(wc -c < <RUN>/console.now)" -le "$(wc -c < <RUN>/console.log)" ] \
  || cp <RUN>/console.now <RUN>/console.log
```

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> floating ip delete "$(cat <RUN>/fip.id)"
```

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> server delete --wait "$(cat <RUN>/server.id)"
```

After step 6b only, the boot volume. Wait for the server to let go of it, at most 2 minutes,
then delete it:

```
for _ in $(seq 24); do perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack \
  --os-cloud servercore --os-region-name <REGION> volume show "$(cat <RUN>/volume.id)" \
  -f value -c status > <RUN>/volume.status; cat <RUN>/volume.status; \
  case "$(cat <RUN>/volume.status)" in in-use | detaching) sleep 5 ;; *) break ;; esac; done
```

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> volume delete "$(cat <RUN>/volume.id)"
```

The router's subnet and its gateway, then the router, the subnet and the network:

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> router remove subnet "$(cat <RUN>/router.id)" \
  "$(cat <RUN>/subnet.id)"
```

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> router unset --external-gateway "$(cat <RUN>/router.id)"
```

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> router delete "$(cat <RUN>/router.id)"
```

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> subnet delete "$(cat <RUN>/subnet.id)"
```

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> network delete "$(cat <RUN>/network.id)"
```

The security group, then the image:

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> security group delete "$(cat <RUN>/group.id)"
```

```
perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> image delete "$(cat <RUN>/image.id)"
```

That is everything this recipe made: the floating IP; the server, whose port on the run's network
goes with it; the boot volume of step 6b; the router, with its gateway and its port on the
subnet; the subnet and the network, whose DHCP port goes with them; the group, whose rules go with
it; and the image. It made no key pair, and after step 6a no volume.

Show that nothing is left: step 2's seven checks, again, must each succeed, and the floating IP's
ID must not be in the project's list. A check by ID is no proof here: in item 1.6's runs,
`image show` still answered with a deleted image's ID while `image list --name` no longer named
it.

```
ids="$(perl -e 'alarm shift; exec @ARGV; exit 127' 120 openstack --os-cloud servercore \
  --os-region-name <REGION> floating ip list -f value -c ID)" \
  && ! printf '%s\n' "$ids" | grep -q -x -e "$(cat <RUN>/fip.id)"
```

Keep `<RUN>` with the report: it holds the console log, its errors and the two times, and the API's
checks' results. Its `token-key.pem` is the run's private key, and is never committed. With
`--proof` it also holds the proof's results, `<RUN>/proof`, and `function-key.pem`, a private key
too, never committed. The proof's data stays in Blob until `nyc-blob.sh --delete`.
