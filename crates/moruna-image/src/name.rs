//! What an `image` task names (SDD 19 d.2): an image Moruna built, by its build key, or any
//! image in an OCI registry, by its repository and its manifest's digest; and the table that
//! tells a run where each one is on this machine.
//!
//! A build key (`sha256:` and 64 hex digits) is known before the image is built: it is a hash
//! over everything the image is made of, and the image's manifest carries it as the annotation
//! [`BUILD_KEY`]. The runner's table maps it to the layout and the manifest the build wrote,
//! and the manifest's annotation is checked against it. A registry reference
//! (`HOST/REPOSITORY@sha256:...`) names the manifest itself; the runner pulls it into a layout
//! of its own and the table maps the reference to it. Either way the image a task runs is the
//! one its document names, checked byte for byte, or the run is refused naming it.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::error::{Error, Result};
use crate::source::{ImageSource, Manifest, OciLayout};

/// The manifest annotation an image Moruna built carries its build key in.
pub const BUILD_KEY: &str = "org.moruna.build.key";

/// What an `image` task names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageName {
    /// An image Moruna built, by its build key.
    Key(Digest),
    /// An image in a registry, by its manifest's digest.
    Registry {
        /// The registry's host, with its port when it has one.
        host: String,
        /// The repository in it.
        repository: String,
        /// The manifest's digest.
        digest: Digest,
    },
}

fn refuse(why: String) -> Error {
    Error::Format(why)
}

impl ImageName {
    /// `text` as an image's name: `sha256:<64 hex>`, a build key, or
    /// `HOST/REPOSITORY@sha256:<64 hex>`, where `HOST` has a `.` or a `:` or is `localhost`
    /// and `REPOSITORY` is one or more `/`-separated components of lowercase letters, digits,
    /// `.`, `_` and `-`. A tag is not part of a name: a name is exact.
    pub fn parse(text: &str) -> Result<ImageName> {
        if Digest::is_digest(text) {
            return Ok(ImageName::Key(text.parse()?));
        }
        let shape = || {
            refuse(format!(
                "`{text}` is not an image: name a build key (sha256:<64 hex>) or a registry \
                 reference HOST/REPOSITORY@sha256:<64 hex>"
            ))
        };
        let (path, digest) = text.split_once('@').ok_or_else(shape)?;
        let digest: Digest = digest.parse().map_err(|_| shape())?;
        let (host, repository) = path.split_once('/').ok_or_else(shape)?;
        let host_ok = !host.is_empty()
            && (host.contains('.') || host.contains(':') || host == "localhost")
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b));
        let repository_ok = !repository.is_empty()
            && repository.split('/').all(|part| {
                !part.is_empty()
                    && part.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)
                    })
            });
        if !host_ok {
            return Err(refuse(format!(
                "`{text}`: `{host}` is not a registry's host: a host names its registry with a \
                 `.` or a port, or is `localhost`"
            )));
        }
        if !repository_ok {
            return Err(refuse(format!(
                "`{text}`: `{repository}` is not a repository: lowercase letters, digits, `.`, \
                 `_` and `-`, in components joined by `/` (a tag is not part of a name)"
            )));
        }
        Ok(ImageName::Registry {
            host: host.to_string(),
            repository: repository.to_string(),
            digest,
        })
    }

    /// Whether the manifest `manifest` is the image this names: the digest a registry
    /// reference names, or the build key the manifest's annotation carries. A refusal names
    /// both sides.
    pub fn check(&self, manifest: &Manifest) -> Result<()> {
        match self {
            ImageName::Registry { digest, .. } if *digest != manifest.digest => {
                Err(refuse(format!(
                    "{self} names the manifest {digest}, and the image found is {}",
                    manifest.digest
                )))
            }
            ImageName::Registry { .. } => Ok(()),
            ImageName::Key(key) => match manifest.annotations.get(BUILD_KEY) {
                Some(found) if *found == key.to_string() => Ok(()),
                Some(found) => Err(refuse(format!(
                    "the build key {key} is mapped to the image {}, which was built for the key \
                     {found}",
                    manifest.digest
                ))),
                None => Err(refuse(format!(
                    "the build key {key} is mapped to the image {}, which carries no build key \
                     ({BUILD_KEY}): it was not built by Moruna",
                    manifest.digest
                ))),
            },
        }
    }
}

