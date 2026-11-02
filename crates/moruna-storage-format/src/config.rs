//! Sole bounded native service boot configuration, containing NO credential/key bytes.
//! Grants travel as actual startup capabilities, never paths to ambient credential files.
use crate::{Error, Result, Role};
use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
};
/// Maximum explicit native boot control metadata.
pub const MAX_CONFIG: usize = 4096;
/// Explicit bounded byte region inside ONE real local device/namespace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Region {
    /// Absolute byte base within the leased parent namespace.
    pub offset: u64,
    /// Positive byte capacity of this logical volume.
    pub bytes: u64,
}
/// Sole checked region geometry. Actual IO owner supplies real capacity/alignment;
/// configuration parsing alone cannot attest that a physical device has this capacity.
pub fn validate_regions(regions: &[Region], alignment: u32, capacity: u64) -> Result<()> {
    if alignment == 0 || !alignment.is_power_of_two() {
        return Err(Error::Invalid);
    }
    for (index, region) in regions.iter().enumerate() {
        let end = region
            .offset
            .checked_add(region.bytes)
            .ok_or(Error::Limit)?;
        if region.bytes == 0
            || !region.offset.is_multiple_of(alignment as u64)
            || !region.bytes.is_multiple_of(alignment as u64)
            || end > capacity
        {
            return Err(Error::Invalid);
        }
        for prior in &regions[..index] {
            let prior_end = prior.offset.checked_add(prior.bytes).ok_or(Error::Limit)?;
            if region.offset < prior_end && prior.offset < end {
                return Err(Error::Invalid);
            }
        }
    }
    Ok(())
}
/// Explicit configured provider. A missing backend never selects another provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Provider {
    /// Real configured TLS object proxy backed by a bucket adapter.
    Bucket {
        /// Explicit DNS/IP target (resolved/authorized by KE2).
        host: String,
        /// Explicit port, no scheme/default guessing.
        port: u16,
        /// Verified TLS peer name.
        tls_name: String,
        /// Exact opaque bytes in the granted credential Memory.
        credential_bytes: u32,
    },
    /// ONE local NVMe owner/lease with actual disjoint metadata and encrypted cache
    /// volumes. Role endpoints share the physical owner, never one writer per string.
    Local {
        /// Literal physical block service.
        service: String,
        /// Explicit alignment for both mounted volumes.
        alignment: u32,
        /// Durable small metadata volume.
        metadata: Region,
        /// Ephemeral encrypted cache/scratch volume.
        cache: Region,
        /// Optional explicitly configured raw local HOME, never inferred/fallback.
        home: Option<Region>,
        /// Exact secret-page bytes in storage.key.
        key_bytes: u32,
        /// Actual workspace in the granted workspace key page.
        requester: String,
    },
    /// Explicit raw namespace; mount validates actual Superblock capacity/role.
    Raw {
        /// Literal service lookup in the granted block directory.
        service: String,
        /// Explicit required raw alignment.
        alignment: u32,
        /// SingleNodeHome or CheckpointMetadata, never plaintext cache.
        role: Role,
    },
}
/// Parsed strict version-1 boot configuration. Every operational bound is explicit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootConfig {
    /// Real provider and its exact required grant roles.
    pub provider: Provider,
    /// Positive bounded transport wait, nanoseconds (not infinite).
    pub timeout_ns: u64,
    /// Maximum admitted body, still constrained by actual caller Account/Memory.
    pub body_limit: u64,
    /// Maximum owned admitted sessions, checked against actual dispatcher capacity.
    pub session_limit: usize,
}
impl BootConfig {
    /// Decode exactly one zero-padded startup Memory page. Only trailing zero padding
    /// is removed; embedded NULs still refuse. No missing-page/environment fallback.
    pub fn decode_page(source: &[u8]) -> Result<Self> {
        if source.len() != MAX_CONFIG {
            return Err(Error::Invalid);
        }
        let used = source
            .iter()
            .rposition(|byte| *byte != 0)
            .map(|last| last + 1)
            .ok_or(Error::Invalid)?;
        Self::decode(&source[..used])
    }

