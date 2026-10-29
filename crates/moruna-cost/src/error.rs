//! The one error type of the crate (SDD 24 d.1).

use std::path::PathBuf;

use crate::model::ModelId;

/// What went wrong, with its cause.
#[derive(Debug, thiserror::Error)]
pub enum CostError {
    /// A setting names a model the registry does not have.
    #[error("no cost model `{name}` is registered; the registry has {known}")]
    UnknownModel {
        /// The name asked for.
        name: String,
        /// The names the registry has, sorted and comma-separated, or `none`.
        known: String,
    },
    /// A model of that name is registered already.
    #[error("a cost model `{name}` is registered already")]
    Taken {
        /// The name.
        name: String,
    },
    /// A model's name is not a name (the rule of a kind's name, SDD 18).
    #[error(
        "`{name}` is not a cost model name: lowercase ASCII letters, digits, `_`, `-` and `.`, \
         starting with a letter, at most 64 characters"
    )]
    BadName {
        /// The name.
        name: String,
    },
    /// An adapter made a model whose id does not carry the adapter's name.
    #[error("the adapter registered as `{adapter}` made the model {made}")]
    Mismatch {
        /// The adapter's name.
        adapter: String,
        /// The id of the model it made.
        made: ModelId,
    },
    /// Fitted state saved by one model offered to another (CM-I3).
    #[error("fitted state saved by {found} cannot be loaded by {expected}")]
    WrongModel {
        /// The model loading.
        expected: ModelId,
        /// The model that saved it.
        found: ModelId,
    },
    /// Fitted state that is not state of this model's format.
    #[error("fitted state of {model} is unreadable: {reason}")]
    State {
        /// The model loading.
        model: ModelId,
        /// Why.
        reason: String,
    },
    /// An observation the store refuses to keep.
    #[error("observation refused: {0}")]
    Invalid(String),
    /// The observation store could not be read or written.
    #[error("observation store {}: {source}", path.display())]
    Io {
        /// The file or directory.
        path: PathBuf,
        /// The cause.
        #[source]
        source: std::io::Error,
    },
    /// A whole record of the store that cannot be read as an observation of this build.
    #[error("observation store {} at byte {offset}: {reason}", path.display())]
    Unreadable {
        /// The file.
        path: PathBuf,
        /// Where the record starts.
        offset: u64,
        /// Why.
        reason: String,
    },
}

/// The crate's result.
pub type Result<T> = std::result::Result<T, CostError>;

impl CostError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> CostError {
        CostError::Io {
            path: path.into(),
            source,
        }
    }
}
