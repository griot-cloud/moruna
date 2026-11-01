//! The data plane's records (SDD 29 k): good ones laid out as `encode_framing` lays them, and
//! the records the engine's check must refuse. The boot's fixtures (`plane-fixture`) and the
//! conformance suite of the port's adapters (KA2) are both made from here, so the kernel's
//! boot and a Linux node refuse the same records with the same words.

use std::sync::Arc;

use arrow::array::{ArrayRef, DictionaryArray, Int64Array, StringArray};
use arrow::datatypes::{DataType, Field, Int32Type, Schema};
use arrow::record_batch::RecordBatch;
use moruna_kernel::ipc::encode_framing;

use crate::FakeAllocator;

#[allow(missing_docs)]
#[path = "../../moruna-kernel/tests/bombs/mod.rs"]
mod bombs;

/// The page every buffer of a record starts on.
pub const PAGE: usize = 4096;

/// `batch` as a record: the framing, then each buffer at the offset the layout gives.
pub fn record(batch: &RecordBatch) -> Result<Vec<u8>, String> {
    let alloc = FakeAllocator::new();
    let (framing, buffers) =
        encode_framing(batch, PAGE, 0, &alloc).map_err(|e| format!("encode_framing: {e}"))?;
    let framing: &[u8] = &framing;
    let end = buffers
        .iter()
        .map(|(at, b)| at + b.len())
        .chain(std::iter::once(framing.len()))
        .max()
        .unwrap_or(PAGE);
    // The record ends where its body does: the message's body length runs to the last
    // buffer's end padded to 8, from the message's end, which is itself a multiple of 8.
    let end = end.div_ceil(8) * 8;
    let mut out = vec![0u8; end];
    out[..framing.len()].copy_from_slice(framing);
    for (at, b) in &buffers {
        out[*at..*at + b.len()].copy_from_slice(b.as_slice());
    }
    Ok(out)
}

/// The framing (one page) of a record of one `Int64` column `x` of `rows` values `0 ..
/// rows`, whose body is the values from the end of the page on: a producer writes them itself.
pub fn framing(rows: usize) -> Result<Vec<u8>, String> {
    let alloc = FakeAllocator::new();
    let batch = int64s(rows)?;
    let (framing, _) =
        encode_framing(&batch, PAGE, 0, &alloc).map_err(|e| format!("encode_framing: {e}"))?;
    let framing: &[u8] = &framing;
    if framing.len() != PAGE {
        return Err(format!(
            "a framing of {} bytes, not one page",
            framing.len()
        ));
    }
    Ok(framing.to_vec())
}

/// One `Int64` column `x` holding `0 .. rows`.
pub fn int64s(rows: usize) -> Result<RecordBatch, String> {
    let schema = Arc::new(Schema::new(vec![Field::new("x", DataType::Int64, false)]));
    let x: ArrayRef = Arc::new(Int64Array::from_iter_values(0..rows as i64));
    RecordBatch::try_new(schema, vec![x]).map_err(|e| e.to_string())
}

/// `x: Int64` (`3i + 7`) and `s: Utf8` (`row i`, null every fifth row), `rows` rows.
pub fn mixed(rows: usize) -> Result<RecordBatch, String> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("x", DataType::Int64, false),
        Field::new("s", DataType::Utf8, true),
    ]));
    let x: ArrayRef = Arc::new(Int64Array::from_iter_values(
        (0..rows as i64).map(|i| i * 3 + 7),
    ));
    let s: ArrayRef = Arc::new(StringArray::from_iter(
        (0..rows).map(|i| (i % 5 != 0).then(|| format!("row {i}"))),
    ));
    RecordBatch::try_new(schema, vec![x, s]).map_err(|e| e.to_string())
}

fn dictionary() -> Result<RecordBatch, String> {
    let d: DictionaryArray<Int32Type> = vec!["a", "b", "a"].into_iter().collect();
    let schema = Arc::new(Schema::new(vec![Field::new(
        "d",
        DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
        false,
    )]));
    RecordBatch::try_new(schema, vec![Arc::new(d) as ArrayRef]).map_err(|e| e.to_string())
}

/// A record the engine's check must refuse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// What it is.
    pub label: String,
    /// How many of its bytes the task puts.
    pub put: u64,
    /// Words the refusal's reason holds.
    pub says: &'static str,
    /// The bytes written into the window.
    pub bytes: Vec<u8>,
}

