//! The error type every contract returns (contracts d.14).

use crate::ids::{Seq, SplitId, StageId};
use crate::morsel::MorselFeatures;
use crate::tier::Tier;

/// Every error in the runtime. A variant that can occur while processing a morsel carries
/// `seq`, `stage` and, where known, the features and the measured footprint, so a diagnostic
/// can be produced without a debugger (CT-I10, G-I8).
#[derive(thiserror::Error, Debug)]
pub enum MorunaError {
    /// A plan-time failure: a schema that cannot cross, a mixed-tier batch, a bad argument.
    #[error("plan: {0}")]
    Plan(String),
    /// A source could not read a split.
    #[error("source split {split}: {msg}")]
    Source {
        /// The split being read.
        split: SplitId,
        /// What went wrong.
        msg: String,
    },
    /// A kernel failed on a morsel.
    #[error("kernel stage {stage} morsel {seq}: {msg}")]
    Kernel {
        /// The stage whose kernel failed.
        stage: StageId,
        /// The morsel being processed.
        seq: Seq,
        /// What went wrong.
        msg: String,
        /// The morsel's features, when known.
        features: Option<MorselFeatures>,
    },
    /// A sink failed to open, write or finish.
    #[error("sink: {0}")]
    Sink(String),
    /// An allocation would exceed the tier's budget.
    #[error("alloc {bytes} bytes in {tier:?}: budget {budget} in use {in_use}")]
    Alloc {
        /// Bytes requested.
        bytes: u64,
        /// The tier asked for.
        tier: Tier,
        /// The tier's budget.
        budget: u64,
        /// Bytes in use in the tier at the time.
        in_use: u64,
    },
    /// An IO operation failed.
    #[error("io {op} {target}: {msg}")]
    Io {
        /// The operation (`read_file`, `mrb1`, `ipc`, ...).
        op: &'static str,
        /// The path, URL or buffer the operation was on.
        target: String,
        /// What went wrong.
        msg: String,
    },
    /// A morsel's measured footprint exceeds the budget; the runtime terminates the run itself.
    #[error("budget: morsel {seq} stage {stage} footprint {footprint} exceeds budget {budget}")]
    Budget {
        /// The morsel.
        seq: Seq,
        /// Its stage.
        stage: StageId,
        /// Measured footprint in bytes.
        footprint: u64,
        /// The budget it exceeded.
        budget: u64,
        /// The morsel's features.
        features: MorselFeatures,
    },
    /// A staging or placement failure: an illegal tier transition, a non-resident payload.
    #[error("staging: {0}")]
    Staging(String),
    /// A table-to-tensor or tensor-to-table conversion failed.
    #[error("convert: {0}")]
    Convert(#[from] ConvertError),
    /// A configuration value is invalid.
    #[error("config {name}: {msg}")]
    Config {
        /// The configuration row.
        name: &'static str,
        /// What is wrong with it.
        msg: String,
    },
    /// The run was cancelled, or a completion's sender was dropped unresolved.
    #[error("cancelled")]
    Cancelled,
    /// A manifest could not be written, read, validated or applied; the message
    /// names the manifest path and the first mismatch.
    #[error("resume: {0}")]
    Resume(String),
    /// A reserved path (`rdma`, `remote`) was reached in a build that does not
    /// implement it. Always a bug or a misconfiguration, never a runtime condition.
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
}

/// Why a payload conversion (d.4) was refused.
#[derive(thiserror::Error, Debug)]
pub enum ConvertError {
    /// The column's Arrow type is not in the e.3 mapping.
    #[error("column {0} is not numeric")]
    NotNumeric(String),
    /// The column has nulls; a tensor has no null slots.
    #[error("column {0} has nulls")]
    HasNulls(String),
    /// `as_tensor(None)` over columns of different dtypes.
    #[error("columns have mixed dtypes")]
    MixedDTypes,
    /// The tensor's strides are not row-major, or the columns are not adjacent in one buffer.
    #[error("tensor is not contiguous")]
    NotContiguous,
    /// The tensor's rank is not 1 or 2.
    #[error("tensor rank {0} not convertible (need 1 or 2)")]
    Rank(usize),
    /// A DLPack capsule of an ABI major version this build does not speak.
    #[error("dlpack major version {found} is not {expected}")]
    Version {
        /// The major version the capsule declares.
        found: u32,
        /// The major version this build speaks.
        expected: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_carry_the_morsel() {
        let e = MorunaError::Kernel {
            stage: 2,
            seq: 41,
            msg: "boom".into(),
            features: None,
        };
        assert_eq!(e.to_string(), "kernel stage 2 morsel 41: boom");
        let b = MorunaError::Budget {
            seq: 7,
            stage: 1,
            footprint: 10,
            budget: 5,
            features: MorselFeatures::default(),
        };
        assert_eq!(
            b.to_string(),
            "budget: morsel 7 stage 1 footprint 10 exceeds budget 5"
        );
        let a = MorunaError::Alloc {
            bytes: 1,
            tier: Tier::Host,
            budget: 2,
            in_use: 3,
        };
        assert_eq!(a.to_string(), "alloc 1 bytes in Host: budget 2 in use 3");
        let s = MorunaError::Source {
            split: 3,
            msg: "gone".into(),
        };
        assert_eq!(s.to_string(), "source split 3: gone");
        let c = MorunaError::Config {
            name: "budget.host",
            msg: "low".into(),
        };
        assert_eq!(c.to_string(), "config budget.host: low");
        let io = MorunaError::Io {
            op: "mrb1",
            target: "f".into(),
            msg: "short".into(),
        };
        assert_eq!(io.to_string(), "io mrb1 f: short");
        for (e, text) in [
            (MorunaError::Plan("p".into()), "plan: p"),
            (MorunaError::Sink("s".into()), "sink: s"),
            (MorunaError::Staging("g".into()), "staging: g"),
            (MorunaError::Cancelled, "cancelled"),
            (MorunaError::Resume("r".into()), "resume: r"),
            (MorunaError::Unsupported("rdma"), "unsupported: rdma"),
            (
                ConvertError::MixedDTypes.into(),
                "convert: columns have mixed dtypes",
            ),
            (
                ConvertError::NotContiguous.into(),
                "convert: tensor is not contiguous",
            ),
            (
                ConvertError::Rank(3).into(),
                "convert: tensor rank 3 not convertible (need 1 or 2)",
            ),
            (
                ConvertError::HasNulls("c".into()).into(),
                "convert: column c has nulls",
            ),
            (
                ConvertError::NotNumeric("c".into()).into(),
                "convert: column c is not numeric",
            ),
        ] {
            assert_eq!(e.to_string(), text);
        }
    }
}
