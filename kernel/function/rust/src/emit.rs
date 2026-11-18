//! Where a function writes its output.

use arrow_array::RecordBatch;

use crate::error::FunctionError;
use crate::lane::Lane;
use crate::record;

/// The output of one call of a function: each [`Emit::batch`] writes a batch into a window, as
/// one record, or as several in order when its rows take more than a window. They all answer the
/// batch the function was called with, in the order written.
pub struct Emit<'a> {
    lane: &'a mut dyn Lane,
    cookie: u64,
    failed: Option<FunctionError>,
    records: u64,
}

impl<'a> Emit<'a> {
    pub(crate) fn new(lane: &'a mut dyn Lane, cookie: u64) -> Emit<'a> {
        Emit {
            lane,
            cookie,
            failed: None,
            records: 0,
        }
    }

    /// Writes `batch` into windows, waiting for a free one when the engine has given none; the
    /// batch's buffers are copied once, into the window, and the function keeps `batch`. An error
    /// (a row too big for a window, a dictionary column, the rings broken) ends the task even if
    /// the function goes on, and every later call returns it again.
    pub fn batch(&mut self, batch: &RecordBatch) -> Result<(), FunctionError> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        match record::write(self.lane, batch, self.cookie) {
            Ok(n) => {
                self.records += n;
                Ok(())
            }
            Err(e) => {
                self.failed = Some(e.clone());
                Err(e)
            }
        }
    }

    /// The records written, or the first error.
    pub(crate) fn finish(self) -> Result<u64, FunctionError> {
        match self.failed {
            Some(e) => Err(e),
            None => Ok(self.records),
        }
    }
}

impl std::fmt::Debug for Emit<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Emit")
            .field("task", &self.lane.name())
            .field("cookie", &self.cookie)
            .field("records", &self.records)
            .field("failed", &self.failed)
            .finish()
    }
}