    /// Decode UTF-8 `moruna-storage 1` plus unique `key value` lines. Unknown, duplicate,
    /// missing or mutually incompatible fields refuse; no environment/default fallback.
    pub fn decode(source: &[u8]) -> Result<Self> {
        if source.len() > MAX_CONFIG {
            return Err(Error::Limit);
        }
        let text = core::str::from_utf8(source).map_err(|_| Error::Invalid)?;
        let mut lines = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'));
        if lines.next() != Some("moruna-storage 1") {
            return Err(Error::Version);
        }
        let mut fields = BTreeMap::new();
        for line in lines {
            let (name, value) = line.split_once(' ').ok_or(Error::Invalid)?;
            let value = value.trim();
            if name.is_empty()
                || value.is_empty()
                || value.bytes().any(|byte| byte == 0)
                || fields.insert(name, value).is_some()
            {
                return Err(Error::Invalid);
            }
        }
        let timeout_ns = number(take(&mut fields, "timeout-ns")?)?;
        let body_limit = number(take(&mut fields, "body-limit")?)?;
        let session_limit = usize::try_from(number(take(&mut fields, "session-limit")?)?)
            .map_err(|_| Error::Limit)?;
        if timeout_ns == 0 || timeout_ns == u64::MAX || body_limit == 0 || session_limit == 0 {
            return Err(Error::Invalid);
        }
        let provider = match take(&mut fields, "provider")? {
            "bucket" => {
                let host = identity(take(&mut fields, "host")?)?;
                let port = u16::try_from(number(take(&mut fields, "port")?)?)
                    .map_err(|_| Error::Invalid)?;
                let tls_name = identity(take(&mut fields, "tls-name")?)?;
                let credential_bytes =
                    u32::try_from(number(take(&mut fields, "credential-bytes")?)?)
                        .map_err(|_| Error::Limit)?;
                if port == 0
                    || credential_bytes == 0
                    || credential_bytes as usize > crate::wire::MAX_CAPABILITY
                {
                    return Err(Error::Invalid);
                }
                Provider::Bucket {
                    host,
                    port,
                    tls_name,
                    credential_bytes,
                }
            }
            "local" => {
                let service = identity(take(&mut fields, "block-service")?)?;
                let alignment = u32::try_from(number(take(&mut fields, "alignment")?)?)
                    .map_err(|_| Error::Limit)?;
                if alignment < 512 || !alignment.is_power_of_two() {
                    return Err(Error::Invalid);
                }
                let metadata = Region {
                    offset: number(take(&mut fields, "metadata-offset")?)?,
                    bytes: number(take(&mut fields, "metadata-bytes")?)?,
                };
                let cache = Region {
                    offset: number(take(&mut fields, "cache-offset")?)?,
                    bytes: number(take(&mut fields, "cache-bytes")?)?,
                };
                let home = match (fields.remove("home-offset"), fields.remove("home-bytes")) {
                    (None, None) => None,
                    (Some(offset), Some(bytes)) => Some(Region {
                        offset: number(offset)?,
                        bytes: number(bytes)?,
                    }),
                    _ => return Err(Error::Invalid),
                };
                let mut regions = alloc::vec![metadata, cache];
                if let Some(home) = home {
                    regions.push(home);
                }
                validate_regions(&regions, alignment, u64::MAX)?;
                let key_bytes = u32::try_from(number(take(&mut fields, "key-bytes")?)?)
                    .map_err(|_| Error::Limit)?;
                if key_bytes == 0 || key_bytes as usize > crate::MAX_KEY_PAGE {
                    return Err(Error::Invalid);
                }
                let requester = identity(take(&mut fields, "requester")?)?;
                Provider::Local {
                    service,
                    alignment,
                    metadata,
                    cache,
                    home,
                    key_bytes,
                    requester,
                }
            }
            mode @ ("single-node-home" | "checkpoint-metadata") => {
                let service = identity(take(&mut fields, "block-service")?)?;
                let alignment = u32::try_from(number(take(&mut fields, "alignment")?)?)
                    .map_err(|_| Error::Limit)?;
                if alignment < 512 || !alignment.is_power_of_two() {
                    return Err(Error::Invalid);
                }
                Provider::Raw {
                    service,
                    alignment,
                    role: if mode == "single-node-home" {
                        Role::SingleNodeHome
                    } else {
                        Role::CheckpointMetadata
                    },
                }
            }
            _ => return Err(Error::Invalid),
        };
        if !fields.is_empty() {
            return Err(Error::Invalid);
        }
        Ok(Self {
            provider,
            timeout_ns,
            body_limit,
            session_limit,
        })
    }
}
fn take<'a>(fields: &mut BTreeMap<&'a str, &'a str>, key: &str) -> Result<&'a str> {
    fields.remove(key).ok_or(Error::Invalid)
}
fn number(value: &str) -> Result<u64> {
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::Invalid);
    }
    value.parse().map_err(|_| Error::Limit)
}
fn identity(value: &str) -> Result<String> {
    if value.is_empty()
        || value.len() > 253
        || value.bytes().any(|byte| byte <= b' ' || byte >= 127)
    {
        return Err(Error::Invalid);
    }
    Ok(value.to_string())
}
/// Explicit private housekeeping policy. It carries no application Scope, credential,
/// object roots or authority; the actual connected boot capability grants maintenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaintenanceConfig {
    /// Positive finite interval between real compactions, also the RPC soft wait bound.
    pub interval_ns: u64,
    /// Maximum retained object identities in one sealed liveness set.
    pub max_objects: usize,
    /// Maximum actual committed namespace roots traversed in one owner.
    pub max_roots: usize,
    /// Explicit canonical page/decode workspace, exactly wire::MAX_HEADER.
    /// Whole-walk fingerprint/index reservations are additional actual charges.
    pub metadata_bytes: usize,
    /// Maximum actual page IOs admitted for a bounded namespace walk.
    pub page_io: u32,
}
impl MaintenanceConfig {
    /// Validate finite explicit admission ceilings, never substitute defaults.
    pub fn validate(&self) -> Result<()> {
        if self.interval_ns == 0
            || self.interval_ns == u64::MAX
            || self.max_objects == 0
            || self.max_roots == 0
            || self.max_roots > self.max_objects
            || self.max_objects > u32::MAX as usize
            || self.metadata_bytes != crate::wire::MAX_HEADER
            || self.page_io == 0
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    /// Decode the one canonical zero-padded readonly startup page.
    pub fn decode_page(source: &[u8]) -> Result<Self> {
        if source.len() != MAX_CONFIG {
            return Err(Error::Invalid);
        }
        let used = source
            .iter()
            .rposition(|byte| *byte != 0)
            .map(|at| at + 1)
            .ok_or(Error::Invalid)?;
        Self::decode(&source[..used])
    }
    /// Decode strict `moruna-storage-maintenance 1` and all five required unique keys.
    /// Missing/unknown/duplicate fields, embedded NUL and unbounded values refuse.
    pub fn decode(source: &[u8]) -> Result<Self> {
        if source.len() > MAX_CONFIG {
            return Err(Error::Limit);
        }
        let text = core::str::from_utf8(source).map_err(|_| Error::Invalid)?;
        let mut lines = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'));
        if lines.next() != Some("moruna-storage-maintenance 1") {
            return Err(Error::Version);
        }
        let mut fields = BTreeMap::new();
        for line in lines {
            let (name, value) = line.split_once(' ').ok_or(Error::Invalid)?;
            let value = value.trim();
            if name.is_empty()
                || value.is_empty()
                || value.bytes().any(|b| b == 0)
                || fields.insert(name, value).is_some()
            {
                return Err(Error::Invalid);
            }
        }
        let config = Self {
            interval_ns: number(take(&mut fields, "interval-ns")?)?,
            max_objects: usize::try_from(number(take(&mut fields, "max-objects")?)?)
                .map_err(|_| Error::Limit)?,
            max_roots: usize::try_from(number(take(&mut fields, "max-roots")?)?)
                .map_err(|_| Error::Limit)?,
            metadata_bytes: usize::try_from(number(take(&mut fields, "metadata-bytes")?)?)
                .map_err(|_| Error::Limit)?,
            page_io: u32::try_from(number(take(&mut fields, "page-io")?)?)
                .map_err(|_| Error::Limit)?,
        };
        if !fields.is_empty() {
            return Err(Error::Invalid);
        }
        config.validate()?;
        Ok(config)
    }
    /// Internal traversal ceilings, not a client file quota or permission. Logical sparse
    /// lengths remain fully representable; actual metadata/page/object resources above
    /// stay explicitly finite and charged. No historical retention guarantee is added.
    pub fn limits(&self) -> Result<crate::namespace::Limits> {
        self.validate()?;
        crate::namespace::Limits::maintenance(self.page_io, self.metadata_bytes)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    const COMMON: &str =
        "moruna-storage 1\ntimeout-ns 1000000000\nbody-limit 1048576\nsession-limit 4\n";
    #[test]
    fn explicit_provider_fields_and_all_bounds_are_required() {
        let bucket = alloc::format!(
            "{COMMON}provider bucket\nhost objects.example\nport 443\ntls-name objects.example\ncredential-bytes 32\n"
        );
        assert!(matches!(
            BootConfig::decode(bucket.as_bytes()).unwrap().provider,
            Provider::Bucket { port: 443, .. }
        ));
        let mut page = [0; MAX_CONFIG];
        page[..bucket.len()].copy_from_slice(bucket.as_bytes());
        assert_eq!(
            BootConfig::decode_page(&page).unwrap(),
            BootConfig::decode(bucket.as_bytes()).unwrap()
        );
        page[20] = 0;
        assert!(BootConfig::decode_page(&page).is_err());
        let raw = alloc::format!(
            "{COMMON}provider checkpoint-metadata\nblock-service nvme0\nalignment 4096\n"
        );
        assert!(matches!(
            BootConfig::decode(raw.as_bytes()).unwrap().provider,
            Provider::Raw {
                role: Role::CheckpointMetadata,
                ..
            }
        ));
        for invalid in [
            bucket.replace("credential-bytes 32", "credential-bytes 0"),
            bucket.replace("port 443", "port 0"),
            bucket.replace("timeout-ns 1000000000\n", ""),
            bucket.replace("session-limit 4", "session-limit 0"),
            alloc::format!("{bucket}provider bucket\n"),
            alloc::format!("{raw}host guessed.example\n"),
            raw.replace("checkpoint-metadata", "cache-scratch"),
            raw.replace("alignment 4096", "alignment 511"),
        ] {
            assert!(BootConfig::decode(invalid.as_bytes()).is_err(), "{invalid}");
        }
        assert!(matches!(
            BootConfig::decode(
                bucket
                    .replace("moruna-storage 1", "moruna-storage 2")
                    .as_bytes()
            ),
            Err(Error::Version)
        ));
    }
}

#[cfg(test)]
mod maintenance_tests {
    use super::*;
    const POLICY:&[u8]=b"moruna-storage-maintenance 1\ninterval-ns 1000000000\nmax-objects 1024\nmax-roots 16\nmetadata-bytes 65536\npage-io 4096\n";
    #[test]
    fn maintenance_requires_all_explicit_finite_bounds() {
        let policy = MaintenanceConfig::decode(POLICY).unwrap();
        assert_eq!(policy.max_objects, 1024);
        assert_eq!(policy.limits().unwrap().page_io, 4096);
        let mut page = [0; MAX_CONFIG];
        page[..POLICY.len()].copy_from_slice(POLICY);
        assert_eq!(MaintenanceConfig::decode_page(&page).unwrap(), policy);
        page[POLICY.len() + 1] = 1;
        assert!(MaintenanceConfig::decode_page(&page).is_err());
    }
    #[test]
    fn maintenance_missing_unknown_duplicate_and_infinite_values_refuse() {
        for text in [
            "moruna-storage-maintenance 1\n",
            "moruna-storage-maintenance 2\n",
        ] {
            assert!(MaintenanceConfig::decode(text.as_bytes()).is_err());
        }
        let text = core::str::from_utf8(POLICY).unwrap();
        for bad in [
            alloc::format!("{text}max-roots 1\n"),
            alloc::format!("{text}scope trusted\n"),
            text.replace("interval-ns 1000000000", "interval-ns 18446744073709551615"),
            text.replace("max-roots 16", "max-roots 2048"),
            text.replace("metadata-bytes 65536", "metadata-bytes 0"),
        ] {
            assert!(MaintenanceConfig::decode(bad.as_bytes()).is_err());
        }
    }
}
