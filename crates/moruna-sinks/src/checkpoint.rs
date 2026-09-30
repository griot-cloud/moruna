//! The sink checkpoint (08 e.5) and the commit watermark every file sink shares (f.7).
//!
//! The ledger is the one place that decides what `committed_seq` answers, and it answers from
//! committed files and declared skips only, never from what has merely been written (SI-I8).

use std::collections::BTreeSet;

use moruna_kernel::{MorunaError, Seq};
use serde::{Deserialize, Serialize};

/// The version of the checkpoint document this build writes and accepts.
pub(crate) const VERSION: u32 = 1;

/// One committed file, as e.5 records it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct CommittedFile {
    /// The file or object name, without the destination prefix.
    pub(crate) name: String,
    /// Lowest sequence number whose rows the file holds.
    pub(crate) seq_min: Seq,
    /// Highest sequence number whose rows the file holds.
    pub(crate) seq_max: Seq,
    /// Rows in the file.
    pub(crate) rows: u64,
    /// Bytes in the file.
    pub(crate) bytes: u64,
}

/// The bytes `Sink::checkpoint` returns and `Sink::resume` receives (e.5).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Document {
    pub(crate) version: u32,
    pub(crate) kind: String,
    pub(crate) next_index: u32,
    pub(crate) committed: Vec<CommittedFile>,
}

/// Committed files, the next file index, and the watermark (f.7).
///
/// `contiguous_next` is the lowest sequence number that is neither committed nor skipped: a
/// sequence number enters `done` only when the file holding it is committed, or when the
/// scheduler declared it skipped. That alone is not the watermark. Under unordered delivery a
/// committed file can hold sequence numbers on both sides of it (17, 18 and 20 in one file, 19
/// still in the file being written), and a resume from 18 would keep that file and replay 20
/// into another one. So `watermark_next` is `contiguous_next` held back to the first sequence
/// number of any committed file that straddles it, and `committed_seq` is `watermark_next - 1`:
/// every lower sequence number is committed or skipped, and no committed file holds one of
/// them together with one above it (SI-I8, f.7).
#[derive(Debug)]
pub(crate) struct Ledger {
    kind: &'static str,
    committed: Vec<CommittedFile>,
    next_index: u32,
    /// Sequence numbers at or above `contiguous_next` that are committed or skipped.
    done: BTreeSet<Seq>,
    contiguous_next: Seq,
    /// The ranges of the committed files that end at or above `watermark_next`, the only ones
    /// that can straddle a later watermark. A range leaves as soon as the watermark passes it,
    /// so this holds the files within the reorder distance, not the run's.
    open_ranges: Vec<(Seq, Seq)>,
    watermark_next: Seq,
}

impl Ledger {
    /// An empty ledger for a sink of this kind (`parquet`, `ipc` or `amb1_per_morsel`).
    pub(crate) fn new(kind: &'static str) -> Ledger {
        Ledger {
            kind,
            committed: Vec::new(),
            next_index: 0,
            done: BTreeSet::new(),
            contiguous_next: 0,
            open_ranges: Vec::new(),
            watermark_next: 0,
        }
    }

    /// Take the next file index and advance the counter.
    pub(crate) fn take_index(&mut self) -> u32 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    /// The committed files, in index order.
    pub(crate) fn committed(&self) -> &[CommittedFile] {
        &self.committed
    }

    /// The names of the committed files, which is `SinkSummary::files` (SI-I7).
    pub(crate) fn names(&self) -> Vec<String> {
        self.committed.iter().map(|f| f.name.clone()).collect()
    }

    /// Rows and bytes over every committed file.
    pub(crate) fn totals(&self) -> (u64, u64) {
        self.committed
            .iter()
            .fold((0, 0), |(r, b), f| (r + f.rows, b + f.bytes))
    }

    /// Record a file whose bytes a reader can now see, with the sequence numbers it holds.
    pub(crate) fn commit(&mut self, file: CommittedFile, seqs: &[Seq]) {
        // A file that holds no sequence number (a run-mode tensor file, the empty file of a run
        // that wrote nothing) cannot straddle anything.
        if !seqs.is_empty() {
            self.open_ranges.push((file.seq_min, file.seq_max));
        }
        self.committed.push(file);
        for seq in seqs {
            self.done.insert(*seq);
        }
        self.advance();
    }

