# The node on OpenStack

How to put the node on an OpenStack cloud: upload its image, create one machine from it with
node config as user-data and a firewall rule for port 443 only, read its console, check it, and
delete everything (system design `architecture/moruna-os-system.md`, section 1E; work item 1.5).
It is written for Servercore, cloud `servercore` in `~/.config/openstack/clouds.yaml`. The
machine boots from the image itself where the flavor has a disk of its own (step 4a), and from a
2 GB volume made from the image where it has none (step 4b).

- **Written 2026-11-17 and not run.** Item 1.6 runs it, and corrects it here where it is wrong.
- **Every `openstack` line below was checked without a cloud:** parsed by the argument parsers of
  python-openstackclient 10.5.0, the version on the project's Mac (`openstack --version` shows
  it), with the `openstack` program itself never run. That shows each option exists and is
  spelled right. What Servercore answers is item 1.6's to find.
- **Creating anything on a cloud needs the PM's go-ahead each time** (1E).
- **Every command stands alone:** it names its cloud and region, and reads the IDs of what was
  created from files in the run's directory. Nothing depends on a shell variable or on the shell
  it was typed in, so the steps may be run from separate shells.

## The values the operator fills in

Each is written `<LIKE_THIS>` below. Replace every one before running a command. Nothing else in
a command changes.

| Value | What it is | Where it comes from |
| --- | --- | --- |
| `<REGION>` | The Servercore region the machine is created in | The project's account. `openstack --os-cloud servercore catalog list` names the regions that have a `compute` endpoint |
| `<FLAVOR>` | The name or ID of the smallest flavor with 1 CPU and 1 GiB of memory or more. Whether it has a disk of its own decides step 4's path | Step 4's `flavor list` |
| `<EXTERNAL_NETWORK>` | The name or ID of the external network the machine is put on | Step 4's `network list --external` |
| `<VOLUME_TYPE>` | Step 4b only: the type of the boot volume | Step 4b's `volume type list` |
| `<ZONE>` | Step 4b only: the availability zone the boot volume and the server are both created in | Step 4b's `availability zone list` |
| `<OUT>` | The directory `package.py` wrote, holding `moruna-node.qcow2` | The build, below |
| `<RUN>` | A new directory for this run's files: node config, the IDs, the console log, the times. Outside the repository, since a private key is kept there | The operator chooses it. Step 1 makes it |
| `<NAME>` | One name for this run's image, security group and server, for example `moruna-node-20261117-1` | The operator chooses it. No other image, group or server of the project has it |
| `<TOKEN_KEY>` | The public half of an Ed25519 key pair: standard base64 of its 32 bytes (work item 1.4, step 5) | Step 1 |
| `<ADDRESS>` | The machine's public IPv4 address | Step 4's `server show`, and the console's `network` line |

## What it needs

