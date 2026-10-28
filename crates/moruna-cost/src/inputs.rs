//! What is known about a unit when its cost is asked for (design section 3; SDD 24 e.2).
//!
//! One fixed, versioned record. Every field is optional, so the planner fills what table
//! statistics and history give it and the run fills what it measures; a model reads what is
//! there and widens its answer for what is not. Nothing here is a tenant value: counts, sizes,
//! ratios and capacities only.

use serde::{Deserialize, Serialize};

/// The version of [`Inputs`] this build writes and reads. A change of a field's meaning is a new
/// version; an added optional field is not.
pub const INPUTS_VERSION: u32 = 1;

fn inputs_version() -> u32 {
    INPUTS_VERSION
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

/// Section 3: the data, the kernel, the run's resources and the unit's place in the job.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Inputs {
    /// [`INPUTS_VERSION`] when written by this build.
    #[serde(default = "inputs_version")]
    pub version: u32,
    /// Section 3.1.
    #[serde(default, skip_serializing_if = "is_default")]
    pub data: Data,
    /// Section 3.2, less the identity, which is the [`crate::CostKey`].
    #[serde(default, skip_serializing_if = "is_default")]
    pub kernel: Kernel,
    /// Section 3.3.
    #[serde(default, skip_serializing_if = "is_default")]
    pub resources: Resources,
    /// Section 3.4.
    #[serde(default, skip_serializing_if = "is_default")]
    pub job: Job,
}

impl Default for Inputs {
    fn default() -> Inputs {
        Inputs {
            version: INPUTS_VERSION,
            data: Data::default(),
            kernel: Kernel::default(),
            resources: Resources::default(),
            job: Job::default(),
        }
    }
}

impl Inputs {
    /// Inputs that know only the batch's rows and bytes, the two a run always has.
    pub fn batch(rows: u64, bytes: u64) -> Inputs {
        let mut inputs = Inputs::default();
        inputs.data.rows = Some(rows);
        inputs.data.bytes = Some(bytes);
        inputs
    }

    /// Bytes per row: the batch's bytes over its rows when both are known, else the sum of the
    /// mean widths of the columns read when any is known.
    pub fn bytes_per_row(&self) -> Option<f64> {
        if let (Some(rows), Some(bytes)) = (self.data.rows, self.data.bytes)
            && rows > 0
        {
            return Some(bytes as f64 / rows as f64);
        }
        let mut sum = 0.0;
        let mut any = false;
        for column in &self.data.columns {
            if let Some(mean) = column.bytes_mean {
                sum += mean;
                any = true;
            }
        }
        any.then_some(sum)
    }

    /// Columns read: the projection's count when given, else the columns described.
    pub fn columns(&self) -> Option<u32> {
        self.data.columns_read.or_else(|| {
            (!self.data.columns.is_empty())
                .then(|| u32::try_from(self.data.columns.len()).unwrap_or(u32::MAX))
        })
    }

    /// The largest distinct count among the key columns, a lower bound on the groups a
    /// stateful kernel keeps.
    pub fn distinct(&self) -> Option<u64> {
        self.data
            .columns
            .iter()
            .filter(|c| c.key)
            .filter_map(|c| c.distinct)
            .max()
    }

    /// The bytes of the batch: given, or rows times bytes per row from the columns.
    pub fn bytes(&self) -> Option<f64> {
        if let Some(bytes) = self.data.bytes {
            return Some(bytes as f64);
        }
        let rows = self.data.rows? as f64;
        self.bytes_per_row().map(|per| per * rows)
    }
}

/// Section 3.1: the data a unit reads.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Data {
    /// Rows: from table statistics, or the batch at run time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    /// The batch's bytes in memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Per column: width, nulls, distinct count, skew. Empty when nothing is known.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<Column>,
    /// Columns read, after projection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns_read: Option<u32>,
    /// Storage layout, from table metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<Layout>,
    /// Rows out per row in, measured in the run (warm-up gives only the sensitivity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selectivity: Option<f64>,
}

/// One column's statistics (section 3.1).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Column {
    /// The column's name.
    pub name: String,
    /// Mean bytes per value (for strings, binary and lists, their mean length).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_mean: Option<f64>,
    /// The 95th percentile of bytes per value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_p95: Option<f64>,
    /// The share of values that are null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub null_fraction: Option<f64>,
    /// Distinct values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distinct: Option<u64>,
    /// Whether the unit groups, joins or partitions by this column.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub key: bool,
    /// For a key column: the share of rows in the top 1% of keys, measured in the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skew: Option<f64>,
}