    /// The scheduler will never write `seq`; it counts as committed for the watermark (d.8).
    pub(crate) fn skip(&mut self, seq: Seq) {
        if seq >= self.contiguous_next {
            self.done.insert(seq);
            self.advance();
        }
    }

    /// `None` until the first sequence number is committed or skipped (d.8).
    pub(crate) fn committed_seq(&self) -> Option<Seq> {
        self.watermark_next.checked_sub(1)
    }

    fn advance(&mut self) {
        while self.done.remove(&self.contiguous_next) {
            self.contiguous_next += 1;
        }
        // Hold the watermark at the start of any committed file that straddles it, then at the
        // start of whatever file straddles that point, until none does (f.7). The result never
        // falls below the previous watermark: every sequence number below that one was already
        // committed or skipped, so a file committed since holds only sequence numbers at or
        // above it, and no file committed before straddled it.
        let mut next = self.contiguous_next;
        while let Some(min) = self
            .open_ranges
            .iter()
            .filter(|(min, max)| *min < next && next <= *max)
            .map(|(min, _)| *min)
            .min()
        {
            next = min;
        }
        debug_assert!(next >= self.watermark_next, "the watermark went back");
        self.watermark_next = next.max(self.watermark_next);
        let watermark = self.watermark_next;
        self.open_ranges.retain(|(_, max)| *max >= watermark);
    }

    /// Serialise e.5. A resumable sink answers `Some` from the moment it opens, with an empty
    /// file list, which is how the scheduler tells it from a sink that cannot resume (SC f.11).
    pub(crate) fn to_bytes(&self) -> moruna_kernel::Result<Vec<u8>> {
        let doc = Document {
            version: VERSION,
            kind: self.kind.to_string(),
            next_index: self.next_index,
            committed: self.committed.clone(),
        };
        serde_json::to_vec(&doc)
            .map_err(|e| MorunaError::Sink(format!("sink checkpoint could not be serialised: {e}")))
    }

    /// Parse e.5, refusing an unknown version or a kind that does not match this sink (f.8).
    pub(crate) fn parse(kind: &'static str, state: &[u8]) -> moruna_kernel::Result<Document> {
        let doc: Document = serde_json::from_slice(state)
            .map_err(|e| MorunaError::Resume(format!("sink checkpoint is not readable: {e}")))?;
        if doc.version != VERSION {
            return Err(MorunaError::Resume(format!(
                "sink checkpoint version {} is not {VERSION}",
                doc.version
            )));
        }
        if doc.kind != kind {
            return Err(MorunaError::Resume(format!(
                "sink checkpoint kind {} is not {kind}",
                doc.kind
            )));
        }
        Ok(doc)
    }

    /// Rebuild from a parsed checkpoint and the watermark the scheduler restored (f.8), and
    /// say which of the checkpoint's files lie above the watermark and must go.
    ///
    /// A file whose range straddles the watermark means the checkpoint and the store disagree,
    /// which a correct checkpoint cannot produce: the watermark a sink reports is held back to
    /// the start of any committed file that would straddle it (f.7), and a file committed after
    /// the scheduler read it holds only sequence numbers above it. It is refused rather than
    /// guessed at.
    pub(crate) fn restore(
        kind: &'static str,
        doc: Document,
        committed_seq: Option<Seq>,
    ) -> moruna_kernel::Result<(Ledger, Vec<CommittedFile>)> {
        let mut kept = Vec::with_capacity(doc.committed.len());
        let mut above = Vec::new();
        for file in doc.committed {
            if file.seq_min > file.seq_max {
                return Err(MorunaError::Resume(format!(
                    "committed file {} has range {}..{}",
                    file.name, file.seq_min, file.seq_max
                )));
            }
            match committed_seq {
                Some(watermark) if file.seq_max <= watermark => kept.push(file),
                Some(watermark) if file.seq_min > watermark => above.push(file),
                Some(watermark) => {
                    return Err(MorunaError::Resume(format!(
                        "committed file {} holds sequences {}..{}, which straddles the watermark {watermark}",
                        file.name, file.seq_min, file.seq_max
                    )));
                }
                None => above.push(file),
            }
        }
        let watermark_next = committed_seq.map_or(0, |w| w + 1);
        Ok((
            Ledger {
                kind,
                committed: kept,
                next_index: doc.next_index,
                done: BTreeSet::new(),
                contiguous_next: watermark_next,
                // Every kept file ends at or below the watermark, so none can straddle a later
                // one.
                open_ranges: Vec::new(),
                watermark_next,
            },
            above,
        ))
    }
}
