"""The run's Blob container, as each engine's own reader and writer names it (system design 6A,
the PM's answers of 2026-11-18): every engine reads its inputs from and writes its output to the
same container, inside operational time.

A store is an account, its key, a container and, for a local emulator (Azurite), an endpoint;
it travels to a run in the environment (`BENCH_ACCOUNT`, `BENCH_KEY`, `BENCH_CONTAINER`,
`BENCH_ENDPOINT`). Each engine is given it in its own reader's terms:

| Engine | Reads and writes with | Names a blob as |
| --- | --- | --- |
| 0.6.6 (both functions) | its object store (`storage=`, object_store) | `az://CONTAINER/NAME` |
| DuckDB | its `azure` extension, a secret from the connection string | `az://CONTAINER/NAME` |
| Polars | its object store (`storage_options`) | `az://CONTAINER/NAME` |
| Dask | adlfs (`storage_options`) | `abfs://CONTAINER/NAME` |
| Ray | pyarrow's `AzureFileSystem` | `CONTAINER/NAME` |
| Spark | hadoop-azure (`wasbs`), the account key in the Hadoop configuration | `wasbs://CONTAINER@ACCOUNT.blob.core.windows.net/NAME` |

The Python function's output, for the engines that run it batch by batch (DuckDB, Polars), is
written by pyarrow's Parquet writer, as bench/vm's runner wrote it, onto pyarrow's
`AzureFileSystem`. The Rust pool has no Blob reader: it gets a local copy, timed apart.

A store may hold a container SAS in place of the key (work item 3.8b: the proof on Servercore,
whose data is in nyc-blob.sh's Blob container, read and written with that container's SAS; the
account key stays with nyc-blob.sh). Such a store only downloads (`download`), by Blob's REST API
on the standard library: List Blobs and Get Blob, each with the SAS as its query, which a
container SAS with read and list allows.
"""
import dataclasses
import http.client
import os
import shutil
import time
import urllib.parse
import urllib.request
from xml.etree import ElementTree

# Azurite's well-known development account (not a secret: it is in Azurite's documentation).
AZURITE_ACCOUNT = "devstoreaccount1"
AZURITE_KEY = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="


