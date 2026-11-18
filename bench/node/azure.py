"""The node benchmark's `az` commands (system design 6A, work item 6.1), as argument lists.

Everything is made in one resource group of subscription `griot-dev` in Central India,
`moruna-bench-<date>`, and the group is deleted at the end. Every command names the subscription
and that group, so none can touch another (`guard` checks it).

The Moruna node's steps are `kernel/node/AZURE.md`'s (item 1.13), as `azure-up.sh --proof` runs
them: a page blob, a Generation 2 managed disk from it, a static address, a network security group
admitting TCP 443 from the operator's address alone (1E), and the machine (`Standard` security
type, accelerated networking off, boot diagnostics into the run's account from the first
power-on, so it is never restarted, node config as user-data), made with `--no-wait`.

The Linux machine for the other engines is the node's size in the node's zone, with the same
network settings; it has no inbound rule at all and is driven only through `az vm run-command`,
each of which starts a detached batch (engines/batch.py) and returns, so none nears Azure's 90
minutes; the batches report through the run's `results` container.
Both machines have a public address, since a new virtual network gives a machine no outbound
access without one, and the Linux machine installs its engines from the internet.
"""
import dataclasses
import shlex
import urllib.parse

SUBSCRIPTION = "griot-dev"
LOCATION = "centralindia"
# `functions` holds the gate tasks the node fetches by their SAS address (3E), as the proof does.
CONTAINERS = ("vhds", "data", "code", "functions", "results")
NODE, LINUX = "moruna-node", "bench-linux"


@dataclasses.dataclass(frozen=True)
class Cloud:
    stamp: str                      # yyyymmddHHMM, the run's
    size: str = "Standard_D4s_v4"
    zone: str = "3"
    linux_image: str = "Canonical:ubuntu-24_04-lts:server:latest"
    os_disk_gib: int = 128
    subscription: str = SUBSCRIPTION
    location: str = LOCATION

    @property
    def group(self):
        return f"moruna-bench-{self.stamp[:8]}"

    @property
    def account(self):
        # A storage account's name: 3 to 24 lowercase letters and digits, unique in Azure.
        return f"mrnb{self.stamp}"

    def az(self, *args, group=True):
        cmd = ["az", *args, "--subscription", self.subscription]
        if group:
            cmd[cmd.index("--subscription"):cmd.index("--subscription")] = ["-g", self.group]
        return cmd


def blob(cloud: Cloud, key: str, *args):
    """A data-plane `az storage` command on the run's account, with its key."""
    return ["az", "storage", *args, "--account-name", cloud.account, "--account-key", key]


def group_create(c: Cloud):
    return ["az", "group", "create", "-n", c.group, "-l", c.location, "--subscription", c.subscription,
            "--tags", "purpose=moruna-bench", "item=6.1", "-o", "none"]


def storage(c: Cloud):
    return [c.az("storage", "account", "create", "-n", c.account, "-l", c.location, "--sku", "Standard_LRS",
                 "--kind", "StorageV2", "--min-tls-version", "TLS1_2", "--allow-blob-public-access", "false",
                 "-o", "none")]


def storage_key(c: Cloud):
    return c.az("storage", "account", "keys", "list", "-n", c.account, "--query", "[0].value", "-o", "tsv")


def containers(c: Cloud, key: str):
    return [blob(c, key, "container", "create", "-n", name, "-o", "none") for name in CONTAINERS]


def upload(c: Cloud, key: str, container: str, name: str, path: str, page=False):
    kind = ["--type", "page"] if page else []
    return blob(c, key, "blob", "upload", "-c", container, "-n", name, "-f", path, *kind, "-o", "none")


def blob_download(c: Cloud, key: str, container: str, name: str, path: str):
    """One blob of the run's account into the file `path` on this machine."""
    return blob(c, key, "blob", "download", "-c", container, "-n", name, "-f", path, "-o", "none")


def sas(c: Cloud, key: str, container: str, permissions: str, expiry: str):
    """A container SAS: made from the key on this machine, no call to Azure."""
    return blob(c, key, "container", "generate-sas", "-n", container, "--permissions", permissions,
                "--expiry", expiry, "--https-only", "-o", "tsv")


def blob_url(c: Cloud, key: str, container: str, name: str):
    return blob(c, key, "blob", "url", "-c", container, "-n", name, "-o", "tsv")


def node_disk(c: Cloud, vhd_url: str, account_id: str):
    return c.az("disk", "create", "-n", f"{NODE}-os", "-l", c.location, "--source", vhd_url,
                "--source-storage-account-id", account_id, "--hyper-v-generation", "V2", "--os-type", "Linux",
                "--sku", "Standard_LRS", "--zone", c.zone, "-o", "none")


def account_id(c: Cloud):
    return c.az("storage", "account", "show", "-n", c.account, "--query", "id", "-o", "tsv")


