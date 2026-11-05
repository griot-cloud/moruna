//! An attached image's record (SDD 30 f.12): what the launcher that attached an image's
//! layers as disks writes beside them, so that the engine on the Moruna kernel, which holds
//! no layout, can check that the layers it runs on are the image its entry names.
//!
//! The record holds the image's manifest and config byte for byte, and for each of the
//! manifest's layers, lowest first, its digest and the seal of the layer file it was converted
//! into (the SHA-256 of the file's header page, which carries the root every page the image
//! filesystem serves is checked against; `moruna_imagefs::format::Header::seal`).
//! [`Record::verify`] makes every check that needs nothing but the record and the seals the
//! image filesystem says it mounted:
//!
//! 1. the entry's reference is a digest, and the manifest's bytes hash to it;
//! 2. the config's bytes hash to the digest the manifest gives it;
//! 3. the record's layers are the manifest's, in order;
//! 4. the layers served are the record's, in order, seal for seal.
//!
//! What it cannot check is the conversion: that a layer file holds what its OCI layer held.
//! That is the launcher's, which converted it; the engine trusts the launcher's record of it
//! as a Linux node trusts its own unpacked cache (SDD 19 IM-I1).

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::error::{Error, Result};
use crate::source::{Config, Manifest};

/// The record's format.
pub const VERSION: u32 = 1;

/// One of the image's layers: its OCI digest, and the seal of the layer file made from it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layer {
    /// The OCI layer's digest, as the manifest names it.
    pub digest: Digest,
    /// The layer file's seal, 64 lowercase hex digits.
    pub seal: String,
}

/// An attached image's record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// [`VERSION`].
    pub moruna_image_record: u32,
    /// The manifest, byte for byte (a manifest is JSON, so UTF-8).
    pub manifest: String,
    /// The config, byte for byte.
    pub config: String,
    /// The image's layers, lowest first.
    pub layers: Vec<Layer>,
}

/// An image whose identity [`Record::verify`] checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    /// The manifest's digest: the reference.
    pub digest: Digest,
    /// Its config, as a run reads it.
    pub config: Config,
}

fn hex(seal: &[u8; 32]) -> String {
    seal.iter().map(|b| format!("{b:02x}")).collect()
}

fn refuse(why: String) -> Error {
    Error::Format(why)
}

impl Record {
    /// The record of the image whose manifest and config are these bytes, with `seals` the
    /// seals of its layers' files, lowest first. Refused when the bytes are not UTF-8, the
    /// config is not the manifest's, or there is not one seal for each layer.
    pub fn new(manifest: &[u8], config: &[u8], seals: &[[u8; 32]]) -> Result<Record> {
        let parsed = Manifest::parse(Digest::of(manifest), manifest)?;
        let config_digest = Digest::of(config);
        if config_digest != parsed.config.digest {
            return Err(refuse(format!(
                "the config hashes to {config_digest}, not the {} the manifest names",
                parsed.config.digest
            )));
        }
        if seals.len() != parsed.layers.len() {
            return Err(refuse(format!(
                "{} layer files for a manifest of {} layers",
                seals.len(),
                parsed.layers.len()
            )));
        }
        let text = |b: &[u8], what: &str| {
            String::from_utf8(b.to_vec())
                .map_err(|_| refuse(format!("the {what} is not UTF-8, so not JSON")))
        };
        Ok(Record {
            moruna_image_record: VERSION,
            manifest: text(manifest, "manifest")?,
            config: text(config, "config")?,
            layers: parsed
                .layers
                .iter()
                .zip(seals)
                .map(|(l, s)| Layer {
                    digest: l.digest.clone(),
                    seal: hex(s),
                })
                .collect(),
        })
    }

