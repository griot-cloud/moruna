//! An image derived from another (SDD 30 k): its layers with one more on top made of a
//! directory's tree, and its config's command replaced, written as an OCI layout of one image.
//! It is what a `FROM` image, a `COPY` of the directory to `/` and a `CMD` would build, made
//! without a container runtime and the same bytes every time: the added layer is an
//! uncompressed tar whose entries are in name order, owned by root, dated 0.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde_json::{Value, json};

use crate::digest::{Digest, Hashing};
use crate::error::{Error, Result};
use crate::source::{ImageSource, REF_NAME};
use crate::unpack::blob_bytes;

/// The media type of the added layer.
const LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
/// The largest manifest read.
const MANIFEST_MAX: u64 = 4 << 20;

fn io_at(what: String) -> impl Fn(io::Error) -> Error {
    move |e| Error::io(what.clone(), e)
}

/// The bytes of the manifest `reference` names, checked against its digest.
pub fn manifest_bytes(source: &dyn ImageSource, reference: &str) -> Result<(Digest, Vec<u8>)> {
    let manifest = source.manifest(reference)?;
    let mut bytes = Vec::new();
    source
        .blob(&manifest.digest)?
        .take(MANIFEST_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(io_at(format!("{}: reading the manifest", manifest.digest)))?;
    if Digest::of(&bytes) != manifest.digest {
        return Err(Error::Digest {
            digest: manifest.digest.to_string(),
            actual: Digest::of(&bytes).to_string(),
        });
    }
    Ok((manifest.digest, bytes))
}

/// The config of the image whose manifest is `manifest` (its digest `digest`), checked against
/// the digest the manifest gives it.
pub fn config_bytes(source: &dyn ImageSource, digest: &Digest, manifest: &[u8]) -> Result<Vec<u8>> {
    let parsed = crate::source::Manifest::parse(digest.clone(), manifest)?;
    blob_bytes(source, &parsed.config)
}

/// The tar of `dir`'s tree, as the added layer.
fn layer_tar(dir: &Path) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    append(&mut builder, dir, "")?;
    builder
        .into_inner()
        .map_err(io_at(format!("finishing the tar of {}", dir.display())))
}

fn header(kind: tar::EntryType, mode: u32, size: u64) -> tar::Header {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(kind);
    h.set_mode(mode);
    h.set_size(size);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h
}

fn append(builder: &mut tar::Builder<Vec<u8>>, dir: &Path, prefix: &str) -> Result<()> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .map_err(io_at(format!("listing {}", dir.display())))?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<io::Result<_>>()
        .map_err(io_at(format!("listing {}", dir.display())))?;
    names.sort();
    for name in names {
        let path = dir.join(&name);
        let name = name.to_str().ok_or_else(|| {
            Error::Format(format!("{}: a name that is not UTF-8", path.display()))
        })?;
        let at = format!("{prefix}{name}");
        let meta =
            fs::symlink_metadata(&path).map_err(io_at(format!("reading {}", path.display())))?;
        let mode = meta.permissions().mode() & 0o777;
        let added = if meta.file_type().is_symlink() {
            let target =
                fs::read_link(&path).map_err(io_at(format!("reading {}", path.display())))?;
            let mut h = header(tar::EntryType::Symlink, 0o777, 0);
            builder.append_link(&mut h, &at, &target)
        } else if meta.is_dir() {
            let mut h = header(tar::EntryType::Directory, mode, 0);
            builder
                .append_data(&mut h, format!("{at}/"), io::empty())
                .map_err(io_at(format!("adding {} to the layer", path.display())))?;
            append(builder, &path, &format!("{at}/"))?;
            continue;
        } else if meta.is_file() {
            let mut h = header(tar::EntryType::Regular, mode, meta.len());
            let file = File::open(&path).map_err(io_at(format!("opening {}", path.display())))?;
            builder.append_data(&mut h, &at, file)
        } else {
            return Err(Error::Format(format!(
                "{} is not a file, a directory or a link",
                path.display()
            )));
        };
        added.map_err(io_at(format!("adding {} to the layer", path.display())))?;
    }
    Ok(())
}

fn write_blob(out: &Path, bytes: &[u8]) -> Result<(Digest, u64)> {
    let digest = Digest::of(bytes);
    let path = out.join("blobs").join("sha256").join(digest.hex());
    fs::write(&path, bytes).map_err(io_at(format!("writing {}", path.display())))?;
    Ok((digest, bytes.len() as u64))
}

