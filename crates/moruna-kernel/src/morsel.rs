//! The morsel, its origin and its features (contracts d.5, f.2).

use arrow::array::{Array, ArrayData};
use arrow::datatypes::DataType;

use crate::ids::{NodeId, Seq, SplitId, StageId};
use crate::payload::{DType, Payload};

/// Where a morsel's bytes came from. The lineage of every morsel in the run: a
/// morsel at stage `s` is exactly `kernels[1..=s]` applied to
/// `source.read(split, row_start..row_end)`, so any morsel can be re-obtained
/// from its origin (CT-I12). This is what Q0 eviction and run resume both rely on.
#[derive(Clone, Debug)]
pub struct Origin {
    /// The split the rows came from.
    pub split: SplitId,
    /// First row, inclusive, within the split.
    pub row_start: u64,
    /// Last row, exclusive, within the split.
    pub row_end: u64,
    /// The node that read the split. `LOCAL_NODE` in every v1 run.
    pub node: NodeId,
}

/// Features the controller and the trace consume. Table fields are None for tensors and vice versa.
#[derive(Clone, Debug, Default)]
pub struct MorselFeatures {
    /// Rows in the payload.
    pub rows: u64,
    /// Payload bytes (CT-I3).
    pub bytes: u64,
    /// Bytes per column, in schema order; empty for tensors.
    pub column_bytes: Vec<u64>,
    /// Mean string length over every `Utf8` and `LargeUtf8` column; `None` when there are none.
    pub mean_string_len: Option<f32>,
    /// Nulls over cells; `None` for tensors.
    pub null_ratio: Option<f32>,
    /// Tensor shape; `None` for tables.
    pub shape: Option<Vec<i64>>,
    /// Tensor dtype; `None` for tables.
    pub dtype: Option<DType>,
}

impl MorselFeatures {
    /// Extract the features of a payload. O(columns), never O(rows): string totals come from
    /// the offsets buffer's last value, not by iterating (f.2).
    pub fn from_payload(payload: &Payload) -> MorselFeatures {
        let bytes = payload.bytes();
        let rows = payload.rows();
        match payload {
            Payload::Table(batch, _) => {
                let mut column_bytes = Vec::with_capacity(batch.num_columns());
                let mut string_bytes = 0u64;
                let mut string_values = 0u64;
                let mut nulls = 0u64;
                let mut cells = 0u64;
                for column in batch.columns() {
                    column_bytes.push(column.get_array_memory_size() as u64);
                    nulls += column.null_count() as u64;
                    cells += column.len() as u64;
                    let data = column.to_data();
                    if let Some((total, values)) = string_totals(&data) {
                        string_bytes += total;
                        string_values += values;
                    }
                }
                let mean_string_len = if string_values == 0 {
                    // No string column at all means None; a string column with no values
                    // means a mean of zero.
                    if has_string_column(batch) {
                        Some(0.0)
                    } else {
                        None
                    }
                } else {
                    Some(string_bytes as f32 / string_values as f32)
                };
                let null_ratio = if cells == 0 {
                    Some(0.0)
                } else {
                    Some(nulls as f32 / cells as f32)
                };
                MorselFeatures {
                    rows,
                    bytes,
                    column_bytes,
                    mean_string_len,
                    null_ratio,
                    shape: None,
                    dtype: None,
                }
            }
            Payload::Tensor(t, _) => MorselFeatures {
                rows,
                bytes,
                column_bytes: Vec::new(),
                mean_string_len: None,
                null_ratio: None,
                shape: Some(t.shape().to_vec()),
                dtype: Some(t.dtype()),
            },
        }
    }
}

fn has_string_column(batch: &arrow::record_batch::RecordBatch) -> bool {
    batch
        .schema()
        .fields()
        .iter()
        .any(|f| matches!(f.data_type(), DataType::Utf8 | DataType::LargeUtf8))
}

/// `(total string bytes, string value count)` for a `Utf8` or `LargeUtf8` array, read from the
/// offsets buffer's first and last value; `None` for any other type (f.2).
fn string_totals(data: &ArrayData) -> Option<(u64, u64)> {
    let len = data.len();
    let offset = data.offset();
    match data.data_type() {
        DataType::Utf8 => {
            let offsets: &[i32] = data.buffers().first()?.typed_data();
            let first = *offsets.get(offset)? as i64;
            let last = *offsets.get(offset + len)? as i64;
            Some(((last - first) as u64, len as u64))
        }
        DataType::LargeUtf8 => {
            let offsets: &[i64] = data.buffers().first()?.typed_data();
            let first = *offsets.get(offset)?;
            let last = *offsets.get(offset + len)?;
            Some(((last - first) as u64, len as u64))
        }
        _ => None,
    }
}

/// One unit of work: a payload plus its header.
#[derive(Debug)]
pub struct Morsel {
    /// The sequence number the source assigned.
    pub seq: Seq,
    /// The stage whose output this payload is; stage 0 is the source's.
    pub stage: StageId,
    /// The data.
    pub payload: Payload,
    /// `payload.bytes()`; kept for lock-free accounting reads (CT-I3).
    pub bytes: u64,
    /// The lineage of the morsel (CT-I12).
    pub origin: Origin,
    /// Features the controller and the trace consume.
    pub features: MorselFeatures,
}

impl Morsel {
    /// A morsel over `payload`; computes `bytes` and `features` once.
    pub fn new(seq: Seq, stage: StageId, payload: Payload, origin: Origin) -> Morsel {
        let bytes = payload.bytes();
        let features = MorselFeatures::from_payload(&payload);
        Morsel {
            seq,
            stage,
            payload,
            bytes,
            origin,
            features,
        }
    }

    /// Replace the payload (a kernel produced a new one); recomputes bytes and features,
    /// advances stage by one.
    pub fn with_output(self, payload: Payload) -> Morsel {
        let bytes = payload.bytes();
        let features = MorselFeatures::from_payload(&payload);
        Morsel {
            seq: self.seq,
            stage: self.stage.saturating_add(1),
            payload,
            bytes,
            origin: self.origin,
            features,
        }
    }
}
