//! Private KD3 proof reducer for the typed sink acknowledgment, not kernel report rows.
use moruna_event::{Element, Event, EventMsg, Phase, TaskOutcome};

#[derive(Default)]
pub struct SinkAck {
    run: Option<String>,
    identity: Option<(String, u32, Option<String>)>,
    seq: u64,
    started: bool,
    completed: bool,
    invalid: bool,
}
impl SinkAck {
    pub fn observe(&mut self, event: &EventMsg, approved_run: Option<&str>, rows: u64) {
        let relevant = match &event.body {
            Event::TaskRegistered { task, .. }
            | Event::TaskStarted { task, .. }
            | Event::TaskCompleted { task, .. }
            | Event::TaskSkipped { task, .. } => task == "sink",
            Event::TaskFailed { task, .. } => task.as_deref() == Some("sink"),
            _ => false,
        };
        if !relevant {
            return;
        }
        // Native events can legitimately omit run. Their existing SDK/control
        // stream is bound to the approved launch and subsequently checked report.
        for run in [approved_run, event.run.as_deref()].into_iter().flatten() {
            if self.run.as_deref().is_some_and(|known| known != run) {
                self.invalid = true;
            }
            self.run = Some(run.to_owned());
        }
        let identity = (
            event.start.clone(),
            event.attempt,
            event.body.step().map(str::to_owned),
        );
        if event.seq == 0 || event.seq <= self.seq {
            self.invalid = true;
        }
        self.seq = event.seq;
        match &event.body {
            Event::TaskRegistered {
                element: Element::Sink,
                stage: 3,
                ..
            } => {
                if identity.0.is_empty()
                    || identity.2.as_deref().is_none_or(str::is_empty)
                    || self.identity.replace(identity).is_some()
                {
                    self.invalid = true;
                }
            }
            Event::TaskStarted {
                phase: Phase::Writing,
                ..
            } => {
                if self.identity.as_ref() != Some(&identity) || self.started || self.completed {
                    self.invalid = true;
                }
                self.started = true;
            }
            Event::TaskCompleted {
                outcome,
                counters,
                cause,
                ..
            } => {
                if self.identity.as_ref() != Some(&identity) || !self.started || self.completed {
                    self.invalid = true;
                }
                self.completed = true;
                if *outcome != TaskOutcome::Succeeded
                    || cause.is_some()
                    || !counters.as_ref().is_some_and(|c| {
                        c.rows_in == rows
                            && c.rows_out == rows
                            && c.morsels > 0
                            && c.errors == 0
                            && c.skipped == 0
                    })
                {
                    self.invalid = true;
                }
            }
            _ => self.invalid = true,
        }
    }
    pub fn verify(&self, report_run: Option<&str>) -> Result<(), &'static str> {
        if self.invalid
            || !self.completed
            || report_run.is_none_or(str::is_empty)
            || self
                .run
                .as_deref()
                .is_none_or(|run| run.is_empty() || Some(run) != report_run)
        {
            return Err("sink did not acknowledge all rows for this run/attempt");
        }
        Ok(())
    }
}
