//! Report assembly (12 f.2). `RunMeta` is everything the report needs that the trace does
//! not hold; `RunReport::compute` is a pure function of the trace, the limits and the meta
//! (TR-I3), so this module decides nothing and only gathers.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use moruna_controller::{Bottleneck, ControllerSummary};
use moruna_kernel::{GilState, IoPaths, Limits, RunId, StageId};
use moruna_scheduler::RunOutcome;
use moruna_trace::{ExitReason, RunMeta, RunReport, TraceView};

/// Wall clock in nanoseconds since the epoch, which is what `RunMeta` records.
pub fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// `RunOutcome` to `ExitReason`, with the manifest the outcome carried (12 f.1).
pub fn exit_of(outcome: &RunOutcome) -> (ExitReason, Option<PathBuf>) {
    match outcome {
        RunOutcome::Completed { .. } => (ExitReason::Completed, None),
        RunOutcome::Terminated {
            diagnostic,
            manifest,
        } => (
            ExitReason::Terminated {
                diagnostic: diagnostic.to_string(),
            },
            manifest.clone(),
        ),
        RunOutcome::Cancelled { manifest } => (ExitReason::Cancelled, manifest.clone()),
    }
}

/// The bottleneck names the report prints, one per `Bottleneck` variant.
pub(crate) fn bottleneck_name(b: Bottleneck) -> String {
    match b {
        Bottleneck::IoRead => "IoRead",
        Bottleneck::Memory => "Memory",
        Bottleneck::Sink => "Sink",
        Bottleneck::Compute => "Compute",
        Bottleneck::CpuQuota => "CpuQuota",
        Bottleneck::Idle => "Idle",
        Bottleneck::StateGrowth => "StateGrowth",
    }
    .to_string()
}

/// What the facade knows and the trace does not, gathered for one run (12 f.2).
pub struct MetaInput {
    /// The run's identity.
    pub run_id: RunId,
    /// How the run ended.
    pub exit: ExitReason,
    /// Wall clock at the first step, nanoseconds.
    pub start_ns: u64,
    /// Wall clock after the last step, nanoseconds.
    pub end_ns: u64,
    /// The run continued from a manifest.
    pub resumed: bool,
    /// The outcome's manifest, or the engine's when `checkpoint_keep` kept it.
    pub manifest: Option<PathBuf>,
    /// Discovery notes, then the surface's, then the facade's.
    pub notes: Vec<String>,
    /// One entry per Python stage.
    pub gil: Vec<(StageId, GilState)>,
    /// The direct paths the reactor selected.
    pub io_paths: IoPaths,
    /// The controller's summary, when the controller ran.
    pub controller: Option<ControllerSummary>,
}

/// Build the `RunMeta` of 04 d.1 from what the run gathered (12 f.2).
pub fn meta(input: MetaInput) -> RunMeta {
    let (sizer, fallback_at, timeline, controller_notes) = match input.controller {
        Some(summary) => (
            summary.sizer,
            summary.fallback_at,
            summary
                .timeline
                .into_iter()
                .map(|(seconds, class)| (seconds, bottleneck_name(class)))
                .collect(),
            summary.notes,
        ),
        None => ("rule", None, Vec::new(), Vec::new()),
    };
    RunMeta {
        run_id: input.run_id,
        exit: input.exit,
        start_ns: input.start_ns,
        end_ns: input.end_ns,
        resumed: input.resumed,
        manifest: input.manifest,
        notes: input.notes,
        gil: input.gil,
        io_paths: input.io_paths,
        sizer,
        sizer_fallback_at: fallback_at,
        bottleneck_timeline: timeline,
        controller_notes,
    }
}

/// `RunReport::compute(&trace.finish()?, &limits, &meta)` (12 f.2).
pub fn compute(view: &TraceView, limits: &Limits, meta: &RunMeta) -> RunReport {
    RunReport::compute(view, limits, meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_controller::ControllerSummary;
    use moruna_kernel::{KnobSnapshot, MorunaError};

    /// Every outcome maps to its exit reason, and a terminated one carries its manifest.
    #[test]
    fn every_outcome_has_an_exit() {
        let (exit, manifest) = exit_of(&RunOutcome::Completed {
            sink: moruna_kernel::SinkSummary::default(),
        });
        assert_eq!(exit, ExitReason::Completed);
        assert!(manifest.is_none());

        let (exit, manifest) = exit_of(&RunOutcome::Terminated {
            diagnostic: MorunaError::Plan("no splits".into()),
            manifest: Some(PathBuf::from("/tmp/m.json")),
        });
        assert!(matches!(exit, ExitReason::Terminated { .. }));
        assert_eq!(manifest, Some(PathBuf::from("/tmp/m.json")));

        let (exit, manifest) = exit_of(&RunOutcome::Cancelled {
            manifest: Some(PathBuf::from("/tmp/c.json")),
        });
        assert_eq!(exit, ExitReason::Cancelled);
        assert_eq!(manifest, Some(PathBuf::from("/tmp/c.json")));
    }

    /// Every bottleneck has a name, and the timeline carries them through.
    #[test]
    fn the_timeline_names_every_bottleneck() {
        let all = [
            Bottleneck::IoRead,
            Bottleneck::Memory,
            Bottleneck::Sink,
            Bottleneck::Compute,
            Bottleneck::CpuQuota,
            Bottleneck::Idle,
            Bottleneck::StateGrowth,
        ];
        let summary = ControllerSummary {
            timeline: all.iter().map(|b| (1.0, *b)).collect(),
            sizer: "rule",
            fallback_at: Some(7),
            freezes: 0,
            breaches: 0,
            final_knobs: KnobSnapshot::default(),
            notes: vec!["a controller note".to_string()],
        };
        let meta = meta(MetaInput {
            run_id: moruna_kernel::RunId([1; 16]),
            exit: ExitReason::Completed,
            start_ns: 1,
            end_ns: 2,
            resumed: false,
            manifest: None,
            notes: Vec::new(),
            gil: Vec::new(),
            io_paths: moruna_kernel::IoPaths::default(),
            controller: Some(summary),
        });
        assert_eq!(meta.bottleneck_timeline.len(), all.len());
        assert_eq!(meta.bottleneck_timeline[0].1, "IoRead");
        assert_eq!(meta.bottleneck_timeline[6].1, "StateGrowth");
        assert_eq!(meta.sizer_fallback_at, Some(7));
        assert_eq!(meta.controller_notes.len(), 1);
    }

    /// Without a controller the meta still says which sizer the run would have used.
    #[test]
    fn a_run_without_a_controller_still_has_meta() {
        let meta = meta(MetaInput {
            run_id: moruna_kernel::RunId([2; 16]),
            exit: ExitReason::Cancelled,
            start_ns: 0,
            end_ns: 0,
            resumed: true,
            manifest: None,
            notes: vec!["a note".to_string()],
            gil: Vec::new(),
            io_paths: moruna_kernel::IoPaths::default(),
            controller: None,
        });
        assert_eq!(meta.sizer, "rule");
        assert!(meta.bottleneck_timeline.is_empty());
        assert!(meta.resumed);
        assert!(now_ns() > 0);
    }
}
