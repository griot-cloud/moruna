//! A stage's warm-up on its first instance (system design 3A, item 6.3f; the Moruna kernel's
//! image task warms up the same way, 6.3g): a function whose memory the run has no measurement
//! of, or one resting on too few units, is measured on its job's first batches before its other
//! instances start. A function's memory is measured, never assumed.
//!
//! - Only the first instance is given work while the stage warms up
//!   (`Kernel::instances_ready`). It takes pieces of 1, 2, 4, 8 and 16 MiB ([`PIECES`]), one at a
//!   time, and after each answer its cgroup's `memory.peak` is read beside the bytes it was
//!   given: on Linux the instance's cgroup holds its whole process, as an account does on the
//!   Moruna kernel.
//! - The readings are fitted as the node fits its own (`moruna_api::Measured::from_readings`),
//!   with the slope of what the run had of the function kept where it had one.
//! - The stage's instances are then funded from the fit over the run's room for them
//!   (`instances::fund_measured`, at pieces from the largest morsel down to the least: the piece
//!   shrinks first, and only then do fewer instances run). The others are carved at the fit's
//!   account at that piece and every instance's batches are cut at the piece, as the node's
//!   are (item 6.3v); the room they do not need goes back to the run's arena, and the others
//!   start. An instance its cgroup kills while it warms up is started again by the restart's
//!   rule (the `restart` module) and the warm-up goes on.

use std::sync::atomic::Ordering;
use std::time::Instant;

use moruna_adapters::instances;
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{Payload, Result};

use super::stage::{self, Stage};
use super::{ImageKernel, ImageState, join, laid_bytes, lock, pieces};

/// The warm-up's pieces, the smallest first: the Moruna kernel's (`WARM_PIECES` of
/// `moruna_micro::layout`).
pub(super) const PIECES: [u64; 5] = [1 << 20, 2 << 20, 4 << 20, 8 << 20, 16 << 20];

/// A stage's warm-up on its first instance.
#[derive(Default)]
pub(super) struct Warm {
    /// What the run had of the function, whose slope the fit keeps.
    pub(super) client: Option<moruna_api::Measured>,
    /// `(bytes given, the instance's cgroup's memory.peak after its answer)`, one a piece, the
    /// smallest first.
    pub(super) readings: Vec<(u64, u64)>,
    /// When its first piece was given.
    pub(super) started: Option<Instant>,
    /// What it found and did, for the stage's note.
    pub(super) said: Option<String>,
    /// The line it fitted, which the stage's kernel says to the controller (item 6.3w).
    pub(super) line: Option<moruna_kernel::MeasuredLine>,
}

/// The node's form of a measurement, which its fit keeps the slope of.
fn as_measured(m: &moruna_cost::Memory) -> moruna_api::Measured {
    moruna_api::Measured {
        fixed_bytes: m.fixed_bytes,
        per_input_byte: m.per_input_byte,
        output_per_input_byte: m.output_per_input_byte,
        spread: m.spread,
        units: m.units,
        input_bytes_min: m.input_bytes_min,
        input_bytes_max: m.input_bytes_max,
    }
}

/// The engine's form of a fit, which a stage's instances are funded and cut by.
fn as_memory(m: &moruna_api::Measured) -> moruna_cost::Memory {
    moruna_cost::Memory {
        fixed_bytes: m.fixed_bytes,
        per_input_byte: m.per_input_byte,
        output_per_input_byte: m.output_per_input_byte,
        spread: m.spread,
        units: m.units,
        input_bytes_min: m.input_bytes_min,
        input_bytes_max: m.input_bytes_max,
    }
}

/// What one instance needs at a piece of `piece` bytes by `line`, in whole bytes; `u64::MAX`
/// past what a number holds.
fn need_at(line: &moruna_cost::Memory, piece: u64) -> u64 {
    let need = line.account(piece as f64).ceil();
    if need.is_finite() && need >= 0.0 {
        need as u64
    } else {
        u64::MAX
    }
}

