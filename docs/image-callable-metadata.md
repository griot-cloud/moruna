# Callable metadata in OCI images

Moruna builds embed passive, versioned callable metadata in standard OCI image
configuration labels. This supports Moruna and consumers such as Griot without
running the image or importing its source to discover its interface.

## Version 1

`config.Labels["org.moruna.callable.version"]` is the string `"1"`.
`config.Labels["org.moruna.callable"]` is canonical JSON with these fields:

| Field | Meaning |
|---|---|
| `schema_version` | Integer 1 |
| `entry` | The callable's module and entry name |
| `build_key` | The existing Moruna build key |
| `platform` | OCI `os` and `architecture` |
| `definition` | The same declaration included in MJS: name, source/function/sink role, parameter schema, input/output schemas, requirements and provenance |

The manifest carries `org.moruna.callable.version` for discovery. The full
configuration label is authoritative; consumers fetch the config descriptor by
its digest and validate its bytes. The declaration's provenance build key must
match the descriptor's build key. The existing manifest annotations still carry
entry, build key, lock digest and source provenance.

No task-instance argument values, credentials, Griot catalog/domain identifiers
or execution-server addresses are added. The descriptor does not include its own
image digest: the enclosing image manifest already pins the config, and including
its own digest would create a content-addressing cycle. The JSON label is bounded
to 128 KiB. Consumers should distinguish absent metadata from malformed or
unsupported-version metadata.

## Behavior and compatibility

Labels and annotations do not alter the existing command, environment, working
directory, filesystem layers or execution protocol. Other OCI consumers can
ignore these namespaced fields. This does not make another engine implement the
Moruna callable protocol automatically; it preserves ordinary OCI execution
behavior while allowing interested tools to discover the interface.

Adding/changing metadata changes configuration/manifest digests. Moruna never
mutates an image already published under a digest. Newly built images and MJS
references use their new digest. The build cache compares the desired descriptor
so an older image without metadata is not silently reused as a described build.
Repeated builds with unchanged code and metadata reuse the same image.

Metadata describes a callable; it is not a signature or execution permission.
Consumers still validate the full declaration, selected platform, runtime ABI,
image digest and applicable access policy. Registry credentials remain supplied
separately. Image-backed Griot function registration stores a reference and the
verified descriptor; it does not require a Griot-specific OCI registry.

The implementation lives in `python/moruna/_image_metadata.py`, used by the
builder. The metadata is additive to the OCI config; no MJS format change is
required. The SDK adds this metadata in 0.6.5. Existing 0.6.4 runtimes can still execute
the resulting images; the OCI labels do not change the execution protocol.

OCI references: https://github.com/opencontainers/image-spec/blob/main/annotations.md
and https://github.com/opencontainers/image-spec/blob/main/config.md.

Registry uploads can refresh pull-only bearer tokens when the registry asks for
push scope. File uploads stream from disk and allow a longer transfer timeout
than metadata reads, avoiding whole-image memory copies and short upload limits.
