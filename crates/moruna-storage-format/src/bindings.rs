//! Sole no_std engine identity envelope. JSON configuration remains an opaque payload;
//! only Admission is authority, and consumers must reject duplicate JSON disagreements.
use crate::{Error, Result, auth::Admission, wire};
use alloc::vec::Vec;
/// Versioned bounded engine bootstrap identity plus opaque std configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineBindings {
    /// Exact trusted native scope/role, shared with actual private session admission.
    pub admission: Admission,
    /// Opaque application configuration; this core never extracts identity from JSON.
    pub payload: Vec<u8>,
}
impl EngineBindings {
    /// Encode complete canonical control, total at most MAX_HEADER. No truncation,
    /// guessed identity, hidden payload allocation or alternate unversioned JSON mode.
    pub fn encode_into(&self, destination: &mut [u8]) -> Result<usize> {
        let mut out = wire::Out::new(destination)?;
        out.u8(11)?;
        self.admission.write(&mut out)?;
        out.blob(&self.payload)?;
        out.finish()
    }
    /// Decode exact bounded whole envelope; unknown kinds/majors/trailing bytes refuse.
    pub fn decode(source: &[u8]) -> Result<Self> {
        let mut input = wire::Input::new(source)?;
        if input.u8()? != 11 {
            return Err(Error::Invalid);
        }
        let admission = Admission::read(&mut input)?;
        let payload = input.blob()?.to_vec();
        input.finish()?;
        Ok(Self { admission, payload })
    }
    /// Decode page-rounded startup Memory. Header determines exact bytes; padding must
    /// be zero. Consumers map PREFIX_BYTES/first page, call header_bytes, then map/read
    /// precisely the necessary page-rounded capacity, bounded by MAX_HEADER.
    pub fn decode_page(source: &[u8]) -> Result<Self> {
        let length = wire::header_bytes(source)?;
        if length > source.len() || source[length..].iter().any(|byte| *byte != 0) {
            return Err(Error::Incomplete);
        }
        Self::decode(&source[..length])
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Scope, auth::SessionRole};
    #[test]
    fn identity_is_explicit_and_payload_is_opaque_with_exact_framing() {
        let bindings = EngineBindings {
            admission: Admission {
                scope: Scope {
                    workspace: "trusted".into(),
                    run: "run".into(),
                },
                role: SessionRole::BucketHome,
            },
            payload: br#"{"workspace":"untrusted-other","run":"other","binary":"\u0000"}"#.to_vec(),
        };
        let mut bytes = [0; 4096];
        let length = bindings.encode_into(&mut bytes).unwrap();
        assert_eq!(EngineBindings::decode(&bytes[..length]).unwrap(), bindings);
        assert_eq!(EngineBindings::decode_page(&bytes).unwrap(), bindings);
        assert!(Admission::decode(&bytes[..length]).is_err());
        for cut in 0..length {
            assert!(EngineBindings::decode(&bytes[..cut]).is_err());
        }
        bytes[length] = 1;
        assert!(EngineBindings::decode_page(&bytes).is_err());
        let large = EngineBindings {
            payload: alloc::vec![0; wire::MAX_HEADER],
            ..bindings
        };
        assert!(
            large
                .encode_into(&mut alloc::vec![0; wire::MAX_HEADER])
                .is_err()
        );
    }
}