impl ImageKernel {
    /// Warm the stage up on its first instance before its others start (system design 3A): the
    /// run has no measurement of its function resting on enough units, and `client` is what it
    /// has, if anything, whose slope the fit keeps. `freed` is told the bytes of the run's room
    /// for the stage that its instances do not need once the fit has funded them. A stage of
    /// one instance has nothing to size, and is not warmed up ([`ImageKernel::bind_run`]).
    pub fn warm_up(
        &self,
        client: Option<moruna_cost::Memory>,
        freed: Box<dyn Fn(u64) + Send + Sync>,
    ) {
        lock(&self.warm).client = client.as_ref().map(as_measured);
        let _ = self.on_sized.set(freed);
        self.warming.store(true, Ordering::Relaxed);
    }

    /// Whether the stage is warming up on its first instance.
    pub(super) fn warming(&self) -> bool {
        self.warming.load(Ordering::Relaxed)
    }

    /// What the warm-up found and did, for the stage's note, once it has.
    pub(super) fn warm_said(&self) -> Option<String> {
        lock(&self.warm).said.clone()
    }

    /// The first instance's unit while the stage warms up: `batch` in pieces of the warm-up's
    /// sizes ([`PIECES`]), the smallest first, each given and answered before the next and the
    /// instance's peak read after each; once the warm-up ends, what is left of the batch is
    /// cut by the fit's cap. The answers are joined in order into one batch.
    pub(super) fn warm(&self, st: &mut ImageState, batch: RecordBatch) -> Result<Payload> {
        let rows = batch.num_rows();
        if rows == 0 {
            return self.exchange(st, batch);
        }
        let per_row = (laid_bytes(&batch, &*st.alloc)? / rows as u64).max(1);
        let mut answers = Vec::new();
        let mut at = 0;
        while at < rows {
            if !self.warming() {
                let rest = batch.slice(at, rows - at);
                for slice in pieces(&rest, self.piece_cap(st.ceiling), &*st.alloc)? {
                    answers.push(self.exchange(st, slice)?);
                }
                break;
            }
            let next = lock(&self.warm).readings.len();
            let size = PIECES
                .get(next)
                .copied()
                .unwrap_or(PIECES[PIECES.len() - 1]);
            let take = usize::try_from(size / per_row)
                .unwrap_or(usize::MAX)
                .clamp(1, rows - at);
            let piece = batch.slice(at, take);
            at += take;
            let laid = laid_bytes(&piece, &*st.alloc)?;
            lock(&self.warm).started.get_or_insert_with(Instant::now);
            answers.push(self.exchange(st, piece)?);
            self.reading(st, laid);
        }
        if answers.len() > 1 {
            st.busy.cut(answers.len());
        }
        match answers.len() {
            1 => answers
                .pop()
                .ok_or_else(|| super::kernel_error(format!("image `{}`: no answer", self.name))),
            _ => join(&self.name, answers, &st.alloc),
        }
    }

    /// One warm-up reading: the first instance's cgroup's peak after its answer to a piece of
    /// `laid` bytes (its `memory.current` where the kernel keeps no peak). The last of
    /// [`PIECES`] ends the warm-up, and the stage is sized from what it found.
    pub(super) fn reading(&self, st: &mut ImageState, laid: u64) {
        let cgroup = &st.child.cgroup;
        let Ok(peak) = cgroup
            .memory_peak_ever()
            .or_else(|_| cgroup.memory_current())
        else {
            return;
        };
        let done = {
            let mut warm = lock(&self.warm);
            warm.readings.push((laid, peak));
            warm.readings.len() >= PIECES.len()
        };
        if done {
            self.warmed(st.place.stage());
            st.ceiling = st.place.limit();
        }
    }