/// Copies the blob `digest` of `source` into the layout at `out`, checking its digest.
fn copy_blob(source: &dyn ImageSource, digest: &Digest, out: &Path) -> Result<()> {
    let path = out.join("blobs").join("sha256").join(digest.hex());
    let mut file = File::create(&path).map_err(io_at(format!("creating {}", path.display())))?;
    let mut reader = Hashing::new(source.blob(digest)?);
    io::copy(&mut reader, &mut file).map_err(io_at(format!("copying {digest}")))?;
    file.flush()
        .map_err(io_at(format!("writing {}", path.display())))?;
    let (actual, _) = reader
        .finish()
        .map_err(io_at(format!("reading {digest}")))?;
    if &actual != digest {
        return Err(Error::Digest {
            digest: digest.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(())
}

/// Writes at `out` (made afresh) the OCI layout of the image `reference` of `source` with the
/// tree of `dir` as one more layer on top and `cmd` as its config's `Cmd`, named `name` in its
/// `index.json`. Returns the new image's manifest digest.
pub fn derive(
    source: &dyn ImageSource,
    reference: &str,
    dir: &Path,
    cmd: &[String],
    name: &str,
    out: &Path,
) -> Result<Digest> {
    let (base_digest, base_bytes) = manifest_bytes(source, reference)?;
    let base = crate::source::Manifest::parse(base_digest.clone(), &base_bytes)?;
    let mut manifest: Value = serde_json::from_slice(&base_bytes)
        .map_err(|e| Error::Format(format!("{base_digest}: the manifest: {e}")))?;
    let mut config: Value = serde_json::from_slice(&blob_bytes(source, &base.config)?)
        .map_err(|e| Error::Format(format!("{}: the config: {e}", base.config.digest)))?;

    let _ = fs::remove_dir_all(out);
    fs::create_dir_all(out.join("blobs").join("sha256"))
        .map_err(io_at(format!("making {}", out.display())))?;
    for layer in &base.layers {
        copy_blob(source, &layer.digest, out)?;
    }
    let (layer, layer_size) = write_blob(out, &layer_tar(dir)?)?;

    let diff_ids = config
        .pointer_mut("/rootfs/diff_ids")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            Error::Format(format!(
                "{}: the config has no rootfs.diff_ids",
                base.config.digest
            ))
        })?;
    diff_ids.push(json!(layer.to_string()));
    let container = config
        .as_object_mut()
        .ok_or_else(|| Error::Format("the config is not an object".into()))?
        .entry("config")
        .or_insert_with(|| json!({}));
    container["Cmd"] = json!(cmd);
    if let Some(history) = config.get_mut("history").and_then(Value::as_array_mut) {
        history
            .push(json!({"created": "1970-01-01T00:00:00Z", "created_by": "moruna-layer derive"}));
    }
    let config_bytes = serde_json::to_vec(&config).map_err(|e| Error::Format(e.to_string()))?;
    let (config_digest, config_size) = write_blob(out, &config_bytes)?;

    manifest["config"]["digest"] = json!(config_digest.to_string());
    manifest["config"]["size"] = json!(config_size);
    manifest["layers"]
        .as_array_mut()
        .ok_or_else(|| Error::Format(format!("{base_digest}: the manifest has no layers")))?
        .push(json!({"mediaType": LAYER_TAR, "digest": layer.to_string(), "size": layer_size}));
    let manifest_bytes = serde_json::to_vec(&manifest).map_err(|e| Error::Format(e.to_string()))?;
    let (digest, size) = write_blob(out, &manifest_bytes)?;
    let media_type = manifest
        .get("mediaType")
        .and_then(Value::as_str)
        .unwrap_or("application/vnd.oci.image.manifest.v1+json");
    let index = json!({
        "schemaVersion": 2,
        "manifests": [{"mediaType": media_type, "digest": digest.to_string(), "size": size,
                       "annotations": {REF_NAME: name}}],
    });
    let write = |file: &str, value: &Value| {
        let path = out.join(file);
        fs::write(&path, value.to_string()).map_err(io_at(format!("writing {}", path.display())))
    };
    write("oci-layout", &json!({"imageLayoutVersion": "1.0.0"}))?;
    write("index.json", &index)?;
    Ok(digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OciLayout;
    use crate::testing::{Entry, LayoutBuilder, Scratch, media};

    #[test]
    fn kd2_t14_a_derived_image_adds_a_layer_and_its_command_the_same_bytes_every_time() {
        let scratch = Scratch::new("derive");
        let base = scratch.path().join("base");
        LayoutBuilder::new(&base)
            .config_document(json!({"config": {"Cmd": ["sh"], "Env": ["PATH=/bin"]},
                                    "rootfs": {"type": "layers", "diff_ids": []}}))
            .layer(media::TAR, &[Entry::file("tool", b"tool")])
            .finish(Some("base"));
        let dir = scratch.path().join("programs");
        fs::create_dir_all(dir.join("kd2")).unwrap();
        fs::write(dir.join("kd2/transform.py"), b"print(1)\n").unwrap();
        let source = OciLayout::open(&base).unwrap();
        let cmd = vec!["python3".to_string(), "/kd2/transform.py".to_string()];
        let a = derive(
            &source,
            "base",
            &dir,
            &cmd,
            "kd2",
            &scratch.path().join("a"),
        )
        .unwrap();
        let b = derive(
            &source,
            "base",
            &dir,
            &cmd,
            "kd2",
            &scratch.path().join("b"),
        )
        .unwrap();
        assert_eq!(a, b, "the same tree and command derive the same image");

        let derived = OciLayout::open(scratch.path().join("a")).unwrap();
        let manifest = derived.manifest("kd2").unwrap();
        assert_eq!(manifest.digest, a);
        let base_manifest = source.manifest("base").unwrap();
        assert_eq!(manifest.layers.len(), base_manifest.layers.len() + 1);
        assert_eq!(
            manifest.layers[..base_manifest.layers.len()],
            base_manifest.layers[..]
        );
        let config =
            crate::Config::parse(&blob_bytes(&derived, &manifest.config).unwrap()).unwrap();
        assert_eq!(config.cmd, cmd);

        let image = crate::unpack(&derived, &a.to_string(), &scratch.path().join("cache")).unwrap();
        assert_eq!(
            fs::read(image.rootfs.join("kd2/transform.py")).unwrap(),
            b"print(1)\n"
        );
        assert_eq!(fs::read(image.rootfs.join("tool")).unwrap(), b"tool");
        assert_eq!(config.env, ["PATH=/bin"], "the base's environment is kept");
    }

    /// What the launcher writes and what the image filesystem reports agree: the seal of each
    /// converted layer file's first page is the seal of the header the server mounts.
    #[test]
    fn kd2_t14_the_record_of_a_converted_image_verifies_against_its_mounted_headers() {
        let scratch = Scratch::new("derive-record");
        let base = scratch.path().join("base");
        LayoutBuilder::new(&base)
            .config_document(json!({"config": {"Cmd": ["sh"]},
                                    "rootfs": {"type": "layers", "diff_ids": []}}))
            .layer(media::TAR, &[Entry::file("tool", b"tool")])
            .finish(Some("base"));
        let dir = scratch.path().join("programs");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("x"), b"x").unwrap();
        let source = OciLayout::open(&base).unwrap();
        let out = scratch.path().join("image");
        let digest = derive(&source, "base", &dir, &["/x".to_string()], "kd2", &out).unwrap();
        let derived = OciLayout::open(&out).unwrap();
        let converted = crate::layerfile::convert(
            &derived,
            &digest.to_string(),
            &scratch.path().join("layers"),
        )
        .unwrap();
        let seals: Vec<[u8; 32]> = converted
            .layers
            .iter()
            .map(|l| {
                let page = &fs::read(&l.path).unwrap()[..moruna_imagefs::format::PAGE];
                let (header, _) = moruna_imagefs::format::Header::parse(page).unwrap();
                assert_eq!(header.seal().unwrap(), moruna_imagefs::sha256::digest(page));
                assert_eq!(
                    l.id.bytes(),
                    header.seal().unwrap(),
                    "a layer's id is its seal"
                );
                header.seal().unwrap()
            })
            .collect();
        let (_, manifest) = manifest_bytes(&derived, &digest.to_string()).unwrap();
        let config = config_bytes(&derived, &digest, &manifest).unwrap();
        let record = crate::record::Record::new(&manifest, &config, &seals).unwrap();
        let verified = record.verify(&digest.to_string(), &seals).unwrap();
        assert_eq!(verified.config.cmd, ["/x"]);
        assert_eq!(verified.digest, digest);
    }
}