def nsgs(c: Cloud, operator: str):
    """The node's group admits TCP 443 from the operator's address alone (AZURE.md, step 5); the
    Linux machine's admits nothing."""
    return [c.az("network", "nsg", "create", "-n", f"{NODE}-nsg", "-l", c.location, "-o", "none"),
            c.az("network", "nsg", "rule", "create", "--nsg-name", f"{NODE}-nsg", "-n", "https-from-operator",
                 "--priority", "100", "--direction", "Inbound", "--access", "Allow", "--protocol", "Tcp",
                 "--source-address-prefixes", f"{operator}/32", "--source-port-ranges", "*",
                 "--destination-address-prefixes", "*", "--destination-port-ranges", "443", "-o", "none"),
            c.az("network", "nsg", "create", "-n", f"{LINUX}-nsg", "-l", c.location, "-o", "none")]


def node_ip(c: Cloud):
    """The node's static address, made before the machine (AZURE.md, step 5)."""
    return c.az("network", "public-ip", "create", "-n", f"{NODE}-ip", "-l", c.location, "--zone", c.zone,
                "--sku", "Standard", "--allocation-method", "Static", "--version", "IPv4", "-o", "none")


def node_address(c: Cloud):
    return c.az("network", "public-ip", "show", "-n", f"{NODE}-ip", "--query", "ipAddress", "-o", "tsv")


def node_vm(c: Cloud, user_data: str):
    """AZURE.md, step 6: boot diagnostics into the run's account from the first power-on, so the
    machine is never restarted, and `--no-wait`: the command returns when Azure has taken it."""
    return c.az("vm", "create", "-n", NODE, "-l", c.location, "--attach-os-disk", f"{NODE}-os", "--os-type", "linux",
                "--size", c.size, "--zone", c.zone, "--security-type", "Standard", "--accelerated-networking", "false",
                "--public-ip-address", f"{NODE}-ip", "--nsg", f"{NODE}-nsg", "--boot-diagnostics-storage", c.account,
                "--user-data", user_data, "--no-wait", "-o", "none")


def boot_log(c: Cloud):
    """The CLI's own read of the console: a fallback only. On 2026-11-18 it failed on every call
    with the CLI's bug (`TypeError: unsupported operand type(s) for +=: 'int' and 'NoneType'`)."""
    return c.az("vm", "boot-diagnostics", "get-boot-log", "-n", NODE)


def boot_log_uri(c: Cloud):
    """Where the node's serial console is kept: a blob of the run's account (boot diagnostics into
    it from the first power-on), read directly with the account's key (`console_blob`)."""
    return c.az("vm", "boot-diagnostics", "get-boot-log-uris", "-n", NODE, "--query", "serialConsoleLogBlobUri",
                "-o", "tsv")


def console_blob(c: Cloud, uri: str):
    """The (container, blob) of the console's `uri`, which must be in the run's account."""
    parts = urllib.parse.urlsplit((uri or "").strip())
    container, _, name = parts.path.lstrip("/").partition("/")
    if parts.scheme != "https" or parts.hostname != f"{c.account}.blob.core.windows.net" or not (container and name):
        raise ValueError(f"the console is not a blob of the run's account {c.account}: {uri!r}")
    return container, urllib.parse.unquote(name)


def node_vm_id(c: Cloud):
    """The node machine's `vmId`: the instance its tokens name (1B's `moruna: cloud azure instance`)."""
    return c.az("vm", "show", "-n", NODE, "--query", "vmId", "-o", "tsv")


def linux_vm(c: Cloud, ssh_public_key: str):
    return c.az("vm", "create", "-n", LINUX, "-l", c.location, "--image", c.linux_image, "--size", c.size,
                "--zone", c.zone, "--security-type", "Standard", "--accelerated-networking", "false",
                "--public-ip-sku", "Standard", "--nsg", f"{LINUX}-nsg", "--os-disk-size-gb", str(c.os_disk_gib),
                "--storage-sku", "Premium_LRS", "--admin-username", "bench", "--ssh-key-values", ssh_public_key,
                "-o", "none")


def image_version(c: Cloud):
    return c.az("vm", "show", "-n", LINUX, "--query", "storageProfile.imageReference.exactVersion", "-o", "tsv")


def run_command(c: Cloud, script: str):
    """`script` run as root on the Linux machine by the action run command (v1, RunShellScript);
    its output in the answer's `value[0].message` (Azure keeps the last 4,096 bytes).

    It is given no parameters, and so carries no value: the v1 handler (run-command-extension-linux,
    main/cmds.go writeTempScript) writes script.sh as the platform hands it, and the platform puts
    named parameters ahead of the script unquoted, `export N1=V1 N2=V2 ...` then `set -- V1 V2 ...`
    (measured on Ubuntu 24.04, 2026-11-18), run by /bin/sh: a value with `&` (every SAS) is cut
    there and lost, and every value sits in clear in script.sh. A value a script needs is read on
    the machine (a batch's env file), or given through `run_command_create`."""
    no_file_prefix(script)
    return c.az("vm", "run-command", "invoke", "-n", LINUX, "--command-id", "RunShellScript", "--scripts", script,
                "--query", "value[0].message", "-o", "tsv")


