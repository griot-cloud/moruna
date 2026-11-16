//! `TensorSink` (08 f.3, e.4).
//!
//! Tensor bytes go from the payload's own arena memory to the file through `BufferView`, so
//! nothing is copied. The safetensors header is metadata, written from a small arena buffer of
//! its own and padded with spaces so the data section starts on a page boundary.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use moruna_kernel::{
    Allocator, BoxFuture, Buffer, BufferView, Completion, DType, ManagedTensor, MorunaError,
    Payload, PayloadKind, PayloadSpec, Reactor, Result, Seq, Sink, SinkSummary, SourceSchema,
    TierPref,
};

use crate::checkpoint::{CommittedFile, Ledger};
use crate::commit::{LocalDir, TMP_SUFFIX};
use crate::stats::SinkStats;
use crate::{Phase, host_tier, require_host};

/// The kind a per-morsel sink writes into its checkpoint (e.5).
const KIND: &str = "safetensors_per_morsel";

/// Which tensor format a `TensorSink` writes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TensorFormat {
    /// The safetensors format. The header is padded with spaces, as the spec allows, so the
    /// data section starts at the page size and a reader can map or read it aligned.
    #[default]
    SafeTensors,
}

/// Where a tensor sink writes and in which shape.
pub struct TensorSinkConfig {
    /// Directory the files are created in.
    pub path: PathBuf,
    /// Output format.
    pub format: TensorFormat,
    /// One file per morsel, rather than one file for the run. Only this shape can resume.
    pub one_file_per_morsel: bool,
    /// The tensor's name: the file stem, and the safetensors entry name.
    pub name: String,
}

impl Default for TensorSinkConfig {
    fn default() -> TensorSinkConfig {
        TensorSinkConfig {
            path: PathBuf::new(),
            format: TensorFormat::SafeTensors,
            one_file_per_morsel: false,
            name: "tensor".to_string(),
        }
    }
}

/// The one file a run-mode sink appends to.
struct RunFile {
    name: String,
    tmp: PathBuf,
    /// Where the next tensor's bytes go.
    cursor: u64,
    /// Where the first tensor's bytes went.
    data_offset: u64,
    rows: u64,
    inflight: u64,
}

struct Inner {
    phase: Phase,
    dtype: Option<DType>,
    /// The schema's shape, with `-1` in position 0 for a variable batch dimension.
    shape: Option<Vec<i64>>,
    run_file: Option<RunFile>,
    ledger: Ledger,
    stats: SinkStats,
}

/// A sink that writes tensors as safetensors files (e.4).
pub struct TensorSink {
    cfg: TensorSinkConfig,
    dir: LocalDir,
    reactor: Arc<dyn Reactor>,
    alloc: Arc<dyn Allocator>,
    inner: Mutex<Inner>,
}

/// What one write left for the caller to await.
struct Submission {
    completions: Vec<Completion<()>>,
    tensor: Arc<ManagedTensor>,
    header: Option<Arc<Buffer>>,
    /// The per-morsel file this write produced, once its bytes are down.
    entry: Option<CommittedFile>,
}

impl TensorSink {
    /// Build a sink over `cfg`.
    pub fn new(
        cfg: TensorSinkConfig,
        reactor: Arc<dyn Reactor>,
        alloc: Arc<dyn Allocator>,
    ) -> Result<TensorSink> {
        // The name becomes a file name in `path`; a separator or `..` would put it elsewhere.
        if cfg.name.is_empty()
            || cfg.name == "."
            || cfg.name == ".."
            || cfg.name.contains(['/', '\\', '\0'])
        {
            return Err(MorunaError::Sink(format!(
                "tensor name {:?} is not a plain file name",
                cfg.name
            )));
        }
        let dir = LocalDir::new(&cfg.path)?;
        Ok(TensorSink {
            cfg,
            dir,
            reactor,
            alloc,
            inner: Mutex::new(Inner {
                phase: Phase::Created,
                dtype: None,
                shape: None,
                run_file: None,
                ledger: Ledger::new(KIND),
                stats: SinkStats::default(),
            }),
        })
    }