- **The `openstack` command line** with cloud `servercore` configured.
- **The image, `<OUT>/moruna-node.qcow2`:** build it as `kernel/node/HOST.md` says ("Build the
  cloud image"), adding `--formats qcow2` to its `package.py` line. Give `--node-memory` the
  flavor's memory or less: the build checks the profile's accounts against it, and the node
  supervisor checks them again at boot against the machine's real memory
  (`kernel/node/supervisor/src/main.rs`, `Config::parse`). The file's size and SHA-256 are in
  `<OUT>/inputs-and-artifacts.json`, under its path.

## 1. Node config

Make the run's directory:

```
mkdir -m 700 <RUN>
```

The machine's user-data is node config, version 1 (system design 1D). Write this to
`<RUN>/node-config.json`:

```json
{
  "moruna": 1,
  "api": {
    "token_keys": ["<TOKEN_KEY>"]
  }
}
```

It carries no secret. A token key is a public key, and with no `tls` the node makes its own key
and certificate at boot (1D). Anyone who can read the machine's instance data learns nothing
that lets them call the node.

One way to make the key pair, with OpenSSL 1.1.1 or later:

```
openssl genpkey -algorithm ed25519 -out <RUN>/token-key.pem
openssl pkey -in <RUN>/token-key.pem -pubout -outform DER | tail -c 32 | base64
```

- The first command writes the private half. It stays in `<RUN>`: never commit it, and never
  put it in node config.
- The second prints `<TOKEN_KEY>`, 44 characters: an Ed25519 public key's DER form is 44 bytes,
  the last 32 of them the key.
- macOS's own `openssl` is LibreSSL 3.3.6, which answers `Algorithm ed25519 not found`. On the
  project's Mac, name Homebrew's: `/opt/homebrew/bin/openssl` (both verified there, 2026-11-17).

## 2. Upload the image

```
openstack --os-cloud servercore --os-region-name <REGION> image create \
  --disk-format qcow2 --container-format bare \
  --property hw_firmware_type=uefi --property hw_machine_type=q35 \
  --property hw_disk_bus=virtio --property hw_vif_model=virtio \
  --file <OUT>/moruna-node.qcow2 \
  -f value -c id <NAME> > <RUN>/image.id
```

The four properties are 1E's: UEFI firmware, a q35 machine, and virtio for the disk and the
network card. Then read the image back:

```
openstack --os-cloud servercore --os-region-name <REGION> image show "$(cat <RUN>/image.id)" \
  -c status -c size -c disk_format -c container_format -c properties
```

Go on only when `status` is `active`, `size` is the file's size in
`<OUT>/inputs-and-artifacts.json`, and `properties` holds the four above. If Servercore refuses
the image or a property, stop and report (item 1.6).

## 3. The security group

One rule is added: TCP 443, inbound, from anywhere.

```
openstack --os-cloud servercore --os-region-name <REGION> security group create \
  --description "Moruna node: TCP 443 inbound only" \
  -f value -c id <NAME> > <RUN>/group.id
```

```
openstack --os-cloud servercore --os-region-name <REGION> security group rule create \
  --ingress --ethertype IPv4 --protocol tcp --dst-port 443 --remote-ip 0.0.0.0/0 \
  "$(cat <RUN>/group.id)"
```

```
openstack --os-cloud servercore --os-region-name <REGION> security group rule list \
  "$(cat <RUN>/group.id)"
```

The list must show exactly one `ingress` rule: `tcp`, port range `443:443`, `0.0.0.0/0`. It also
shows the two `egress` rules OpenStack puts in every new group (any protocol, one for IPv4 and
one for IPv6). This recipe leaves them (PM, 2026-11-17):

- they let nothing in;
- the node itself denies outbound traffic that is not granted (1C);
- the node's request for its instance data (`169.254.169.254`, port 80; 1D) is outbound
  traffic, and so, later, is a job's reach to object storage.

## 4. The server

Find the two values first. Both commands only read.

```
openstack --os-cloud servercore --os-region-name <REGION> flavor list --sort-column RAM
```

`<FLAVOR>` is the first row with `VCPUs` of 1 or more and `RAM` of 1024 (MiB) or more. Its `Disk`
column (GiB) decides how the server boots:

- **`Disk` is not 0: step 4a.** The server boots from the image, on the flavor's own disk.
- **`Disk` is 0: step 4b.** The server boots from a volume made from the image. A flavor with
  disk 0 is for a boot from a volume (Nova's `zero_disk_flavor` policy can refuse an image boot
  on one). Servercore's small flavors mostly have no disk of their own, so this is the path
  expected there (PM, 2026-11-17).

```
openstack --os-cloud servercore --os-region-name <REGION> network list --external
```

`<EXTERNAL_NETWORK>` is the external network the project may put a machine on.

Either path creates the server the same way otherwise:

- No key pair, no password, no config drive and no floating IP: nobody logs in, the node reads
  its config from the metadata service (1D), and its address is the external network's.
- No `--wait`: with it, the command writes its progress to standard output, into the ID's file.
- The time the command returns is noted, for step 5.

### 4a. From the image

```
openstack --os-cloud servercore --os-region-name <REGION> server create \
  --image "$(cat <RUN>/image.id)" --flavor <FLAVOR> --network <EXTERNAL_NETWORK> \
  --security-group "$(cat <RUN>/group.id)" --user-data <RUN>/node-config.json \
  -f value -c id <NAME> > <RUN>/server.id && date -u +%s > <RUN>/created.at
```

### 4b. From a volume

Find the volume's two values first. These commands only read.

```
openstack --os-cloud servercore --os-region-name <REGION> volume type list
openstack --os-cloud servercore --os-region-name <REGION> availability zone list --volume
openstack --os-cloud servercore --os-region-name <REGION> availability zone list --compute
```

`<VOLUME_TYPE>` is one of the types. `<ZONE>` is a zone that both zone lists name: the volume
and the server are created in the same one.

Make the volume from the image, 2 GB:

```
openstack --os-cloud servercore --os-region-name <REGION> volume create \
  --image "$(cat <RUN>/image.id)" --size 2 --type <VOLUME_TYPE> --availability-zone <ZONE> \
  -f value -c id <NAME> > <RUN>/volume.id
```

Wait for the image to be copied onto it, at most 5 minutes:

```
for _ in $(seq 60); do
  state="$(openstack --os-cloud servercore --os-region-name <REGION> volume show \
    "$(cat <RUN>/volume.id)" -f value -c status)"
  echo "$state"
  [ "$state" = creating ] || [ "$state" = downloading ] || break
  sleep 5
done
```

It must end at `available`. Then read the volume back:

```
openstack --os-cloud servercore --os-region-name <REGION> volume show "$(cat <RUN>/volume.id)" \
  -c status -c size -c bootable -c volume_image_metadata
```

Go on only when `bootable` is `true` and `volume_image_metadata` holds the image's four
properties of step 2: a server booted from a volume takes its firmware, machine type and device
models from there. If the volume ends at `error`, or is not `available` after the 5 minutes, go
to step 7 and report.

Create the server from the volume, in the volume's zone:

```
openstack --os-cloud servercore --os-region-name <REGION> server create \
  --volume "$(cat <RUN>/volume.id)" --flavor <FLAVOR> --network <EXTERNAL_NETWORK> \
  --availability-zone <ZONE> \
  --security-group "$(cat <RUN>/group.id)" --user-data <RUN>/node-config.json \
  -f value -c id <NAME> > <RUN>/server.id && date -u +%s > <RUN>/created.at
```

The volume outlives the server: step 7 deletes it.

### After either

Wait for the server to leave `BUILD`, at most 5 minutes:

```
for _ in $(seq 60); do
  state="$(openstack --os-cloud servercore --os-region-name <REGION> server show \
    "$(cat <RUN>/server.id)" -f value -c status)"
  echo "$state"
  [ "$state" = BUILD ] || break
  sleep 5
done
```

It must end at `ACTIVE`. Then read its address, `<ADDRESS>`:

```
openstack --os-cloud servercore --os-region-name <REGION> server show "$(cat <RUN>/server.id)" \
  -c status -c addresses -c fault
```

If it ends at `ERROR`, `fault` says why. Then, or if it is still `BUILD` after the 5 minutes, go
to step 7 and report.

## 5. The console log

The node writes its boot lines to the first serial port, which OpenStack keeps as the console
log (1B). Read it until the node is ready or has stopped, at most 5 minutes, and note the time:

```
for _ in $(seq 60); do
  openstack --os-cloud servercore --os-region-name <REGION> console log show \
    "$(cat <RUN>/server.id)" > <RUN>/console.log
  grep -a -q -e 'moruna: ready' -e 'moruna: fatal' <RUN>/console.log && break
  sleep 5
done
date -u +%s > <RUN>/seen.at
grep -a 'moruna: ' <RUN>/console.log
```

- **Success** is 1B's boot lines in order, the last one
  `moruna: ready api=<ADDRESS>:443 boot_ms=<n>`, with `<ADDRESS>` the one step 4 read and the
  `cloud` line naming `openstack`.
- **Create-to-ready time** (1E), when the `ready` line is there, is `seen.at` less `created.at`,
  in seconds, to within the 5 seconds between two reads. `boot_ms` is on the `ready` line.
- **`moruna: fatal <stage>: <reason>`** is the node refusing to run. It then powers off, and the
  server shows `SHUTOFF`.
- **`root.ecam: the firmware describes no PCI configuration space (ACPI MCFG)`** is the core on
  a machine that is not q35 (`kernel/core/src/boot.rs`): Servercore ignored `hw_machine_type`.
  Stop and report (1E).
- **Neither `ready` nor `fatal`** after the 5 minutes: keep `<RUN>/console.log`, go to step 7,
  then report. The log starts with the firmware's own output, which holds terminal escapes.

## 6. Check

These are 1E's success, from a machine outside the cloud:

```
nc -vz -w 5 <ADDRESS> 443
nc -vz -w 5 <ADDRESS> 22
```

- The first must connect. The second must not: the security group drops it, so it times out.
- The server shows as running, with nobody having logged in:

```
openstack --os-cloud servercore --os-region-name <REGION> server show "$(cat <RUN>/server.id)" \
  -f value -c status
```

It must print `ACTIVE`.

## 7. Delete everything

In this order: the volume and the group are in use until the server is gone, and a volume made
from an image can keep that image in use. After a step that failed, delete what was created
before it: each command here applies when its file in `<RUN>` holds an ID.

```
openstack --os-cloud servercore --os-region-name <REGION> server delete --wait \
  "$(cat <RUN>/server.id)"
```

After step 4b only, the boot volume. Wait for the server to let go of it, at most 2 minutes,
then delete it:

```
for _ in $(seq 24); do
  state="$(openstack --os-cloud servercore --os-region-name <REGION> volume show \
    "$(cat <RUN>/volume.id)" -f value -c status)"
  echo "$state"
  [ "$state" = in-use ] || [ "$state" = detaching ] || break
  sleep 5
done
openstack --os-cloud servercore --os-region-name <REGION> volume delete "$(cat <RUN>/volume.id)"
```

```
openstack --os-cloud servercore --os-region-name <REGION> security group delete \
  "$(cat <RUN>/group.id)"
```

```
openstack --os-cloud servercore --os-region-name <REGION> image delete "$(cat <RUN>/image.id)"
```

That is everything this recipe created: the server (its port on the external network was made
with it and goes with it), the boot volume of step 4b, the group (its rules go with it) and the
image. It made no floating IP, key pair, network or router, and after step 4a no volume.

Show that each is gone. Every one of these three must fail, saying it found nothing:

```
openstack --os-cloud servercore --os-region-name <REGION> server show "$(cat <RUN>/server.id)"
openstack --os-cloud servercore --os-region-name <REGION> security group show "$(cat <RUN>/group.id)"
openstack --os-cloud servercore --os-region-name <REGION> image show "$(cat <RUN>/image.id)"
```

After step 4b, so must this one. A volume takes a few seconds to go: while it still answers,
with the status `deleting`, run it again.

```
openstack --os-cloud servercore --os-region-name <REGION> volume show "$(cat <RUN>/volume.id)"
```

Then list what the project still holds. Nothing in these lists is named `<NAME>` or has an ID
from `<RUN>`; the project's own `default` security group stays.

```
openstack --os-cloud servercore --os-region-name <REGION> server list
openstack --os-cloud servercore --os-region-name <REGION> image list --name <NAME>
openstack --os-cloud servercore --os-region-name <REGION> volume list
openstack --os-cloud servercore --os-region-name <REGION> port list
openstack --os-cloud servercore --os-region-name <REGION> floating ip list
openstack --os-cloud servercore --os-region-name <REGION> security group list
```

Keep `<RUN>` with the report: it holds the console log and the two times. Its
`token-key.pem` is the run's private key, and is never committed.