/// How the data is stored (section 3.1).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    /// The codec (`zstd`, `snappy`, `none` ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Rows per row group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_group_rows: Option<u64>,
    /// Files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<u32>,
}

/// Section 3.2: what is declared or already measured of the kernel. A model learns these from
/// observations; when they are supplied (by warm-up, or today's profile store), the default
/// model answers from them before it has history of its own (section 5.4, the declared default).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Kernel {
    /// Declared: the same input gives the same output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deterministic: Option<bool>,
    /// Declared: every input column is in the output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserves_columns: Option<bool>,
    /// Declared: the kernel keeps state across batches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stateful: Option<bool>,
    /// Start-up with the image not cached, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_cold_seconds: Option<f64>,
    /// Start-up with the image cached, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_warm_seconds: Option<f64>,
    /// Fixed time per batch, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_batch_seconds: Option<f64>,
    /// Time per row, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_row_seconds: Option<f64>,
    /// Time per input byte, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_byte_seconds: Option<f64>,
    /// Peak memory per input byte (the controller's amplification).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_per_byte: Option<f64>,
    /// Memory still held after the unit per input byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_per_byte: Option<f64>,
    /// State growth per distinct key, bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_per_key_bytes: Option<f64>,
    /// Checkpoint size, bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_bytes: Option<f64>,
    /// Checkpoint time, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_seconds: Option<f64>,
    /// Worker time used per wall second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_seconds_per_second: Option<f64>,
    /// Speed-up at 2 workers over 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speedup_2: Option<f64>,
    /// Speed-up at 4 workers over 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speedup_4: Option<f64>,
}

/// Section 3.3: what the run is given and what is free.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Resources {
    /// Workers this unit runs with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workers: Option<u32>,
    /// What the run starts with.
    #[serde(default, skip_serializing_if = "is_default")]
    pub given: Capacity,
    /// What the run may grow to.
    #[serde(default, skip_serializing_if = "is_default")]
    pub ceiling: Capacity,
    /// What is free now.
    #[serde(default, skip_serializing_if = "is_default")]
    pub free: Capacity,
    /// Whether the kernel's image layers are cached on the node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_cached: Option<bool>,
}

/// Workers, memory and disk (section 3.3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Capacity {
    /// Workers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workers: Option<u32>,
    /// Memory, bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// Local disk, bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
    /// Local disk bandwidth, bytes per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes_per_second: Option<u64>,
}

/// Section 3.4: the unit's place in the job's DAG.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Job {
    /// Units this one waits for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<u32>,
    /// Consumers of this unit's output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumers: Option<u32>,
    /// Whether the unit sits behind a contract barrier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_barrier: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_quantities() {
        let mut inputs = Inputs::batch(1000, 64_000);
        assert_eq!(inputs.bytes_per_row(), Some(64.0));
        assert_eq!(inputs.bytes(), Some(64_000.0));
        assert_eq!(inputs.columns(), None);
        assert_eq!(inputs.distinct(), None);
        inputs.data.bytes = None;
        assert_eq!(inputs.bytes_per_row(), None);
        assert_eq!(inputs.bytes(), None);
        inputs.data.columns = vec![
            Column {
                name: "a".into(),
                bytes_mean: Some(8.0),
                distinct: Some(10),
                key: true,
                ..Column::default()
            },
            Column {
                name: "b".into(),
                bytes_mean: Some(24.0),
                distinct: Some(500),
                ..Column::default()
            },
            Column {
                name: "c".into(),
                distinct: Some(40),
                key: true,
                ..Column::default()
            },
        ];
        assert_eq!(inputs.bytes_per_row(), Some(32.0));
        assert_eq!(inputs.bytes(), Some(32_000.0));
        assert_eq!(inputs.columns(), Some(3));
        assert_eq!(inputs.distinct(), Some(40));
        inputs.data.columns_read = Some(2);
        assert_eq!(inputs.columns(), Some(2));
    }

    #[test]
    fn a_default_record_is_written_as_its_version_alone() {
        let text = serde_json::to_string(&Inputs::default()).expect("serialises");
        assert_eq!(text, r#"{"version":1}"#);
        let back: Inputs = serde_json::from_str("{}").expect("reads");
        assert_eq!(back, Inputs::default());
    }
}