@dataclasses.dataclass(frozen=True)
class Store:
    account: str
    key: str
    container: str
    endpoint: str = ""      # http://127.0.0.1:10000/devstoreaccount1 for Azurite; "" is Azure
    sas: str = ""           # a container SAS (Azure's query string, without its `?`) in place of the key

    @classmethod
    def from_env(cls, env=None):
        env = os.environ if env is None else env
        if not env.get("BENCH_ACCOUNT"):
            return None
        return cls(env["BENCH_ACCOUNT"], env["BENCH_KEY"], env["BENCH_CONTAINER"], env.get("BENCH_ENDPOINT", ""))

    def env(self):
        out = {"BENCH_ACCOUNT": self.account, "BENCH_KEY": self.key, "BENCH_CONTAINER": self.container}
        if self.endpoint:
            out["BENCH_ENDPOINT"] = self.endpoint
        return out

    @property
    def blob_endpoint(self):
        return self.endpoint or f"https://{self.account}.blob.core.windows.net"

    def connection_string(self):
        return (f"DefaultEndpointsProtocol={urllib.parse.urlsplit(self.blob_endpoint).scheme};"
                f"AccountName={self.account};AccountKey={self.key};BlobEndpoint={self.blob_endpoint};")

    # The engines' names and options ---------------------------------------------------------
    def az(self, name):
        return f"az://{self.container}/{name}"

    def moruna_storage(self):
        """0.6.6's `storage=`: its object store reads `az://`; an emulator is named in object_store's
        environment (`moruna_env`)."""
        s = {"account": self.account, "access_key": self.key}
        if self.endpoint.startswith("http://"):
            s["allow_http"] = True
        return s

    def moruna_env(self):
        return {"AZURE_STORAGE_USE_EMULATOR": "true"} if self.endpoint else {}

    def polars_options(self):
        o = {"account_name": self.account, "account_key": self.key}
        if self.endpoint:
            o.update(use_emulator="true", allow_http="true")
        return o

    def adlfs_options(self):
        if self.endpoint:
            return {"connection_string": self.connection_string()}
        return {"account_name": self.account, "account_key": self.key}

    def abfs(self, name):
        return f"abfs://{self.container}/{name}"

    def duckdb_secret(self):
        cs = self.connection_string().replace("'", "''")
        return f"CREATE OR REPLACE SECRET bench (TYPE azure, CONNECTION_STRING '{cs}')"

    def pyarrow_fs(self):
        import pyarrow.fs as pafs
        kw = {}
        if self.endpoint:
            u = urllib.parse.urlsplit(self.endpoint)
            kw = {"blob_storage_authority": u.netloc, "blob_storage_scheme": u.scheme}
        return pafs.AzureFileSystem(account_name=self.account, account_key=self.key, **kw)

    def path(self, name):
        """A blob as pyarrow's AzureFileSystem names it."""
        return f"{self.container}/{name}"

    def wasbs(self, name):
        return f"wasbs://{self.container}@{self.account}.blob.core.windows.net/{name}"

    def spark_conf(self):
        return {f"spark.hadoop.fs.azure.account.key.{self.account}.blob.core.windows.net": self.key}

    # The harness's own reads and writes (outside every timing) ---------------------------------
    def upload(self, local, name):
        import pyarrow.fs as pafs
        fs = self.pyarrow_fs()
        fs.create_dir(os.path.dirname(self.path(name)), recursive=True)
        pafs.copy_files(local, self.path(name), destination_filesystem=fs)

    def download(self, prefix, dest):
        """Every blob under `prefix` into `dest`; the bytes copied."""
        if self.sas:
            return self.sas_download(prefix, dest)
        import pyarrow.fs as pafs
        fs = self.pyarrow_fs()
        total = 0
        base = self.path(prefix).rstrip("/")
        single = fs.get_file_info(base)
        if single.type == pafs.FileType.File:
            infos = [single]
        else:
            infos = fs.get_file_info(pafs.FileSelector(base, recursive=True, allow_not_found=True))
        for info in infos:
            if info.type != pafs.FileType.File:
                continue
            rel = os.path.basename(info.path) if info.path == base else os.path.relpath(info.path, base)
            dst = os.path.join(dest, rel)
            os.makedirs(os.path.dirname(dst) or ".", exist_ok=True)
            pafs.copy_files(info.path, dst, source_filesystem=fs)
            total += info.size or 0
        return total

    # A store with a SAS: Blob's REST API, the SAS as each request's query -----------------------
    def sas_url(self, path, query=""):
        """`path` in the container, with `query` and the SAS; the SAS is the last of the query."""
        url = f"{self.blob_endpoint}/{self.container}" + (f"/{urllib.parse.quote(path)}" if path else "")
        return f"{url}?{query + '&' if query else ''}{self.sas.lstrip('?')}"

    def sas_names(self, prefix, opener=urllib.request.urlopen):
        """The names of the blobs under `prefix` (List Blobs, page by page)."""
        names, marker = [], ""
        while True:
            query = f"restype=container&comp=list&prefix={urllib.parse.quote(prefix, safe='')}"
            if marker:
                query += f"&marker={urllib.parse.quote(marker, safe='')}"
            with opener(self.sas_url("", query), timeout=120) as r:
                body = r.read()
            root = ElementTree.fromstring(body.removeprefix(b"\xef\xbb\xbf"))
            names += [b.findtext("Name") for b in root.iter("Blob") if b.findtext("Name")]
            marker = root.findtext("NextMarker") or ""
            if not marker:
                return names

    def sas_download(self, prefix, dest, opener=urllib.request.urlopen, tries=3, wait_s=5.0):
        """`download` with the SAS: each blob under `prefix` (Get Blob), tried `tries` times."""
        total = 0
        base = prefix.rstrip("/")
        for name in self.sas_names(prefix, opener):
            if name.endswith("/"):
                continue
            rel = os.path.basename(name) if name == base else os.path.relpath(name, base)
            dst = os.path.join(dest, rel)
            os.makedirs(os.path.dirname(dst) or ".", exist_ok=True)
            for attempt in range(tries):
                try:
                    with opener(self.sas_url(name), timeout=120) as r, open(dst, "wb") as f:
                        shutil.copyfileobj(r, f, 1 << 20)
                        length, got = r.headers.get("Content-Length"), f.tell()
                    # A connection that ends early ends the copy too: the length tells.
                    if length is not None and int(length) != got:
                        raise OSError(f"{name}: {got} of its {length} bytes came")
                    break
                except (OSError, http.client.HTTPException):
                    if attempt == tries - 1:
                        raise
                    time.sleep(wait_s * (attempt + 1))
            total += os.path.getsize(dst)
        return total

    def create(self):
        """The container (a local emulator's; on Azure, azure.py's commands make it)."""
        self.pyarrow_fs().create_dir(self.container)