    /// The record's bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a record of strings serializes")
    }

    /// The record in `bytes`.
    pub fn parse(bytes: &[u8]) -> Result<Record> {
        let record: Record = serde_json::from_slice(bytes)
            .map_err(|e| refuse(format!("not an image record: {e}")))?;
        if record.moruna_image_record != VERSION {
            return Err(refuse(format!(
                "an image record of version {}; this build reads {VERSION}",
                record.moruna_image_record
            )));
        }
        Ok(record)
    }

    /// The image `name` names (a build key or a registry reference, SDD 19 d.2), checked
    /// against this record and the seals of the layers served: the attached manifest is the one
    /// the name names (its digest, or the build key its annotation carries), then every check
    /// of [`Record::verify`].
    pub fn verify_name(&self, name: &crate::ImageName, served: &[[u8; 32]]) -> Result<Verified> {
        let digest = Digest::of(self.manifest.as_bytes());
        let manifest = Manifest::parse(digest.clone(), self.manifest.as_bytes())?;
        name.check(&manifest)?;
        self.verify(&digest.to_string(), served)
    }

    /// The image `reference` names, checked against this record and the seals of the layers
    /// served, lowest first (the four checks of the module's text). Every refusal names what
    /// was expected and what was found.
    pub fn verify(&self, reference: &str, served: &[[u8; 32]]) -> Result<Verified> {
        let named: Digest = reference.parse().map_err(|_| {
            refuse(format!(
                "`{reference}` is a name: on the Moruna kernel an image is named by its \
                 manifest's digest (sha256:...), which the layers attached are checked against"
            ))
        })?;
        let attached = Digest::of(self.manifest.as_bytes());
        if attached != named {
            return Err(refuse(format!(
                "the entry names the image {named}, and the image attached is {attached}"
            )));
        }
        let manifest = Manifest::parse(attached, self.manifest.as_bytes())?;
        let config = Digest::of(self.config.as_bytes());
        if config != manifest.config.digest {
            return Err(refuse(format!(
                "the image's manifest names the config {}, and the config attached hashes to \
                 {config}",
                manifest.config.digest
            )));
        }
        let listed: Vec<&Digest> = manifest.layers.iter().map(|l| &l.digest).collect();
        let recorded: Vec<&Digest> = self.layers.iter().map(|l| &l.digest).collect();
        if listed != recorded {
            return Err(refuse(format!(
                "the manifest names the layers {}, and the record attached {}",
                join(&listed),
                join(&recorded)
            )));
        }
        if served.len() != self.layers.len() {
            return Err(refuse(format!(
                "the image {named} has {} layers, and its image filesystem serves {}",
                self.layers.len(),
                served.len()
            )));
        }
        for (i, (layer, seal)) in self.layers.iter().zip(served).enumerate() {
            let seal = hex(seal);
            if layer.seal != seal {
                return Err(refuse(format!(
                    "layer {i} of the image {named} ({}) is the layer file sealed {}, and the \
                     image filesystem serves one sealed {seal}",
                    layer.digest, layer.seal
                )));
            }
        }
        Ok(Verified {
            digest: named,
            config: Config::parse(self.config.as_bytes())?,
        })
    }
}

fn join(digests: &[&Digest]) -> String {
    if digests.is_empty() {
        return "none".to_string();
    }
    digests
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> (Vec<u8>, Vec<u8>, Vec<[u8; 32]>) {
        let config = br#"{"architecture":"amd64","os":"linux","config":{"Cmd":["/usr/local/bin/python3","/kd2/transform.py"],"Env":["PATH=/usr/local/bin:/usr/bin","LANG=C.UTF-8"]},"rootfs":{"type":"layers","diff_ids":[]}}"#.to_vec();
        let layer_a = Digest::of(b"layer a");
        let layer_b = Digest::of(b"layer b");
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{layer_a}","size":7}},{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{layer_b}","size":7}}]}}"#,
            Digest::of(&config),
            config.len()
        )
        .into_bytes();
        (manifest, config, vec![[1u8; 32], [2u8; 32]])
    }

    #[test]
    fn kd2_t13_an_image_named_by_its_digest_on_its_own_layers_is_verified() {
        let (manifest, config, seals) = image();
        let record =
            Record::parse(&Record::new(&manifest, &config, &seals).unwrap().to_bytes()).unwrap();
        let reference = Digest::of(&manifest).to_string();
        let verified = record.verify(&reference, &seals).unwrap();
        assert_eq!(verified.digest.to_string(), reference);
        assert_eq!(
            verified.config.command(),
            ["/usr/local/bin/python3", "/kd2/transform.py"]
        );
        assert_eq!(verified.config.env[1], "LANG=C.UTF-8");
    }

    #[test]
    fn kd2_t13_every_mismatch_is_refused_naming_both_sides() {
        let (manifest, config, seals) = image();
        let record = Record::new(&manifest, &config, &seals).unwrap();
        let reference = Digest::of(&manifest).to_string();
        let attached = reference.clone();

        let other = Digest::of(b"an image nobody attached").to_string();
        let e = record.verify(&other, &seals).unwrap_err().to_string();
        assert!(
            e.contains(&other) && e.contains(&attached),
            "names both digests: {e}"
        );

        let e = record
            .verify("python:3.14-slim", &seals)
            .unwrap_err()
            .to_string();
        assert!(e.contains("is a name"), "{e}");

        let e = record
            .verify(&reference, &[[1u8; 32], [9u8; 32]])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("layer 1") && e.contains(&"02".repeat(32)) && e.contains(&"09".repeat(32)),
            "names both seals: {e}"
        );

        let e = record
            .verify(&reference, &seals[..1])
            .unwrap_err()
            .to_string();
        assert!(e.contains("has 2 layers") && e.contains("serves 1"), "{e}");

        let mut forged = record.clone();
        forged.config = forged.config.replace("transform.py", "steal.py");
        let e = forged.verify(&reference, &seals).unwrap_err().to_string();
        assert!(e.contains("the config attached hashes to"), "{e}");

        let mut swapped = record.clone();
        swapped.layers.swap(0, 1);
        let e = swapped.verify(&reference, &seals).unwrap_err().to_string();
        assert!(e.contains("the record attached"), "{e}");

        assert!(
            Record::new(&manifest, b"{}", &seals).is_err(),
            "not its config"
        );
        assert!(
            Record::new(&manifest, &config, &seals[..1]).is_err(),
            "a seal short"
        );
        let mut future = record.clone();
        future.moruna_image_record = 2;
        assert!(Record::parse(&future.to_bytes()).is_err());
    }
}
