//! Identifiers and constants (contracts d.1).

/// Cache-line alignment for every buffer and file layout in Amoru (CT-I9).
pub const ALIGNMENT: usize = 64;

/// Stage index in the linear chain. Stage 0 is the source's output.
pub type StageId = u16;
/// Monotonic morsel sequence number assigned by the source.
pub type Seq = u64;
/// Source split identifier, unique within a run.
pub type SplitId = u32;

/// Accelerator index as enumerated by discovery.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct DeviceId(pub u8);

/// Index of a node in a run. A single-node run has exactly one node, `LOCAL_NODE`.
/// Reserved for the multi-node extension (architecture section 11); every v1
/// value is `LOCAL_NODE` and every v1 `match` on it handles the general case
/// explicitly (CT-I11).
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Default)]
pub struct NodeId(pub u16);

/// The one node of a single-node run.
pub const LOCAL_NODE: NodeId = NodeId(0);

/// Identity of one run; 16 random bytes, printed as 32 lowercase hex characters.
/// Names the staging directory and the run manifest (placement e.3, e.5).
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct RunId(pub [u8; 16]);

impl RunId {
    /// The 32-character lowercase hex form used in file names and manifests.
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(32);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// Parse the 32-character lowercase hex form; `None` if the text is not exactly that.
    pub fn from_hex(text: &str) -> Option<RunId> {
        let bytes = text.as_bytes();
        if bytes.len() != 32 {
            return None;
        }
        let mut out = [0u8; 16];
        for (i, pair) in bytes.chunks(2).enumerate() {
            let hi = hex_nibble(pair[0])?;
            let lo = hex_nibble(pair[1])?;
            out[i] = (hi << 4) | lo;
        }
        Some(RunId(out))
    }
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

impl core::fmt::Display for RunId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_id_hex_round_trip() {
        let id = RunId([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 255]);
        let hex = id.to_string();
        assert_eq!(hex, "000102030405060708090a0b0c0d0eff");
        assert_eq!(hex.len(), 32);
        assert_eq!(RunId::from_hex(&hex), Some(id));
        assert_eq!(RunId::from_hex("zz"), None);
        assert_eq!(RunId::from_hex(&"g".repeat(32)), None);
        assert_eq!(LOCAL_NODE, NodeId::default());
    }
}