impl fmt::Display for ImageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageName::Key(key) => write!(f, "{key}"),
            ImageName::Registry {
                host,
                repository,
                digest,
            } => write!(f, "{host}/{repository}@{digest}"),
        }
    }
}

/// The image an `image` task runs, as its document names it (SDD 19 d.2): the build key of an
/// image Moruna builds, and, once it is built, the manifest's digest and where the image is (an
/// OCI layout on this machine, or a registry reference). A document whose image has no digest
/// is a draft: complete for drawing and checking, refused for running. An image Moruna did not
/// build has no build key: its digest and its location name it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRef {
    /// The build key, `sha256:` and 64 hex digits: a hash of the code, the lock, the runtime,
    /// the shim, the platform and the entry; absent for an image Moruna did not build.
    #[serde(default)]
    pub build_key: Option<String>,
    /// The manifest's digest, `sha256:...`; absent until the image is built.
    #[serde(default)]
    pub digest: Option<String>,
    /// Where the image is: an OCI layout directory (an absolute path) or a registry reference
    /// `HOST/REPOSITORY@sha256:...` whose digest is `digest`; absent until the image is built.
    #[serde(default)]
    pub location: Option<String>,
}

/// Where an [`ImageRef`] says its image is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    /// An OCI layout on this machine.
    Layout(PathBuf),
    /// An image in a registry, pulled before a run.
    Registry(ImageName),
}

impl ImageRef {
    /// Whether the image is built: its digest and its location are known.
    pub fn is_built(&self) -> bool {
        self.digest.is_some()
    }

    /// The reference checked: a build key is a digest; a digest and a location come together;
    /// a location is an absolute path or a registry reference to the digest; an image with no
    /// build key has a registry location. Returns the location, or `None` for a draft.
    pub fn check(&self) -> Result<Option<(Digest, Location)>> {
        if let Some(key) = &self.build_key {
            key.parse::<Digest>().map_err(|_| {
                refuse(format!(
                    "`{key}` is not a build key: sha256: and 64 hex digits"
                ))
            })?;
        }
        let (digest, location) = match (&self.digest, &self.location) {
            (None, None) if self.build_key.is_some() => return Ok(None),
            (None, None) => {
                return Err(refuse(
                    "an image names a build key, or a digest and a location".into(),
                ));
            }
            (Some(digest), Some(location)) => (digest, location),
            _ => {
                return Err(refuse(
                    "an image's digest and location are given together, once it is built".into(),
                ));
            }
        };
        let digest: Digest = digest
            .parse()
            .map_err(|_| refuse(format!("`{digest}` is not a manifest digest")))?;
        let location = if location.starts_with('/') {
            Location::Layout(PathBuf::from(location))
        } else {
            let name = ImageName::parse(location)?;
            match &name {
                ImageName::Registry { digest: at, .. } if *at == digest => {}
                ImageName::Registry { digest: at, .. } => {
                    return Err(refuse(format!(
                        "the location {location} names the manifest {at}, and the digest is {digest}"
                    )));
                }
                ImageName::Key(_) => {
                    return Err(refuse(format!(
                        "the location `{location}` is neither an OCI layout's absolute path nor a \
                         registry reference"
                    )));
                }
            }
            Location::Registry(name)
        };
        Ok(Some((digest, location)))
    }