    /// The warm-up's end (system design 3A): the readings fitted, the client's slope kept where
    /// it gave one; the stage's instances funded from the fit over the run's room for them
    /// (`instances::fund_measured`: the piece shrinks first, then the count); the stage's
    /// ceiling shared among that many; the room they do not need handed back to the run's
    /// arena; and then the others start.
    pub(super) fn warmed(&self, stage: &Stage) {
        let (readings, client, started) = {
            let warm = lock(&self.warm);
            (warm.readings.clone(), warm.client.clone(), warm.started)
        };
        let took = started.map_or(0, |s| s.elapsed().as_millis());
        let laid: Vec<String> = readings
            .iter()
            .map(|(laid, peak)| format!("{laid} bytes to a peak of {peak}"))
            .collect();
        let mut said = format!(
            "warmed up on its first instance over {} pieces in {took} ms ({})",
            readings.len(),
            laid.join(", ")
        );
        if let Some(fit) = moruna_api::Measured::from_readings(&readings, client.as_ref()) {
            // Said to the controller as soon as it is fitted (item 6.3w).
            lock(&self.warm).line = Some(moruna_kernel::MeasuredLine {
                fixed_bytes: fit.fixed_bytes,
                per_input_byte: fit.per_input_byte,
            });
            let line = as_memory(&fit);
            said.push_str(&format!(
                ": {:.0} bytes of its own and {:.3} bytes a byte, margin {:.2}{}",
                line.fixed_bytes,
                line.per_input_byte,
                line.margin(),
                if client.is_some() {
                    " (the slope of the measurement it came with)"
                } else {
                    ""
                }
            ));
            said.push_str(&self.size_from(stage, line));
        }
        tracing::info!(target: "sandbox", image = %self.name, "{said}");
        lock(&self.warm).said = Some(said);
        self.warming.store(false, Ordering::Relaxed);
    }

    /// The stage's instances funded from `line` over the run's room for them, sized at the
    /// piece and the account it gives ([`measured_sizing`]), the share of the stage's ceiling
    /// counted among them, and the room they do not need handed back; the note's words for it.
    pub(super) fn size_from(&self, stage: &Stage, line: moruna_cost::Memory) -> String {
        let asked = self.asked.load(Ordering::Relaxed);
        let mut room = lock(&self.room);
        let Some(room) = room.as_mut() else {
            return String::from("; the run said no room for the stage, which stays as it was");
        };
        room.line = Some(line.clone());
        let (piece, account, count) = match measured_sizing(room, &line, asked) {
            Ok(sized) => sized,
            Err(why) => {
                // Not one instance at the least piece: one, in the whole ceiling, as before.
                let share = stage.recount(1).unwrap_or_else(|_| stage.share());
                self.funded.store(1, Ordering::Relaxed);
                self.share.store(share, Ordering::Relaxed);
                return format!("; one instance, in the whole of the stage's ceiling ({why})");
            }
        };
        // Its other instances are carved at the account, and its batches cut at the piece, as
        // the node's are (item 6.3v); the first keeps its cgroup, on the share's rule.
        *lock(&self.measured) = Some((piece, account));
        self.funded.store(count, Ordering::Relaxed);
        let share = match stage.recount(count) {
            Ok(share) => share,
            Err(e) => {
                tracing::warn!(target: "sandbox", image = %self.name, "the instances' shares could not be written: {e}");
                stage.share()
            }
        };
        self.share.store(share, Ordering::Relaxed);
        let freed = room
            .bytes
            .saturating_sub(account.saturating_mul(count as u64));
        if let Some(f) = self.on_sized.get() {
            f(freed);
        }
        format!(
            "; its {count} instances funded at pieces of {piece} bytes, {account} bytes each, of \
             the {} bytes the run has for them, {freed} bytes of it handed back to the arena",
            room.bytes
        )
    }
}

/// The piece and the account a stage measured by `line` sizes its instances at, and how many of
/// the `asked` it runs (system design 3A; the node sizes its tasks so, item 6.3v): at the largest
/// of the run's pieces, from its largest morsel down to its least (`stage::ladder`), at which
/// every instance fits in the run's room for them by the line's account there, else the least
/// with as many as fit there (`instances::fund_measured`: the piece shrinks first, then the
/// count). The pieces go down to the largest the line was measured at where that is under the
/// least morsel (`instances::measured_pieces`, as the node decides it): a line of one size is
/// scaled up in proportion past it (`Memory::account`), which would cost an instance at a
/// piece the job has not given as far more than it measured. Refused, in words, when one
/// instance does not fit at the least.
pub(super) fn measured_sizing(
    room: &super::Room,
    line: &moruna_cost::Memory,
    asked: usize,
) -> std::result::Result<(u64, u64, usize), String> {
    let ladder = instances::measured_pieces(
        &stage::ladder(room.morsel, room.least),
        line.input_bytes_max,
    );
    let costs: Vec<u64> = ladder.iter().map(|&piece| need_at(line, piece)).collect();
    let (at, counts) = instances::fund_measured(room.bytes, &[(asked, costs.clone())])?;
    Ok((
        ladder[at],
        costs[at],
        counts.first().copied().unwrap_or(1).max(1),
    ))
}