def run_command_create(c: Cloud, name: str, script: str, protected: dict, timeout_s: int = 600):
    """`script` run once as root on the Linux machine by the managed run command (v2) `name`; az
    returns when it has ended (`--async-execution false`). `protected` maps each variable's name
    to a local file holding its value (mode 600); no plain `--parameters`. Measured by the PM on a
    real machine (2026-11-18): script.sh holds the script alone, each value arrives whole as an
    environment variable. az names the command by `--run-command-name` (it takes no `-n` here).

    - No value is on a command line: `NAME=@FILE` is read by az itself (azure-cli's
      `_expand_file_prefixed_files`: the file's text, its last line end stripped), and sent as a
      protected parameter: encrypted to the machine, never returned by the API, out of the run
      command's visible settings.
    - No shell reads a value: the v2 handler (run-command-handler-linux, internal/exec/exec.go
      `SetEnvironmentVariables`) sets each named parameter with `os.Setenv` in the environment the
      script inherits, so `/`, `+`, `=`, `&`, `%` and `?` arrive as given. It saves the script as
      script.sh verbatim (internal/files `SaveScriptFile`) and runs `/bin/bash -c <script.sh>`.
    Its output is read with `run_command_view`."""
    no_file_prefix(script)
    args = ["vm", "run-command", "create", "--run-command-name", name, "--vm-name", LINUX, "--location", c.location,
            "--script", script, "--async-execution", "false", "--timeout-in-seconds", str(timeout_s)]
    if protected:
        args += ["--protected-parameters", *(f"{k}=@{path}" for k, path in protected.items())]
    return c.az(*args, "-o", "none")


def run_command_view(c: Cloud, name: str):
    """The managed run command's instance view, as JSON: `executionState`, `exitCode`, `output`
    and `error` (the script's stdout and stderr)."""
    return c.az("vm", "run-command", "show", "--run-command-name", name, "--vm-name", LINUX, "--instance-view",
                "--query", "instanceView", "-o", "json")


def run_command_delete(c: Cloud, name: str):
    """The managed run command removed from the machine's model once read, so it does not linger
    there (what it started, a systemd unit, is not its child and goes on)."""
    return c.az("vm", "run-command", "delete", "--run-command-name", name, "--vm-name", LINUX, "--yes")


def no_file_prefix(script: str) -> None:
    """az reads any argument that starts with `@`, or whose text after its first `=` does, as a
    file's name (azure-cli core, `_expand_file_prefixed_files`): a script must not look like one."""
    _, eq, rest = script.partition("=")
    if script.startswith("@") or (eq and rest.startswith("@")):
        raise ValueError(f"az would read this script as a file's name: {script[:80]!r}")


def group_delete(c: Cloud):
    return ["az", "group", "delete", "-n", c.group, "--subscription", c.subscription, "--yes", "-o", "none"]


def group_exists(c: Cloud):
    return ["az", "group", "exists", "-n", c.group, "--subscription", c.subscription]


def guard(c: Cloud, cmd) -> None:
    """Refuses an `az` command that could act outside the run: every one names subscription
    griot-dev, and every one that names a resource group names the run's. Refuses too any value
    given to a run command in the clear: v1's parameters (written unquoted into its script, see
    run_command) and v2's plain ones (kept in the machine's visible model)."""
    if not cmd or cmd[0] != "az":
        return
    if cmd[1:3] == ["vm", "run-command"] and "--parameters" in cmd:
        raise ValueError(f"refused, a run command's value in the clear: {cmd[3]} --parameters")
    if "--subscription" in cmd:
        if cmd[cmd.index("--subscription") + 1] != SUBSCRIPTION:
            raise ValueError(f"refused, not subscription {SUBSCRIPTION}: {shlex.join(cmd)}")
    elif cmd[1] != "storage" or "--account-name" not in cmd or cmd[cmd.index("--account-name") + 1] != c.account:
        raise ValueError(f"refused, no subscription and not the run's storage account: {shlex.join(cmd)}")
    for flag in ("-g", "--resource-group"):
        if flag in cmd and cmd[cmd.index(flag) + 1] != c.group:
            raise ValueError(f"refused, not the run's group {c.group}: {shlex.join(cmd)}")
    if cmd[1] == "group" and cmd[2] in ("create", "delete") and cmd[cmd.index("-n") + 1] != c.group:
        raise ValueError(f"refused, not the run's group {c.group}: {shlex.join(cmd)}")


def redact(cmd, secrets):
    """`cmd` as text, each secret replaced by its name."""
    text = shlex.join(cmd)
    for name, value in secrets.items():
        if value:
            text = text.replace(value, f"<{name}>")
    return text