    /// The counters of section j.
    pub fn stats(&self) -> SinkStats {
        self.lock().stats.clone()
    }

    /// True for the one shape of this sink that tracks commits: one file per morsel. The run
    /// file leaves the resume methods at their defaults, which is how the scheduler learns at
    /// startup that the run is not resumable (d.1, SC f.11).
    pub fn is_resumable(&self) -> bool {
        self.cfg.one_file_per_morsel
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Where the data section starts: the host's page size, never below 4096. The header and
    /// its padding fill everything before it.
    fn data_offset(&self) -> u64 {
        (self.alloc.page_bytes() as u64).max(4096)
    }

    fn file_name(&self, seq: Option<Seq>) -> String {
        match (seq, self.cfg.format) {
            (Some(seq), TensorFormat::SafeTensors) => {
                format!("{}-{seq:012}.safetensors", self.cfg.name)
            }
            (None, TensorFormat::SafeTensors) => format!("{}.safetensors", self.cfg.name),
        }
    }

    /// The header bytes of one file: the length, the JSON and the spaces up to `data_offset`.
    fn header(&self, dtype: DType, shape: &[i64], payload_len: u64) -> Result<Buffer> {
        let data_offset = self.data_offset() as usize;
        let json = safetensors_header(&self.cfg.name, dtype, shape, payload_len, data_offset - 8)?;
        let mut buf = self.alloc.alloc(data_offset, host_tier(&*self.alloc))?;
        buf[0..8].copy_from_slice(&((data_offset - 8) as u64).to_le_bytes());
        buf[8..8 + json.len()].copy_from_slice(json.as_bytes());
        for byte in buf[8 + json.len()..data_offset].iter_mut() {
            *byte = b' ';
        }
        Ok(buf)
    }

    /// The run file, opened at the offset its format reserves for a header written at `finish`.
    fn open_run_file(&self, inner: &mut Inner) -> Result<()> {
        if inner.shape.is_none() {
            return Err(MorunaError::Sink("the sink has no schema".into()));
        }
        let data_offset = self.data_offset();
        let name = self.file_name(None);
        inner.run_file = Some(RunFile {
            tmp: self.dir.tmp_of(&name),
            name,
            cursor: data_offset,
            data_offset,
            rows: 0,
            inflight: 0,
        });
        Ok(())
    }

    fn submit(&self, inner: &mut Inner, seq: Seq, payload: Payload) -> Result<Submission> {
        inner.phase.require_open()?;
        require_host(&payload)?;
        let rows = payload.rows();
        let Payload::Tensor(tensor, _) = payload else {
            return Err(MorunaError::Sink(
                "the tensor sink accepts a tensor payload".into(),
            ));
        };
        let tensor = Arc::new(tensor);
        self.check_shape(inner, &tensor)?;
        let out = if self.is_resumable() {
            self.submit_per_morsel(inner, seq, &tensor, rows)
        } else {
            self.submit_run(inner, &tensor, rows)
        };
        match out {
            Ok((completions, header, entry)) => {
                inner.stats.writes += 1;
                Ok(Submission {
                    completions,
                    tensor,
                    header,
                    entry,
                })
            }
            Err(e) => {
                inner.phase = Phase::Failed(e.to_string());
                Err(e)
            }
        }
    }

    /// One safetensors file for this morsel: the header at 0, the tensor at the page-aligned
    /// data offset, straight from the arena (f.3). The sequence number is the file name, so the
    /// file needs no metadata of its own (SI-I8).
    #[allow(clippy::type_complexity)]
    fn submit_per_morsel(
        &self,
        inner: &mut Inner,
        seq: Seq,
        tensor: &Arc<ManagedTensor>,
        rows: u64,
    ) -> Result<(
        Vec<Completion<()>>,
        Option<Arc<Buffer>>,
        Option<CommittedFile>,
    )> {
        let body = BufferView::of_tensor(tensor)?;
        let bytes = body.len() as u64;
        let data_offset = self.data_offset();
        let buf = Arc::new(self.header(tensor.dtype(), tensor.shape(), bytes)?);
        let name = self.file_name(Some(seq));
        let tmp = self.dir.tmp_of(&name);
        let view = buf.view().slice(0, data_offset as usize);
        let mut completions = vec![self.reactor.write_file(&tmp, 0, view)];
        completions.push(self.reactor.write_file(&tmp, data_offset, body));
        let _ = inner.ledger.take_index();
        Ok((
            completions,
            Some(buf),
            Some(CommittedFile {
                name,
                seq_min: seq,
                seq_max: seq,
                rows,
                bytes: data_offset + bytes,
            }),
        ))
    }

    /// Append the tensor's bytes to the run file at its running offset; the header follows at
    /// `finish`, when the total is known, and until then the file is `.tmp` (f.3).
    #[allow(clippy::type_complexity)]
    fn submit_run(
        &self,
        inner: &mut Inner,
        tensor: &Arc<ManagedTensor>,
        rows: u64,
    ) -> Result<(
        Vec<Completion<()>>,
        Option<Arc<Buffer>>,
        Option<CommittedFile>,
    )> {
        let body = BufferView::of_tensor(tensor)?;
        let bytes = body.len() as u64;
        let Some(file) = inner.run_file.as_mut() else {
            return Err(MorunaError::Sink("the sink has no open file".into()));
        };
        let at = file.cursor;
        file.cursor += bytes;
        file.rows += rows;
        file.inflight += 1;
        let tmp = file.tmp.clone();
        Ok((vec![self.reactor.write_file(&tmp, at, body)], None, None))
    }

    fn check_shape(&self, inner: &Inner, tensor: &ManagedTensor) -> Result<()> {
        let (Some(dtype), Some(shape)) = (inner.dtype, inner.shape.as_ref()) else {
            return Err(MorunaError::Sink("the sink has no schema".into()));
        };
        if tensor.dtype() != dtype {
            return Err(MorunaError::Sink(format!(
                "schema drift: the tensor is {:?}, the sink opened with {dtype:?}",
                tensor.dtype()
            )));
        }
        let found = tensor.shape();
        if found.len() != shape.len() {
            return Err(MorunaError::Sink(format!(
                "schema drift: the tensor has rank {}, the sink opened with {}",
                found.len(),
                shape.len()
            )));
        }
        for (axis, (declared, actual)) in shape.iter().zip(found.iter()).enumerate() {
            if *declared >= 0 && declared != actual {
                return Err(MorunaError::Sink(format!(
                    "schema drift: axis {axis} is {actual}, the sink opened with {declared}"
                )));
            }
        }
        Ok(())
    }

    /// Make a per-morsel file visible and record it (f.5, f.7).
    fn commit(&self, entry: CommittedFile, seq: Seq) -> Result<()> {
        self.dir.commit(&entry.name)?;
        let mut inner = self.lock();
        tracing::info!(target: "sink.commit", file = %entry.name, "committed a tensor file");
        inner.stats.files_committed += 1;
        inner.ledger.commit(entry, &[seq]);
        Ok(())
    }

    fn fail(&self, error: &MorunaError) {
        let mut inner = self.lock();
        if !matches!(inner.phase, Phase::Failed(_)) {
            inner.phase = Phase::Failed(error.to_string());
        }
    }

    /// The run file's header, written last, and the rename that commits it (f.3).
    fn close_run_file(
        &self,
        inner: &mut Inner,
    ) -> Result<(CommittedFile, Completion<()>, Arc<Buffer>)> {
        let Some(file) = inner.run_file.take() else {
            return Err(MorunaError::Sink("the sink has no open file".into()));
        };
        let (Some(dtype), Some(shape)) = (inner.dtype, inner.shape.clone()) else {
            return Err(MorunaError::Sink("the sink has no schema".into()));
        };
        let mut total = shape.clone();
        if let Some(first) = total.first_mut()
            && *first < 0
        {
            *first = file.rows as i64;
        }
        let payload_len = file.cursor - file.data_offset;
        let bytes = self.header(dtype, &total, payload_len)?;
        let bytes = Arc::new(bytes);
        let view = bytes.view().slice(0, file.data_offset as usize);
        let completion = self.reactor.write_file(&file.tmp, 0, view);
        Ok((
            CommittedFile {
                name: file.name,
                seq_min: 0,
                seq_max: 0,
                rows: file.rows,
                bytes: file.cursor,
            },
            completion,
            bytes,
        ))
    }
}

/// The safetensors header of one tensor entry, at most `room` bytes, padded by the caller (f.3).
fn safetensors_header(
    name: &str,
    dtype: DType,
    shape: &[i64],
    payload_len: u64,
    room: usize,
) -> Result<String> {
    let dims = shape
        .iter()
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join(",");
    // The name is the user's: serialised, so a quote or a backslash in it stays inside the key.
    let key = serde_json::to_string(name)
        .map_err(|e| MorunaError::Sink(format!("the tensor name does not serialise: {e}")))?;
    let json = format!(
        "{{{key}:{{\"dtype\":\"{}\",\"shape\":[{dims}],\"data_offsets\":[0,{payload_len}]}}}}",
        safetensors_dtype(dtype)
    );
    if json.len() > room {
        return Err(MorunaError::Sink(format!(
            "the safetensors header is {} bytes, the file reserved {room}",
            json.len()
        )));
    }
    Ok(json)
}

/// The safetensors spelling of a dtype.
fn safetensors_dtype(dtype: DType) -> &'static str {
    match dtype {
        DType::I8 => "I8",
        DType::I16 => "I16",
        DType::I32 => "I32",
        DType::I64 => "I64",
        DType::U8 => "U8",
        DType::U16 => "U16",
        DType::U32 => "U32",
        DType::U64 => "U64",
        DType::F16 => "F16",
        DType::BF16 => "BF16",
        DType::F32 => "F32",
        DType::F64 => "F64",
        DType::Bool => "BOOL",
    }
}

