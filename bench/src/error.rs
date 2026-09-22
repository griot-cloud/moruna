//! The generator's error type. Every fallible function returns `Result`, and
//! `main` prints the error and exits non zero (preamble 6.7: no `unwrap` or
//! `expect` outside tests).

use std::path::PathBuf;

/// Everything that can go wrong while generating a dataset.
#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    /// A file or directory operation failed, with the path that failed.
    #[error("io: {op} {path}: {source}")]
    Io {
        /// What was being attempted, for example `create_dir_all`.
        op: &'static str,
        /// The path the operation was attempted on.
        path: PathBuf,
        /// The underlying operating system error.
        #[source]
        source: std::io::Error,
    },
    /// The command line was not understood. The usage text is printed with it.
    #[error("usage: {0}")]
    Usage(String),
    /// A shape argument was out of the range the format or the document allows.
    #[error("invalid shape: {0}")]
    Shape(String),
    /// Arrow rejected a schema or an array.
    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    /// The Parquet writer failed.
    #[error("parquet: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
    /// The safetensors writer failed.
    #[error("safetensors: {0}")]
    SafeTensors(#[from] safetensors::SafeTensorError),
    /// The object store rejected a request.
    #[error("object_store: {0}")]
    ObjectStore(#[from] object_store::Error),
    /// The object store path was not a valid store path.
    #[error("object_store path: {0}")]
    ObjectStorePath(#[from] object_store::path::Error),
    /// Writing the manifest as JSON failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// The tokio runtime that drives the object store could not be built.
    #[error("runtime: {0}")]
    Runtime(std::io::Error),
    /// A kernel refused its input: the payload arm, a column it needs, a dtype
    /// it cannot read or a shape that does not match its weights.
    #[error("kernel {kernel}: {detail}")]
    Kernel {
        /// The kernel's name as preamble 6.5 gives it.
        kernel: &'static str,
        /// What the kernel wanted and what it was given.
        detail: String,
    },
    /// A kernel exists but the binding that would let this crate call it does
    /// not. The one case is `wide-intermediate`, whose body is Python and whose
    /// caller is the runtime's Python adapter (component 5), not yet built.
    #[error("kernel {kernel} is not wired: {detail}")]
    NotWired {
        /// The kernel's name as preamble 6.5 gives it.
        kernel: &'static str,
        /// What is missing and where the body of the kernel lives meanwhile.
        detail: String,
    },
}

impl BenchError {
    /// Wrap an `std::io::Error` with the operation and path that produced it.
    pub fn io(op: &'static str, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        BenchError::Io {
            op,
            path: path.into(),
            source,
        }
    }
}

/// The generator's result alias.
pub type Result<T> = std::result::Result<T, BenchError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_names_the_operation_and_the_path() {
        let err = BenchError::io(
            "create_dir_all",
            "/nope/out",
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );
        let text = err.to_string();
        assert!(text.contains("create_dir_all"), "{text}");
        assert!(text.contains("/nope/out"), "{text}");
    }

    #[test]
    fn usage_and_shape_errors_carry_their_message() {
        assert!(
            BenchError::Usage("no subcommand".into())
                .to_string()
                .contains("no subcommand")
        );
        assert!(
            BenchError::Shape("ndim 9".into())
                .to_string()
                .contains("ndim 9")
        );
    }

    #[test]
    fn a_kernel_error_names_the_kernel() {
        let err = BenchError::Kernel {
            kernel: "embed-score",
            detail: "weight has 128 rows, the input has 18 numeric columns".to_string(),
        };
        let text = err.to_string();
        assert!(text.starts_with("kernel embed-score:"), "{text}");
        assert!(text.contains("18 numeric columns"), "{text}");
    }

    #[test]
    fn a_not_wired_error_says_what_is_missing() {
        let err = BenchError::NotWired {
            kernel: "wide-intermediate",
            detail: "the Python adapter (component 5) does not exist yet".to_string(),
        };
        let text = err.to_string();
        assert!(text.contains("is not wired"), "{text}");
        assert!(text.contains("component 5"), "{text}");
    }

    #[test]
    fn foreign_errors_convert() {
        let arrow: BenchError = arrow::error::ArrowError::ComputeError("x".into()).into();
        assert!(arrow.to_string().starts_with("arrow:"));
        let json: BenchError = serde_json::from_str::<u8>("nope").unwrap_err().into();
        assert!(json.to_string().starts_with("json:"));
        let store: BenchError = object_store::Error::NotSupported {
            source: "unit".into(),
        }
        .into();
        assert!(store.to_string().starts_with("object_store:"));
        let st: BenchError = safetensors::SafeTensorError::HeaderTooLarge.into();
        assert!(st.to_string().starts_with("safetensors:"));
        let pq: BenchError = parquet::errors::ParquetError::General("g".into()).into();
        assert!(pq.to_string().starts_with("parquet:"));
    }
}
