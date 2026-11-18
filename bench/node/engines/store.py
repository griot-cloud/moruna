"""The run's Blob container, as each engine's own reader and writer names it (system design 6A,
the PM's answers of 2026-11-18): every engine reads its inputs from and writes its output to the
same container, inside operational time.

A store is an account, its key, a container and, for a local emulator (Azurite), an endpoint;
it travels to a run in the environment (`BENCH_ACCOUNT`, `BENCH_KEY`, `BENCH_CONTAINER`,
`BENCH_ENDPOINT`). Each engine is given it in its own reader's terms:

| Engine | Reads and writes with | Names a blob as |
| --- | --- | --- |
| 0.6.6 (both functions) | its object store (`storage=`, object_store) | `az://CONTAINER/NAME` |
| DuckDB | its `azure` extension, a secret from the connection string, its curl transport | `az://CONTAINER/NAME` |
| Polars | its object store (`storage_options`) | `az://CONTAINER/NAME` |
| Dask | adlfs (`storage_options`) | `abfs://CONTAINER/NAME` |
| Ray | pyarrow's `AzureFileSystem` | `CONTAINER/NAME` |
| Spark | hadoop-azure's ABFS driver over the Blob API, the account key in the Hadoop configuration | `abfss://CONTAINER@ACCOUNT.dfs.core.windows.net/NAME` |

The Python function's output, for the engines that run it batch by batch (DuckDB, Polars), is
written by pyarrow's Parquet writer, as bench/vm's runner wrote it, onto pyarrow's
`AzureFileSystem`. The Rust pool has no Blob reader: it gets a local copy, timed apart.
"""
import dataclasses
import os
import urllib.parse

# Azurite's well-known development account (not a secret: it is in Azurite's documentation).
AZURITE_ACCOUNT = "devstoreaccount1"
AZURITE_KEY = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="


@dataclasses.dataclass(frozen=True)
class Store:
    account: str
    key: str
    container: str
    endpoint: str = ""      # http://127.0.0.1:10000/devstoreaccount1 for Azurite; "" is Azure

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

    def abfss(self, name):
        """A blob as hadoop-azure's ABFS driver names it (`abfs://` over an emulator's plain HTTP):
        Hadoop 3.5, which pyspark 4.3 carries, refuses `wasbs://` ("WASB Driver using wasb(s)
        schema is no longer supported")."""
        scheme = "abfs" if self.endpoint.startswith("http://") else "abfss"
        return f"{scheme}://{self.container}@{self.account}.dfs.core.windows.net/{name}"

    def spark_conf(self):
        """The account key for ABFS, and the run's account as it is: no hierarchical namespace
        (azure.py makes a plain StorageV2 account), so ABFS speaks the Blob service's API
        (`fns.account.service.type` BLOB), the one an emulator serves too. An emulator is named
        by its address (`abfs.endpoint`), over its scheme."""
        conf = {f"spark.hadoop.fs.azure.account.key.{self.account}.dfs.core.windows.net": self.key,
                f"spark.hadoop.fs.azure.account.key.{self.account}.blob.core.windows.net": self.key,
                "spark.hadoop.fs.azure.account.hns.enabled": "false",
                "spark.hadoop.fs.azure.fns.account.service.type": "BLOB"}
        if self.endpoint:
            u = urllib.parse.urlsplit(self.endpoint)
            conf["spark.hadoop.fs.azure.abfs.endpoint"] = u.netloc
            conf["spark.hadoop.fs.azure.always.use.https"] = str(u.scheme == "https").lower()
        return conf

    # The harness's own reads and writes (outside every timing) ---------------------------------
    def upload(self, local, name):
        import pyarrow.fs as pafs
        fs = self.pyarrow_fs()
        fs.create_dir(os.path.dirname(self.path(name)), recursive=True)
        pafs.copy_files(local, self.path(name), destination_filesystem=fs)

    def download(self, prefix, dest):
        """Every blob under `prefix` into `dest`, or the one blob `prefix` names; the bytes copied.
        A blob at `prefix` with blobs under it is a directory's marker (ABFS over the Blob API
        leaves an empty one at Spark's output), not the output."""
        import pyarrow.fs as pafs
        fs = self.pyarrow_fs()
        total = 0
        base = self.path(prefix).rstrip("/")
        try:
            infos = fs.get_file_info(pafs.FileSelector(base, recursive=True, allow_not_found=True))
        except (NotADirectoryError, OSError):
            infos = []
        if not any(i.type == pafs.FileType.File for i in infos):
            single = fs.get_file_info(base)
            infos = [single] if single.type == pafs.FileType.File else []
        for info in infos:
            if info.type != pafs.FileType.File:
                continue
            rel = os.path.basename(info.path) if info.path == base else os.path.relpath(info.path, base)
            dst = os.path.join(dest, rel)
            os.makedirs(os.path.dirname(dst) or ".", exist_ok=True)
            pafs.copy_files(info.path, dst, source_filesystem=fs)
            total += info.size or 0
        return total

    def drop_markers(self, name):
        """The empty blobs at the prefixes above `name` (`out` above `out/x`), each deleted, the
        blobs under it kept; the ones deleted. They are directory markers: hadoop-azure's ABFS
        driver leaves one at each directory of Spark's output, and pyarrow's Azure filesystem takes
        them for files, so making a directory under one fails ("Not a directory: 'data/out'"), as
        Ray's writer and the Rust pool's upload do (found in work item 6.2c)."""
        import pyarrow.fs as pafs
        fs = self.pyarrow_fs()
        parts = name.strip("/").split("/")
        gone = []
        for i in range(1, len(parts)):
            path = self.path("/".join(parts[:i]))
            info = fs.get_file_info(path)
            if info.type == pafs.FileType.File and info.size == 0:
                fs.delete_file(path)
                gone.append(path)
        return gone

    def create(self):
        """The container (a local emulator's; on Azure, azure.py's commands make it)."""
        self.pyarrow_fs().create_dir(self.container)