/// The sequence number a per-morsel file name carries (e.4).
fn seq_of(name: &str, stem: &str) -> Option<Seq> {
    let rest = name.strip_prefix(stem)?.strip_prefix('-')?;
    rest.strip_suffix(".safetensors")?.parse().ok()
}

impl Sink for TensorSink {
    fn open(&mut self, schema: &SourceSchema) -> Result<()> {
        let SourceSchema::Tensor { dtype, shape } = schema else {
            return Err(MorunaError::Sink(
                "the tensor sink accepts a tensor schema".into(),
            ));
        };
        let mut inner = self.lock();
        inner.phase.require_created()?;
        inner.dtype = Some(*dtype);
        inner.shape = Some(shape.clone());
        if !self.is_resumable() {
            self.open_run_file(&mut inner)?;
        }
        inner.phase = Phase::Open;
        Ok(())
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Tensor,
            tier: TierPref::Host,
        }
    }

    fn write(&self, seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let submission = {
                let mut inner = self.lock();
                self.submit(&mut inner, seq, payload)?
            };
            let Submission {
                completions,
                tensor,
                header,
                entry,
            } = submission;
            let mut outcome = Ok(());
            for completion in completions {
                if let Err(e) = completion.await
                    && outcome.is_ok()
                {
                    outcome = Err(e);
                }
            }
            // The views over the tensor and the header have been read; both go back here.
            drop(header);
            drop(tensor);
            if !self.is_resumable() {
                let mut inner = self.lock();
                if let Some(file) = inner.run_file.as_mut() {
                    file.inflight -= 1;
                }
            }
            if let Err(e) = outcome {
                self.fail(&e);
                return Err(e);
            }
            match entry {
                Some(entry) => self.commit(entry, seq),
                None => Ok(()),
            }
        })
    }

    fn finish(&mut self) -> Result<SinkSummary> {
        let closing = {
            let mut inner = self.lock();
            match &inner.phase {
                Phase::Created => return Err(MorunaError::Sink("the sink is not open".into())),
                Phase::Finished => return Err(MorunaError::Sink("finish ran twice".into())),
                Phase::Failed(msg) => {
                    let msg = format!(
                        "{msg}; committed files: {}",
                        inner.ledger.names().join(", ")
                    );
                    inner.run_file = None;
                    inner.phase = Phase::Finished;
                    let _ = self.dir.remove_tmps();
                    return Err(MorunaError::Sink(msg));
                }
                Phase::Open => {}
            }
            if self.is_resumable() {
                None
            } else {
                Some(self.close_run_file(&mut inner)?)
            }
        };
        if let Some((entry, completion, header)) = closing {
            let outcome = completion.wait();
            drop(header);
            match outcome {
                Ok(()) => {
                    self.dir.commit(&entry.name)?;
                    let mut inner = self.lock();
                    inner.stats.files_committed += 1;
                    inner.ledger.commit(entry, &[]);
                }
                Err(e) => {
                    self.fail(&e);
                    return Err(e);
                }
            }
        }
        let mut inner = self.lock();
        inner.phase = Phase::Finished;
        let (rows, bytes) = inner.ledger.totals();
        Ok(SinkSummary {
            rows,
            bytes,
            files: inner.ledger.names(),
            commit: None,
        })
    }

    fn committed_seq(&self) -> Option<Seq> {
        if !self.is_resumable() {
            return None;
        }
        self.lock().ledger.committed_seq()
    }

    fn skip(&self, seq: Seq) {
        if self.is_resumable() {
            self.lock().ledger.skip(seq);
        }
    }

    fn checkpoint(&self) -> Result<Option<Vec<u8>>> {
        if !self.is_resumable() {
            return Ok(None);
        }
        Ok(Some(self.lock().ledger.to_bytes()?))
    }

    fn resume(
        &mut self,
        schema: &SourceSchema,
        state: &[u8],
        committed_seq: Option<Seq>,
    ) -> Result<()> {
        if !self.is_resumable() {
            return Err(MorunaError::Resume("sink does not support resume".into()));
        }
        let SourceSchema::Tensor { dtype, shape } = schema else {
            return Err(MorunaError::Sink(
                "the tensor sink accepts a tensor schema".into(),
            ));
        };
        let mut inner = self.lock();
        inner.phase.require_created()?;
        let doc = Ledger::parse(KIND, state)?;
        let (ledger, above) = Ledger::restore(KIND, doc, committed_seq)?;
        let keep: BTreeSet<String> = ledger.committed().iter().map(|f| f.name.clone()).collect();
        let mut removed = 0u64;
        for name in self.dir.list()? {
            if keep.contains(&name) {
                continue;
            }
            // The sequence number is in the file name, so a file above the watermark is
            // recognised without reading a byte of it (e.4, f.8).
            let above_watermark = match seq_of(&name, &self.cfg.name) {
                Some(seq) => committed_seq.is_none_or(|w| seq > w),
                None => name.ends_with(TMP_SUFFIX),
            };
            if above_watermark {
                self.dir.remove(&name)?;
                removed += 1;
            }
        }
        for file in &above {
            self.dir.remove(&file.name)?;
        }
        inner.ledger = ledger;
        inner.stats.resumed_files_removed = removed;
        inner.dtype = Some(*dtype);
        inner.shape = Some(shape.clone());
        inner.phase = Phase::Open;
        Ok(())
    }
}
