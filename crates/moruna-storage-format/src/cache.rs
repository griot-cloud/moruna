//! The single host/native encrypted cache identity, prefix and cipher authority. No IO,
//! entropy/default key/provider, retention promise or physical sanitization claim.
use crate::{Error, MAX_KEY_PAGE, MAX_PAYLOAD, Object, Reader, Result, VERSION, Version, Writer};
use aes_gcm::{Aes256Gcm, KeyInit, aead::AeadInPlace};
use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use zeroize::Zeroizing;
// Compile-time evidence that the globally selected keyed-state representations actually
// have Drop. The pinned upstream source establishes what their Drop erases.
const _: () = assert!(core::mem::needs_drop::<polyval::Polyval>());
const _: () = assert!(core::mem::needs_drop::<ghash::GHash>());
/// Fixed file/extent envelope prefix before canonical identity and ciphertext payload.
pub const PREFIX: usize = 40;
const MAGIC: &[u8; 8] = b"MORUCAC1";
/// Actual requester workspace key from the designated live key authority. No Debug/Clone.
pub struct WorkspaceKey {
    /// Nonsecret key-version identity authenticated in every envelope.
    pub id: String,
    /// Secret key material; zeroized when this owned key leaves scope.
    pub bytes: Zeroizing<[u8; 32]>,
}
/// Maximum requester and key-version identity in a secret page.
pub const MAX_KEY_IDENTITY: usize = 1024;
impl WorkspaceKey {
    /// Construct an actual externally supplied key with a bounded nonsecret version id.
    pub fn new(id: String, bytes: Zeroizing<[u8; 32]>) -> Result<Self> {
        if id.is_empty() || id.len() > MAX_KEY_IDENTITY {
            return Err(Error::Invalid);
        }
        Ok(Self { id, bytes })
    }
    /// Encode only into explicitly granted secret memory. Caller owns producer authority;
    /// this codec never creates a key or grants access. The whole destination is wiped first.
    pub fn encode_secret(&self, requester: &str, destination: &mut [u8]) -> Result<usize> {
        if requester.is_empty()
            || requester.len() > MAX_KEY_IDENTITY
            || self.id.is_empty()
            || self.id.len() > MAX_KEY_IDENTITY
            || destination.len() > MAX_KEY_PAGE
        {
            return Err(Error::Invalid);
        }
        let used = 56usize
            .checked_add(requester.len())
            .and_then(|n| n.checked_add(self.id.len()))
            .ok_or(Error::Limit)?;
        if used > destination.len() {
            return Err(Error::Limit);
        }
        destination.fill(0);
        destination[..8].copy_from_slice(b"MORUKEY1");
        destination[8..12].copy_from_slice(&VERSION.to_le_bytes());
        destination[12..16].copy_from_slice(&(used as u32).to_le_bytes());
        destination[16..20].copy_from_slice(&(requester.len() as u32).to_le_bytes());
        destination[20..24].copy_from_slice(&(self.id.len() as u32).to_le_bytes());
        destination[24..56].copy_from_slice(self.bytes.as_ref());
        destination[56..56 + requester.len()].copy_from_slice(requester.as_bytes());
        destination[56 + requester.len()..used].copy_from_slice(self.id.as_bytes());
        Ok(used)
    }
    /// Read only the canonical validated granted page and bind it to the actual requester.
    /// Native adapter checks Memory capability type/rights/length before exposing this slice.
    pub fn decode_secret(source: &[u8], requester: &str) -> Result<Self> {
        if source.len() < 56 || source.len() > MAX_KEY_PAGE || &source[..8] != b"MORUKEY1" {
            return Err(Error::Invalid);
        }
        let number = |at: usize| -> Result<usize> {
            Ok(
                u32::from_le_bytes(source[at..at + 4].try_into().map_err(|_| Error::Invalid)?)
                    as usize,
            )
        };
        if number(8)? != VERSION as usize {
            return Err(Error::Version);
        }
        let used = number(12)?;
        let requester_len = number(16)?;
        let key_len = number(20)?;
        if requester_len == 0
            || key_len == 0
            || requester_len > MAX_KEY_IDENTITY
            || key_len > MAX_KEY_IDENTITY
            || 56usize
                .checked_add(requester_len)
                .and_then(|n| n.checked_add(key_len))
                != Some(used)
            || used > source.len()
            || source[used..].iter().any(|b| *b != 0)
        {
            return Err(Error::Invalid);
        }
        if source[56..56 + requester_len] != *requester.as_bytes() {
            return Err(Error::Integrity);
        }
        let id = core::str::from_utf8(&source[56 + requester_len..used])
            .map_err(|_| Error::Invalid)?
            .to_string();
        let mut bytes = Zeroizing::new([0; 32]);
        bytes.copy_from_slice(&source[24..56]);
        Self::new(id, bytes)
    }
}
/// Canonical whole-object cache/scratch slot within one requester workspace and node
/// epoch namespace. Resource owner remains distinct; this function grants no access.
pub fn object_slot(requester: &str, object: &Object) -> Result<[u8; 32]> {
    if requester.is_empty() {
        return Err(Error::Invalid);
    }
    object.validate()?;
    let mut out = Writer(Vec::new());
    out.text(requester)?;
    out.text(&object.workspace)?;
    out.text(&object.bucket)?;
    out.text(&object.key)?;
    Ok(*blake3::hash(&out.0).as_bytes())
}
/// Exact authenticated cache identity. Resource owner and requester are distinct.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Binding {
    /// Cache major, never inferred from a filename.
    pub version: u32,
    /// Requester key/cache scope.
    pub requester: String,
    /// Exact resource-owning identity.
    pub object: Object,
    /// Pinned immutable object version.
    pub object_version: Version,
    /// Exact object range start.
    pub offset: u64,
    /// Exact plaintext/ciphertext payload bytes.
    pub bytes: u64,
    /// Actual workspace key version identity.
    pub key_id: String,
}
impl Binding {
    fn validate(&self) -> Result<()> {
        if self.version != VERSION {
            return Err(Error::Version);
        }
        self.object.validate()?;
        self.object_version.validate()?;
        if self.requester.is_empty()
            || self.key_id.is_empty()
            || self.offset.checked_add(self.bytes).is_none()
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    /// Canonical authenticated identity bytes, usable by either platform with one codec.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut out = Writer(Vec::new());
        out.u32(self.version)?;
        out.text(&self.requester)?;
        identity(
            &mut out,
            &self.object,
            &self.object_version,
            self.offset,
            self.bytes,
        )?;
        out.text(&self.key_id)?;
        Ok(out.0)
    }
    /// Exact bounded identity; trailing bytes, malformed identities and major changes refuse.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_PAYLOAD {
            return Err(Error::Limit);
        }
        let mut input = Reader { bytes, at: 0 };
        let binding = Self {
            version: input.u32()?,
            requester: input.text()?,
            object: Object {
                workspace: input.text()?,
                bucket: input.text()?,
                key: input.text()?,
            },
            object_version: Version {
                etag: input.optional()?,
                generation: input.optional()?,
            },
            offset: input.u64()?,
            bytes: input.u64()?,
            key_id: input.text()?,
        };
        if input.at != bytes.len() {
            return Err(Error::Invalid);
        }
        binding.validate()?;
        Ok(binding)
    }
}
fn identity(
    out: &mut Writer,
    object: &Object,
    version: &Version,
    offset: u64,
    bytes: u64,
) -> Result<()> {
    object.validate()?;
    version.validate()?;
    offset.checked_add(bytes).ok_or(Error::Limit)?;
    for text in [&object.workspace, &object.bucket, &object.key] {
        out.text(text)?;
    }
    out.optional(&version.etag)?;
    out.optional(&version.generation)?;
    out.u64(offset)?;
    out.u64(bytes)
}
/// Stable opaque cache name binding requester AND owner/object/version/range. The fresh
/// node epoch directory provides additional epoch separation and is never reused.
pub fn path_identity(
    requester: &str,
    object: &Object,
    version: &Version,
    offset: u64,
    bytes: u64,
) -> Result<String> {
    if requester.is_empty() {
        return Err(Error::Invalid);
    }
    let mut out = Writer(Vec::new());
    out.u32(VERSION)?;
    out.text(requester)?;
    identity(&mut out, object, version, offset, bytes)?;
    Ok(blake3::hash(&out.0).to_hex().to_string())
}
/// Validated fixed prefix; no secret keys/capabilities are serialized here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Prefix {
    /// Canonical identity bytes immediately following the prefix.
    pub binding_bytes: u32,
    /// Actual fresh nonce supplied by the platform's cryptographic entropy service.
    pub nonce: [u8; 12],
    /// Detached AES-GCM authentication tag.
    pub tag: [u8; 16],
}
impl Prefix {
    /// Encode exact bounded metadata length and supplied nonce/tag.
    pub fn encode(&self) -> Result<[u8; PREFIX]> {
        if self.binding_bytes as usize > MAX_PAYLOAD {
            return Err(Error::Limit);
        }
        let mut out = [0; PREFIX];
        out[..8].copy_from_slice(MAGIC);
        out[8..20].copy_from_slice(&self.nonce);
        out[20..36].copy_from_slice(&self.tag);
        out[36..40].copy_from_slice(&self.binding_bytes.to_le_bytes());
        Ok(out)
    }
    /// Parse only the exact fixed prefix, before any metadata allocation.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != PREFIX || &bytes[..8] != MAGIC {
            return Err(Error::Invalid);
        }
        let prefix = Self {
            binding_bytes: u32::from_le_bytes(
                bytes[36..40].try_into().map_err(|_| Error::Invalid)?,
            ),
            nonce: bytes[8..20].try_into().map_err(|_| Error::Invalid)?,
            tag: bytes[20..36].try_into().map_err(|_| Error::Invalid)?,
        };
        if prefix.binding_bytes as usize > MAX_PAYLOAD {
            return Err(Error::Limit);
        }
        Ok(prefix)
    }
}
/// Per-operation cipher, with zeroizing AES key schedule enabled by the pinned library
/// features. Derived/raw temporary keys and keyed BLAKE3 state are zeroized too.
pub struct Cipher(Aes256Gcm);
impl Cipher {
    /// Derive with fresh node epoch, actual requester workspace key and requester identity.
    pub fn new(node: &[u8; 32], workspace: &[u8; 32], requester: &str) -> Result<Self> {
        if requester.is_empty() {
            return Err(Error::Invalid);
        }
        let mut hash = Zeroizing::new(blake3::Hasher::new_keyed(node));
        hash.update(b"moruna-storage-cache-v1\0");
        hash.update(workspace);
        hash.update(requester.as_bytes());
        let digest = Zeroizing::new(hash.finalize());
        let derived = Zeroizing::new(*digest.as_bytes());
        Ok(Self(
            Aes256Gcm::new_from_slice(derived.as_ref()).map_err(|_| Error::Integrity)?,
        ))
    }
    /// Encrypt only the provided charged payload in place; caller supplies actual entropy.
    pub fn seal(&self, nonce: &[u8; 12], aad: &[u8], payload: &mut [u8]) -> Result<[u8; 16]> {
        let tag = self
            .0
            .encrypt_in_place_detached(nonce.into(), aad, payload)
            .map_err(|_| Error::Integrity)?;
        tag.as_slice().try_into().map_err(|_| Error::Integrity)
    }
    /// Authenticate/decrypt in the same destination; failure always wipes that destination.
    pub fn open(
        &self,
        nonce: &[u8; 12],
        aad: &[u8],
        tag: &[u8; 16],
        payload: &mut [u8],
    ) -> Result<()> {
        if self
            .0
            .decrypt_in_place_detached(nonce.into(), aad, payload, tag.into())
            .is_err()
        {
            payload.fill(0);
            return Err(Error::Integrity);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    mod build_policy {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/build_policy.rs"));
    }
    #[test]
    fn selected_schedules_drop_and_unsupported_dependency_flags_refuse() {
        fn aes_zeroizing_schedule<T: zeroize::ZeroizeOnDrop>() {}
        aes_zeroizing_schedule::<aes::Aes256>();
        assert!(core::mem::needs_drop::<polyval::Polyval>());
        assert!(core::mem::needs_drop::<ghash::GHash>());
        assert!(build_policy::allows("--cfg\x1fpolyval_force_soft"));
        assert!(build_policy::allows("--cfg=polyval_force_soft"));
        for flags in [
            "",
            "--cfg\x1fpolyval_armv8",
            "--cfg\x1faes_armv8",
            "--cfg\x1fpolyval_force_soft=\"false\"",
        ] {
            assert!(!build_policy::allows(flags));
        }
    }
    fn binding() -> Binding {
        Binding {
            version: VERSION,
            requester: "requester".to_string(),
            object: Object {
                workspace: "owner".to_string(),
                bucket: "home".to_string(),
                key: "private".to_string(),
            },
            object_version: Version {
                etag: Some("etag".to_string()),
                generation: Some("v1".to_string()),
            },
            offset: 7,
            bytes: 15,
            key_id: "actual-key-version".to_string(),
        }
    }
    #[test]
    fn granted_secret_page_binds_requester_version_and_exact_readonly_layout() {
        let key = WorkspaceKey::new("key-version".to_string(), Zeroizing::new([9; 32])).unwrap();
        let mut page = vec![0x55; MAX_KEY_PAGE];
        let used = key.encode_secret("requester", &mut page).unwrap();
        assert!(page[used..].iter().all(|b| *b == 0));
        let actual = WorkspaceKey::decode_secret(&page, "requester").unwrap();
        assert_eq!(actual.id, key.id);
        assert_eq!(actual.bytes.as_ref(), key.bytes.as_ref());
        assert!(matches!(
            WorkspaceKey::decode_secret(&page, "other"),
            Err(Error::Integrity)
        ));
        page[8..12].copy_from_slice(&(VERSION + 1).to_le_bytes());
        assert!(matches!(
            WorkspaceKey::decode_secret(&page, "requester"),
            Err(Error::Version)
        ));
        for bytes in [0, 7, 55, used - 1] {
            assert!(WorkspaceKey::decode_secret(&page[..bytes], "requester").is_err());
        }
    }
    #[test]
    fn portable_cache_has_one_canonical_identity_prefix_and_cipher_path() {
        let binding = binding();
        let aad = binding.encode().unwrap();
        assert_eq!(Binding::decode(&aad).unwrap(), binding);
        for cut in 0..aad.len() {
            assert!(Binding::decode(&aad[..cut]).is_err());
        }
        let cipher = Cipher::new(&[7; 32], &[9; 32], &binding.requester).unwrap();
        let mut payload = b"private-15bytes!".to_vec();
        let original = payload.clone();
        let pointer = payload.as_ptr();
        let nonce = [3; 12];
        let tag = cipher.seal(&nonce, &aad, &mut payload).unwrap();
        assert_ne!(payload, original);
        assert_eq!(payload.as_ptr(), pointer);
        let prefix = Prefix {
            binding_bytes: aad.len() as u32,
            nonce,
            tag,
        };
        assert_eq!(Prefix::decode(&prefix.encode().unwrap()).unwrap(), prefix);
        cipher.open(&nonce, &aad, &tag, &mut payload).unwrap();
        assert_eq!(payload, original);
        assert_eq!(payload.as_ptr(), pointer);
        let tag = cipher.seal(&nonce, &aad, &mut payload).unwrap();
        let other = Cipher::new(&[7; 32], &[9; 32], "other-requester").unwrap();
        assert_eq!(
            other.open(&nonce, &aad, &tag, &mut payload),
            Err(Error::Integrity)
        );
        assert_eq!(payload, vec![0; original.len()]);
        assert_ne!(
            path_identity("requester", &binding.object, &binding.object_version, 7, 15).unwrap(),
            path_identity("other", &binding.object, &binding.object_version, 7, 15).unwrap()
        );
    }
    #[test]
    fn cache_refuses_changed_major_and_corrupt_prefix_before_metadata_allocation() {
        let mut bytes = binding().encode().unwrap();
        bytes[..4].copy_from_slice(&(VERSION + 1).to_le_bytes());
        assert_eq!(Binding::decode(&bytes), Err(Error::Version));
        let mut prefix = Prefix {
            binding_bytes: 1,
            nonce: [1; 12],
            tag: [2; 16],
        }
        .encode()
        .unwrap();
        prefix[36..40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(Prefix::decode(&prefix), Err(Error::Limit));
        assert!(Prefix::decode(&[0; PREFIX]).is_err());
    }
}
