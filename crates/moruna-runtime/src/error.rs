//! The facade's error, which carries what the surface must put on its exception.
//!
//! 12 d.1 says a `Terminated` outcome is returned as an error "with the partial report and
//! the manifest path attached", and PY-I2 and PY-I9 require `.report` and `.manifest` on the
//! Python exception. `MorunaError` has nowhere to put either, so the facade's `Result` carries
//! this type; the underlying `MorunaError` is `error` and maps to the Python class as e.2 says.
//!
//! The parts live behind one `Box`, so a `Result<RunReport>` is no wider for the error than
//! for the report.

use std::path::PathBuf;

use moruna_kernel::MorunaError;
use moruna_trace::RunReport;

/// What a failed run carries.
#[derive(Debug)]
pub struct Failure {
    /// The diagnostic: the variant e.2 maps to a Python class.
    pub error: MorunaError,
    /// The report computed from the trace the run did write, when there was one.
    pub report: Option<RunReport>,
    /// The last manifest written, when the run is resumable (PY-I9).
    pub manifest: Option<PathBuf>,
    /// Errors collected while unwinding, which are reported and never raised (12 l).
    pub shutdown_notes: Vec<String>,
}

/// An error from a run, with whatever the run had produced by then. Dereferences to
/// [`Failure`], so `error.error`, `error.report` and `error.manifest` read as fields.
#[derive(Debug)]
pub struct RunError(Box<Failure>);

impl RunError {
    /// An error with nothing attached: a failure before the run produced anything.
    pub fn bare(error: MorunaError) -> RunError {
        RunError(Box::new(Failure {
            error,
            report: None,
            manifest: None,
            shutdown_notes: Vec::new(),
        }))
    }

    /// Attach the partial report.
    pub fn with_report(mut self, report: Option<RunReport>) -> RunError {
        self.0.report = report;
        self
    }

    /// Attach the manifest path.
    pub fn with_manifest(mut self, manifest: Option<PathBuf>) -> RunError {
        self.0.manifest = manifest;
        self
    }

    /// Attach the notes collected while unwinding.
    pub fn with_shutdown_notes(mut self, notes: Vec<String>) -> RunError {
        self.0.shutdown_notes = notes;
        self
    }

    /// True when the run wrote a manifest a later run can resume from (PY-I9).
    pub fn is_resumable(&self) -> bool {
        self.0.manifest.is_some()
    }

    /// The parts, by value, for a surface that wants to take them apart.
    pub fn into_parts(self) -> Failure {
        *self.0
    }
}

impl core::ops::Deref for RunError {
    type Target = Failure;

    fn deref(&self) -> &Failure {
        &self.0
    }
}

impl core::fmt::Display for RunError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0.error)?;
        if let Some(manifest) = &self.0.manifest {
            write!(f, " (resumable: pass resume={:?})", manifest.display())?;
        }
        Ok(())
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0.error)
    }
}

impl From<MorunaError> for RunError {
    fn from(error: MorunaError) -> RunError {
        RunError::bare(error)
    }
}

/// What every facade entry point returns.
pub type Result<T> = core::result::Result<T, RunError>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    /// The parts a surface reads: the diagnostic, the report, the manifest and the notes.
    #[test]
    fn a_failure_carries_its_parts() {
        let error = RunError::bare(MorunaError::Cancelled)
            .with_manifest(Some(PathBuf::from("/tmp/moruna/manifest.json")))
            .with_shutdown_notes(vec!["a note".to_string()]);
        assert!(error.is_resumable());
        assert_eq!(error.shutdown_notes, vec!["a note".to_string()]);
        assert!(error.to_string().contains("resumable: pass resume="));
        assert!(error.source().is_some());
        let parts = error.into_parts();
        assert!(matches!(parts.error, MorunaError::Cancelled));
        assert!(parts.report.is_none());
    }

    /// A failure with nothing attached prints the diagnostic and nothing else.
    #[test]
    fn a_bare_failure_is_just_the_diagnostic() {
        let error: RunError = MorunaError::Plan("no splits".into()).into();
        assert!(!error.is_resumable());
        assert_eq!(
            error.to_string(),
            MorunaError::Plan("no splits".into()).to_string()
        );
        assert!(format!("{error:?}").contains("no splits"));
    }
}
