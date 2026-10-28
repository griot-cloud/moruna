//! The Arrow schema a Parquet footer may carry, checked before it is converted (SO-I10).
//!
//! A file written from Arrow keeps its Arrow schema base64-encoded under `ARROW:schema` in the
//! footer's key-value metadata, and the Parquet reader converts it with the Arrow IPC reader
//! (`arrow::ipc::convert::fb_to_schema`), which panics on a schema it cannot map. A footer is
//! untrusted input, so the schema message is put through Moruna's one check of IPC messages
//! (`moruna_kernel::ipc::check`, contracts CT-I13) first, after the same decoding and framing
//! the Parquet reader applies, and one it would panic on is refused with the field and what is
//! wrong with it. A value that is not base64 or not an IPC message is left to the Parquet
//! reader, which returns an error for it.

use moruna_kernel::arrow::ipc;
use parquet::file::metadata::FileMetaData;

/// The key the Arrow writer stores its schema under.
const KEY: &str = "ARROW:schema";

/// `Err` naming the field and the fault when the footer's Arrow schema is one the IPC reader
/// would panic on; `Ok` when there is none, or when it is one the reader converts or refuses.
pub(crate) fn check(metadata: &FileMetaData) -> Result<(), String> {
    for entry in metadata.key_value_metadata().into_iter().flatten() {
        if let (KEY, Some(value)) = (entry.key.as_str(), entry.value.as_deref())
            && let Some(bytes) = base64(value)
        {
            check_message(&bytes)?;
        }
    }
    Ok(())
}

/// The schema message the Parquet reader would hand to `fb_to_schema`, after the same framing
/// it strips (a continuation marker and a length).
fn check_message(bytes: &[u8]) -> Result<(), String> {
    let framed = bytes.len() > 8 && bytes.get(..4) == Some(&[0xFF; 4][..]);
    let body = if framed { &bytes[8..] } else { bytes };
    match ipc::root_as_message(body) {
        Ok(message) => moruna_kernel::ipc::check(&message, &[], None).map(drop),
        Err(_) => Ok(()),
    }
}

/// Standard base64, accepting everything the Parquet reader's strict decoder accepts and
/// giving the same bytes for it; `None` for a character outside the alphabet, which that
/// decoder refuses too.
fn base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut held) = (0u32, 0u32);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(value);
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push((acc >> held) as u8);
            acc &= (1 << held) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_kernel::arrow::ipc::MetadataVersion;
    use moruna_testkit::ipc::{small, stream, unknown_type};

    #[test]
    fn an_unknown_type_is_refused_framed_or_not() {
        let bad = unknown_type(&stream(&[small()], MetadataVersion::V5));
        let len = u32::from_le_bytes([bad[4], bad[5], bad[6], bad[7]]) as usize;
        let framed = &bad[..8 + len];
        let why = check_message(framed).expect_err("refused behind its framing");
        assert!(why.contains("field \"i\""), "{why}");
        let why = check_message(&framed[8..]).expect_err("refused without it");
        assert!(why.contains("field \"i\""), "{why}");
    }

    #[test]
    fn what_is_not_a_schema_message_is_left_to_the_reader() {
        assert_eq!(check_message(b"not a flatbuffer"), Ok(()));
        assert_eq!(check_message(&[]), Ok(()));
    }

    #[test]
    fn base64_decodes_the_standard_alphabet() {
        assert_eq!(base64("TW9ydW5h"), Some(b"Moruna".to_vec()));
        assert_eq!(base64("TW9ydW4="), Some(b"Morun".to_vec()));
        assert_eq!(base64("TW9ydQ=="), Some(b"Moru".to_vec()));
        assert_eq!(base64("+/+/"), Some(vec![0xFB, 0xFF, 0xBF]));
        assert_eq!(base64("TW9y\u{0}"), None);
        assert_eq!(base64(""), Some(Vec::new()));
    }
}
