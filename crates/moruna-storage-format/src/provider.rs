//! Nonsecret provider facts, distinct from resource authorization and native admission.
//! A description conveys NO object permission; unknown/fixture backends never become home.
use crate::{Error, Result, auth::SessionRole, wire};
/// Exact provider-description request. It contains no fake resource or credential.
pub struct Request {
    /// Correlation identity.
    pub id: u64,
}
impl Request {
    /// Encode one bounded canonical provider query.
    pub fn encode_into(&self, destination: &mut [u8]) -> Result<usize> {
        let mut out = wire::Out::new(destination)?;
        out.u8(12)?;
        out.u64(self.id)?;
        out.finish()
    }
    /// Decode only exact provider-query control.
    pub fn decode(source: &[u8]) -> Result<Self> {
        let mut input = wire::Input::new(source)?;
        if input.u8()? != 12 {
            return Err(Error::Invalid);
        }
        let id = input.u64()?;
        input.finish()?;
        Ok(Self { id })
    }
}
/// Actual configured backend fact, NOT an admission/credential or physical cloud proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Description {
    /// Original request identity.
    pub id: u64,
    /// None explicitly means fixture/unknown/unsupported durability role.
    pub role: Option<SessionRole>,
}
impl Description {
    /// Encode actual backend classification without exposing resources or secrets.
    pub fn encode_into(&self, destination: &mut [u8]) -> Result<usize> {
        let mut out = wire::Out::new(destination)?;
        out.u8(13)?;
        out.u64(self.id)?;
        match self.role {
            None => out.u8(0)?,
            Some(role) => {
                out.u8(1)?;
                role.write(&mut out)?;
            }
        }
        out.finish()
    }
    /// Decode exact configured provider facts, rejecting unknown kinds and role values.
    pub fn decode(source: &[u8]) -> Result<Self> {
        let mut input = wire::Input::new(source)?;
        if input.u8()? != 13 {
            return Err(Error::Invalid);
        }
        let id = input.u64()?;
        let role = match input.u8()? {
            0 => None,
            1 => Some(SessionRole::read(&mut input)?),
            _ => return Err(Error::Invalid),
        };
        input.finish()?;
        Ok(Self { id, role })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_is_not_resource_or_admission_authority() {
        let mut bytes = [0; 128];
        let n = Request { id: 9 }.encode_into(&mut bytes).unwrap();
        assert!(wire::Request::decode(&bytes[..n]).is_err());
        assert!(crate::auth::Admission::decode(&bytes[..n]).is_err());
        for role in [
            None,
            Some(SessionRole::BucketHome),
            Some(SessionRole::SingleNodeHome),
        ] {
            let expected = Description { id: 9, role };
            let n = expected.encode_into(&mut bytes).unwrap();
            assert_eq!(Description::decode(&bytes[..n]).unwrap(), expected);
            assert!(Request::decode(&bytes[..n]).is_err());
        }
    }
}