    /// The image, found and checked: its layout (the location, or for a registry image the
    /// layout `pulled` it was pulled into), its manifest the digest, and for an image Moruna
    /// built, a manifest whose annotation carries the build key. A draft, an image not pulled
    /// and an image that is not the one named are refused, naming it.
    pub fn locate(&self, pulled: Option<&Path>) -> Result<(OciLayout, Manifest)> {
        let Some((digest, location)) = self.check()? else {
            return Err(refuse(format!(
                "the image of build key {} is not built: the document is a draft",
                self.build_key.as_deref().unwrap_or("?")
            )));
        };
        let layout = match &location {
            Location::Layout(path) => OciLayout::open(path)?,
            Location::Registry(name) => match pulled {
                Some(dir) => OciLayout::open(dir).map_err(|e| {
                    refuse(format!("the image {name} was not pulled for this run: {e}"))
                })?,
                None => {
                    return Err(refuse(format!(
                        "the image {name} was not pulled for this run: the runner pulls a \
                         registry image before it starts"
                    )));
                }
            },
        };
        let manifest = layout.manifest(&digest.to_string())?;
        if let Some(key) = &self.build_key {
            ImageName::Key(key.parse()?).check(&manifest)?;
        }
        Ok((layout, manifest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn names_are_a_key_or_a_registry_reference() {
        let key = format!("sha256:{HEX}");
        assert!(matches!(ImageName::parse(&key).unwrap(), ImageName::Key(_)));
        let reference = format!("registry.example.com:5000/team/scorer@sha256:{HEX}");
        let parsed = ImageName::parse(&reference).unwrap();
        assert_eq!(parsed.to_string(), reference);
        assert!(
            matches!(&parsed, ImageName::Registry { host, repository, .. }
            if host == "registry.example.com:5000" && repository == "team/scorer")
        );
        assert!(ImageName::parse(&format!("localhost/x@sha256:{HEX}")).is_ok());
        for bad in [
            "scorer:1".to_string(),
            format!("library/python@sha256:{HEX}"),
            format!("ghcr.io/Team/x@sha256:{HEX}"),
            format!("ghcr.io/team/x:1@sha256:{HEX}"),
            "ghcr.io/team/x@sha256:00".to_string(),
        ] {
            assert!(ImageName::parse(&bad).is_err(), "{bad}");
        }
    }

    fn manifest(annotations: &[(&str, &str)]) -> Manifest {
        let bytes = serde_json::json!({
            "schemaVersion": 2,
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "digest": format!("sha256:{HEX}"), "size": 1},
            "layers": [],
            "annotations": annotations.iter().copied().collect::<BTreeMap<_, _>>(),
        })
        .to_string();
        Manifest::parse(Digest::of(bytes.as_bytes()), bytes.as_bytes()).unwrap()
    }

    #[test]
    fn a_key_is_checked_against_the_manifests_annotation() {
        let key = format!("sha256:{HEX}");
        let name = ImageName::parse(&key).unwrap();
        name.check(&manifest(&[(BUILD_KEY, &key)])).unwrap();
        let other = format!("sha256:{}", "1".repeat(64));
        let e = name.check(&manifest(&[(BUILD_KEY, &other)])).unwrap_err();
        assert!(e.to_string().contains("built for the key"), "{e}");
        let e = name.check(&manifest(&[])).unwrap_err();
        assert!(e.to_string().contains("not built by Moruna"), "{e}");
        let found = manifest(&[]);
        let by_digest = ImageName::parse(&format!("r.io/x@{}", found.digest)).unwrap();
        by_digest.check(&found).unwrap();
        let wrong = ImageName::parse(&format!("r.io/x@sha256:{HEX}")).unwrap();
        assert!(wrong.check(&found).is_err());
    }

    #[test]
    fn a_reference_is_a_draft_or_built() {
        let key = format!("sha256:{HEX}");
        let draft = ImageRef {
            build_key: Some(key.clone()),
            ..ImageRef::default()
        };
        assert!(draft.check().unwrap().is_none());
        let e = draft.locate(None).unwrap_err().to_string();
        assert!(e.contains("is not built: the document is a draft"), "{e}");
        let half = ImageRef {
            digest: Some(key.clone()),
            ..draft.clone()
        };
        assert!(half.check().is_err());
        let pushed = ImageRef {
            build_key: Some(key.clone()),
            digest: Some(key.clone()),
            location: Some(format!("r.io/x@{key}")),
        };
        assert!(matches!(
            pushed.check().unwrap(),
            Some((_, Location::Registry(_)))
        ));
        let e = pushed.locate(None).unwrap_err().to_string();
        assert!(e.contains("was not pulled"), "{e}");
        let elsewhere = ImageRef {
            location: Some(format!("r.io/x@sha256:{}", "2".repeat(64))),
            ..pushed.clone()
        };
        assert!(elsewhere.check().is_err());
        let external_on_disk = ImageRef {
            build_key: None,
            digest: Some(key.clone()),
            location: Some("/layouts/x".into()),
        };
        assert!(matches!(
            external_on_disk.check().unwrap(),
            Some((_, Location::Layout(_)))
        ));
        let relative = ImageRef {
            location: Some("layouts/x".into()),
            ..external_on_disk
        };
        assert!(
            relative.check().is_err(),
            "a layout is named by its absolute path"
        );
        assert!(ImageRef::default().check().is_err());
    }
}