/// The records DP-T12 and the port's conformance suite put, each with what its refusal says:
/// garbage, a record put shorter than its buffers reach, a buffer a megabyte outside the
/// region, a dictionary column, and every one of KA10's compressed bombs as a record.
pub fn refusals() -> Result<Vec<Refusal>, String> {
    let good = record(&mixed(8)?)?;
    let mut out = Vec::new();
    // Bytes that are no record: a seeded generator's, so a failure is reproducible.
    let mut seed = 0x5eed_u64;
    let garbage: Vec<u8> = (0..2 * PAGE)
        .map(|_| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 56) as u8
        })
        .collect();
    out.push(Refusal {
        label: "garbage".into(),
        put: garbage.len() as u64,
        says: "continuation marker",
        bytes: garbage,
    });
    // A good record put eight bytes into its body: the body it declares is past the
    // region put.
    out.push(Refusal {
        label: "short".into(),
        put: (PAGE + 8) as u64,
        says: "the record holds",
        bytes: good.clone(),
    });
    // A good record whose first column's values point a megabyte past the region.
    let mut outside = good.clone();
    let schema_len = u32::from_le_bytes([good[4], good[5], good[6], good[7]]) as usize;
    let rb = 8 + schema_len;
    let entries = moruna_ipc_check::frame::buffer_entries(&outside[rb..])?;
    let (offset, len) = entries
        .get(1)
        .copied()
        .ok_or_else(|| String::from("the good record has no values buffer"))?;
    moruna_ipc_check::frame::write_entry(
        &mut outside[rb..],
        1,
        (offset + (1 << 20)) as i64,
        len as i64,
    )?;
    out.push(Refusal {
        label: "outside".into(),
        put: outside.len() as u64,
        says: "outside the body",
        bytes: outside,
    });
    // A dictionary column: its stream's schema message and record batch message, the
    // dictionary message between them left out, as a record would be laid.
    let stream = crate::ipc::stream(&[dictionary()?], arrow::ipc::MetadataVersion::V5);
    let spans = crate::ipc::messages(&stream);
    let (Some(((schema, schema_len), _)), Some(((batch, batch_len), (body, body_len)))) =
        (spans.first().copied(), spans.last().copied())
    else {
        return Err(String::from("the dictionary stream has no messages"));
    };
    let mut dictionary = stream[schema - 8..schema + schema_len].to_vec();
    dictionary.extend_from_slice(&stream[batch - 8..batch + batch_len]);
    dictionary.extend_from_slice(&stream[body..body + body_len]);
    out.push(Refusal {
        label: "dictionary".into(),
        put: dictionary.len() as u64,
        says: "carries no dictionaries",
        bytes: dictionary,
    });
    for bomb in bombs::bombs() {
        let mut bytes = bomb.bytes.clone();
        if bytes.ends_with(&[0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0]) {
            bytes.truncate(bytes.len() - 8);
        }
        out.push(Refusal {
            label: format!("KA10 {}", bomb.label),
            put: bytes.len() as u64,
            says: "compressed",
            bytes,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_records_are_what_the_check_says_they_are() {
        let good = record(&mixed(8).unwrap()).unwrap();
        let framing_len = match moruna_ipc_check::record::framing_need(&good).unwrap() {
            moruna_ipc_check::record::Need::Framing(n) => n,
            other => panic!("{other:?}"),
        };
        let checked =
            moruna_ipc_check::record::check_record(&good[..framing_len], good.len() as u64)
                .unwrap();
        assert_eq!(checked.rows, 8);
        let refusals = refusals().unwrap();
        assert!(refusals.len() > 40, "{}", refusals.len());
        for r in refusals {
            let refused = match moruna_ipc_check::record::framing_need(&r.bytes) {
                Err(e) => e,
                Ok(moruna_ipc_check::record::Need::Framing(n)) if n as u64 <= r.put => {
                    moruna_ipc_check::record::check_record(&r.bytes[..n], r.put)
                        .expect_err(&r.label)
                }
                Ok(other) => format!("{other:?}"),
            };
            assert!(refused.contains(r.says), "{}: {refused}", r.label);
        }
        let f = framing(128 << 10).unwrap();
        assert_eq!(f.len(), PAGE);
    }
}
