//! A stage's calls to named ports (MH 4.10): what a kernel hands bytes to and waits for an
//! answer from, outside its own memory.
//!
//! A port is a name the job document lists for one stage, and where the name goes: to the host,
//! over the host protocol (MH 4.3 `port_call`), or to a handler the program embedding Moruna
//! registered ([`GuestPort`]). Moruna reads none of the bytes. It refuses a name the stage does
//! not list, bounds every call by its deadline, gives the call up at once when the run is
//! cancelled, and says while it waits that the stage is waiting and on which port
//! ([`StageWaits`]): the controller does not take a waiting stage for an idle one or a busy one,
//! and the host hears it as the task's `task_waiting`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::error::MorunaError;
use crate::ids::StageId;
use crate::knobs::CancelToken;
use crate::stages::{StageEvent, StageHook};

/// The most bytes a call's request or its answer may hold.
pub const PORT_BYTES_MAX: usize = 64 << 20;
/// A port's deadline when its entry names none: half an hour.
pub const PORT_DEADLINE_MS: u64 = 30 * 60 * 1000;
/// The longest deadline an entry may name: a day.
pub const PORT_DEADLINE_MAX_MS: u64 = 24 * 60 * 60 * 1000;
/// How often a waiting call looks at the run's cancel and its deadline.
const WAKE: Duration = Duration::from_millis(5);
/// The sentinel the stage of a call has before a build binds it.
const NO_STAGE: StageId = StageId::MAX;

/// How a port answered one call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// The port's answer.
    Ok(Vec<u8>),
    /// The port's own refusal, in its words: handed to the kernel, which decides what it means.
    Error(String),
    /// The call never reached an answer: the host went away, a handler panicked. The stage fails
    /// with this cause.
    Lost(String),
}

/// Why a call stopped being waited for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Abandon {
    /// Its deadline passed.
    Deadline,
    /// The run was cancelled.
    Cancelled,
}

impl Abandon {
    /// `deadline` or `cancelled`, as the host protocol spells it.
    pub fn name(self) -> &'static str {
        match self {
            Abandon::Deadline => "deadline",
            Abandon::Cancelled => "cancelled",
        }
    }
}

/// One call, as a port is handed it.
#[derive(Clone, Debug)]
pub struct PortCall {
    /// The call's id, unique in this process.
    pub id: u64,
    /// The stage that calls.
    pub stage: StageId,
    /// The name the stage's entry lists the port under.
    pub name: String,
    /// The request.
    pub request: Vec<u8>,
    /// How long the call may take, from now.
    pub deadline: Duration,
}

/// Where a port's calls go: the host protocol, or a guest handler.
pub trait Port: Send + Sync {
    /// Start `call`. Its answer arrives on the receiver, once; `Err` is a call that could not
    /// start, and the stage fails with it.
    fn begin(&self, call: PortCall) -> Result<Receiver<Answer>, String>;
    /// `id` is no longer waited for; an answer that still comes is dropped.
    fn abandon(&self, id: u64, why: Abandon);
}

/// What a guest handler is told about the call it answers.
#[derive(Clone, Debug)]
pub struct GuestCall {
    /// The stage that calls.
    pub stage: StageId,
    /// The name the stage's entry lists the port under.
    pub name: String,
    /// When the stage stops waiting.
    pub deadline: Instant,
    /// Cancelled when the stage stops waiting, for its deadline or the run's cancel; a handler
    /// that does long work stops at it.
    pub cancel: CancelToken,
}

/// A handler the program that embeds Moruna registers for a guest port (`{"guest": "<name>"}`):
/// it answers inside the machine, on its own thread, with what the embedding can reach and the
/// kernel cannot.
pub trait GuestPort: Send + Sync {
    /// The answer to one request.
    fn call(&self, request: Vec<u8>, call: &GuestCall) -> Answer;
}

/// A guest handler as a [`Port`]: each call on a thread of its own, so the stage's deadline and
/// the run's cancel hold whatever the handler does.
pub struct GuestRoute {
    handler: Arc<dyn GuestPort>,
    running: Mutex<BTreeMap<u64, CancelToken>>,
}

impl GuestRoute {
    /// The port that `handler` answers.
    pub fn new(handler: Arc<dyn GuestPort>) -> Arc<GuestRoute> {
        Arc::new(GuestRoute {
            handler,
            running: Mutex::new(BTreeMap::new()),
        })
    }
}

impl Port for GuestRoute {
    fn begin(&self, call: PortCall) -> Result<Receiver<Answer>, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = CancelToken::new();
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(call.id, cancel.clone());
        let handler = Arc::clone(&self.handler);
        let told = GuestCall {
            stage: call.stage,
            name: call.name.clone(),
            deadline: Instant::now() + call.deadline,
            cancel,
        };
        let request = call.request;
        std::thread::Builder::new()
            .name(format!("moruna-port-{}", call.name))
            .spawn(move || {
                let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handler.call(request, &told)
                }))
                .unwrap_or_else(|panic| {
                    let what = panic
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "a panic".to_string());
                    Answer::Lost(format!("the guest handler panicked: {what}"))
                });
                let _ = tx.send(answer);
            })
            .map_err(|e| format!("no thread for the guest handler: {e}"))?;
        Ok(rx)
    }

    fn abandon(&self, id: u64, _why: Abandon) {
        if let Some(cancel) = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
        {
            cancel.cancel();
        }
    }
}

/// One name a stage may call, and where it goes.
#[derive(Clone)]
pub struct Route {
    /// `host` or `guest`.
    pub place: &'static str,
    /// Where calls go.
    pub port: Arc<dyn Port>,
    /// How long one call may take.
    pub deadline: Duration,
}

/// What a run gives the ports of its stages: its cancel and its record of who waits.
#[derive(Clone)]
pub struct RunPorts {
    /// The run's cancel.
    pub cancel: CancelToken,
    /// The run's waits.
    pub waits: Arc<StageWaits>,
}

/// Why a call failed the stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallFailure {
    /// The stage called a name its entry does not list.
    Unlisted {
        /// The name called.
        name: String,
        /// The names it lists.
        listed: Vec<String>,
    },
    /// The request is larger than [`PORT_BYTES_MAX`].
    TooLarge {
        /// The port.
        name: String,
        /// Its size.
        bytes: usize,
    },
    /// No answer before the deadline.
    Deadline {
        /// The port.
        name: String,
        /// The deadline, in milliseconds.
        ms: u64,
    },
    /// The run was cancelled while the call waited.
    Cancelled,
    /// The call reached no answer.
    Lost {
        /// The port.
        name: String,
        /// Why.
        reason: String,
    },
    /// The kernel runs with no run bound: `moruna check` or `moruna load`.
    Unbound {
        /// The port.
        name: String,
    },
}

impl CallFailure {
    /// The run's error for it: `Cancelled` for a cancel, a kernel error naming the stage, the
    /// port and the cause for everything else.
    pub fn into_error(self, stage: StageId) -> MorunaError {
        let msg = match self {
            CallFailure::Cancelled => return MorunaError::Cancelled,
            CallFailure::Unlisted { name, listed } => format!(
                "port `{name}` is not listed for this stage (it lists {})",
                if listed.is_empty() {
                    "none".to_string()
                } else {
                    listed.join(", ")
                }
            ),
            CallFailure::TooLarge { name, bytes } => format!(
                "port `{name}`: a request of {bytes} bytes is larger than the {PORT_BYTES_MAX} a \
                 call may carry"
            ),
            CallFailure::Deadline { name, ms } => {
                format!("port `{name}`: no answer within its deadline of {ms} ms")
            }
            CallFailure::Lost { name, reason } => format!("port `{name}`: {reason}"),
            CallFailure::Unbound { name } => format!(
                "port `{name}`: the kernel called a port outside a run; a check or a load gives \
                 a kernel no ports"
            ),
        };
        MorunaError::Kernel {
            stage,
            seq: u64::MAX,
            msg,
            features: None,
        }
    }
}

/// The ports one stage may call (its entry's `ports`), bound to the run once it starts.
pub struct StagePorts {
    stage: StageId,
    routes: BTreeMap<String, Route>,
    run: OnceLock<RunPorts>,
}

static NEXT_CALL: AtomicU64 = AtomicU64::new(1);

impl StagePorts {
    /// The ports `routes` lists for `stage`.
    pub fn new(stage: StageId, routes: BTreeMap<String, Route>) -> Arc<StagePorts> {
        Arc::new(StagePorts {
            stage,
            routes,
            run: OnceLock::new(),
        })
    }

    /// No ports at all: every call is refused by name.
    pub fn none() -> Arc<StagePorts> {
        StagePorts::new(NO_STAGE, BTreeMap::new())
    }

    /// The stage.
    pub fn stage(&self) -> StageId {
        self.stage
    }

    /// The names the entry lists.
    pub fn names(&self) -> Vec<String> {
        self.routes.keys().cloned().collect()
    }

    /// Whether the entry lists any port.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// The run starts: its cancel and its waits. A second binding is ignored.
    pub fn bind_run(&self, run: RunPorts) {
        let _ = self.run.set(run);
    }

    /// Call `name` with `request` and wait for its answer, its deadline or the run's cancel.
    pub fn call(&self, name: &str, request: Vec<u8>) -> Result<Answer, CallFailure> {
        let Some(route) = self.routes.get(name) else {
            return Err(CallFailure::Unlisted {
                name: name.to_string(),
                listed: self.names(),
            });
        };
        if request.len() > PORT_BYTES_MAX {
            return Err(CallFailure::TooLarge {
                name: name.to_string(),
                bytes: request.len(),
            });
        }
        let Some(run) = self.run.get() else {
            return Err(CallFailure::Unbound {
                name: name.to_string(),
            });
        };
        if run.cancel.is_cancelled() {
            return Err(CallFailure::Cancelled);
        }
        let id = NEXT_CALL.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let deadline = started + route.deadline;
        let _waiting = run.waits.begin(self.stage, name);
        let answers = route
            .port
            .begin(PortCall {
                id,
                stage: self.stage,
                name: name.to_string(),
                request,
                deadline: route.deadline,
            })
            .map_err(|reason| CallFailure::Lost {
                name: name.to_string(),
                reason,
            })?;
        loop {
            match answers.recv_timeout(WAKE) {
                Ok(Answer::Lost(reason)) => {
                    return Err(CallFailure::Lost {
                        name: name.to_string(),
                        reason,
                    });
                }
                Ok(Answer::Ok(bytes)) if bytes.len() > PORT_BYTES_MAX => {
                    return Err(CallFailure::Lost {
                        name: name.to_string(),
                        reason: format!(
                            "an answer of {} bytes is larger than the {PORT_BYTES_MAX} a call \
                             may carry",
                            bytes.len()
                        ),
                    });
                }
                Ok(answer) => return Ok(answer),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(CallFailure::Lost {
                        name: name.to_string(),
                        reason: "the port dropped the call without answering".to_string(),
                    });
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            if run.cancel.is_cancelled() {
                route.port.abandon(id, Abandon::Cancelled);
                return Err(CallFailure::Cancelled);
            }
            if Instant::now() >= deadline {
                route.port.abandon(id, Abandon::Deadline);
                return Err(CallFailure::Deadline {
                    name: name.to_string(),
                    ms: route.deadline.as_millis() as u64,
                });
            }
        }
    }
}

#[derive(Default)]
struct Waiting {
    calls: u32,
    port: String,
}

/// Which stages wait on a port now (MH 4.10). Told to the scheduler's counters, which the
/// controller classifies from, and to the facade's stage hook, which the host hears.
pub struct StageWaits {
    stages: Mutex<BTreeMap<StageId, Waiting>>,
    hook: Option<StageHook>,
}

impl StageWaits {
    /// No stage waits; `hook` hears each stage start and stop waiting.
    pub fn new(hook: Option<StageHook>) -> Arc<StageWaits> {
        Arc::new(StageWaits {
            stages: Mutex::new(BTreeMap::new()),
            hook,
        })
    }

    /// `stage` waits on `port` until the guard is dropped.
    pub fn begin(self: &Arc<Self>, stage: StageId, port: &str) -> WaitGuard {
        let mut stages = self.stages.lock().unwrap_or_else(|e| e.into_inner());
        let waiting = stages.entry(stage).or_default();
        waiting.calls += 1;
        if waiting.calls == 1 {
            waiting.port = port.to_string();
            if let Some(hook) = &self.hook {
                hook(&StageEvent::PortWaiting {
                    stage,
                    port: port.to_string(),
                });
            }
        }
        WaitGuard {
            waits: Arc::clone(self),
            stage,
        }
    }

    fn end(&self, stage: StageId) {
        let mut stages = self.stages.lock().unwrap_or_else(|e| e.into_inner());
        let Some(waiting) = stages.get_mut(&stage) else {
            return;
        };
        waiting.calls -= 1;
        if waiting.calls == 0 {
            stages.remove(&stage);
            if let Some(hook) = &self.hook {
                hook(&StageEvent::PortAnswered { stage });
            }
        }
    }

    /// The stages waiting now, ascending.
    pub fn stages(&self) -> Vec<StageId> {
        self.stages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect()
    }

    /// The calls waiting now: each holds the worker that made it.
    pub fn calls(&self) -> u32 {
        self.stages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|w| w.calls)
            .sum()
    }

    /// The port `stage` waits on, when it waits.
    pub fn port_of(&self, stage: StageId) -> Option<String> {
        self.stages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&stage)
            .map(|w| w.port.clone())
    }
}

/// A call in progress; dropping it ends the wait.
pub struct WaitGuard {
    waits: Arc<StageWaits>,
    stage: StageId,
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        self.waits.end(self.stage);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    impl GuestPort for Echo {
        fn call(&self, request: Vec<u8>, _call: &GuestCall) -> Answer {
            Answer::Ok(request)
        }
    }

    struct Sleeps;

    impl GuestPort for Sleeps {
        fn call(&self, _request: Vec<u8>, call: &GuestCall) -> Answer {
            while !call.cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            Answer::Error("stopped".into())
        }
    }

    fn ports(deadline_ms: u64) -> (Arc<StagePorts>, Arc<StageWaits>, CancelToken) {
        let mut routes = BTreeMap::new();
        routes.insert(
            "echo".to_string(),
            Route {
                place: "guest",
                port: GuestRoute::new(Arc::new(Echo)),
                deadline: Duration::from_millis(deadline_ms),
            },
        );
        routes.insert(
            "slow".to_string(),
            Route {
                place: "guest",
                port: GuestRoute::new(Arc::new(Sleeps)),
                deadline: Duration::from_millis(deadline_ms),
            },
        );
        let told = Arc::new(Mutex::new(Vec::new()));
        let hook: StageHook = {
            let told = Arc::clone(&told);
            Arc::new(move |e: &StageEvent| told.lock().unwrap().push(e.clone()))
        };
        let waits = StageWaits::new(Some(hook));
        let cancel = CancelToken::new();
        let stage = StagePorts::new(2, routes);
        stage.bind_run(RunPorts {
            cancel: cancel.clone(),
            waits: Arc::clone(&waits),
        });
        (stage, waits, cancel)
    }

    #[test]
    fn a_listed_port_answers_and_the_wait_is_told() {
        let (stage, waits, _) = ports(10_000);
        assert_eq!(
            stage.call("echo", b"hi".to_vec()),
            Ok(Answer::Ok(b"hi".to_vec()))
        );
        assert!(waits.stages().is_empty());
        assert_eq!(waits.calls(), 0);
    }

    #[test]
    fn an_unlisted_name_is_refused_by_name() {
        let (stage, _, _) = ports(10_000);
        let failure = stage.call("model", Vec::new()).unwrap_err();
        let text = failure.clone().into_error(2).to_string();
        assert!(
            text.contains("port `model` is not listed for this stage (it lists echo, slow)"),
            "{text}"
        );
        assert!(StagePorts::none().call("x", Vec::new()).is_err());
    }

    #[test]
    fn a_deadline_ends_the_wait_and_tells_the_handler() {
        let (stage, waits, _) = ports(50);
        let t = Instant::now();
        let failure = stage.call("slow", Vec::new()).unwrap_err();
        assert_eq!(
            failure,
            CallFailure::Deadline {
                name: "slow".into(),
                ms: 50
            }
        );
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(waits.stages().is_empty());
    }

    #[test]
    fn a_cancel_ends_the_wait_promptly() {
        let (stage, waits, cancel) = ports(600_000);
        let canceller = std::thread::spawn({
            let waits = Arc::clone(&waits);
            move || {
                while waits.stages().is_empty() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert_eq!(waits.port_of(2).as_deref(), Some("slow"));
                cancel.cancel();
                Instant::now()
            }
        });
        let failure = stage.call("slow", Vec::new()).unwrap_err();
        let cancelled_at = canceller.join().unwrap();
        assert_eq!(failure, CallFailure::Cancelled);
        assert!(cancelled_at.elapsed() < Duration::from_millis(500));
        assert!(matches!(
            CallFailure::Cancelled.into_error(2),
            MorunaError::Cancelled
        ));
    }

    #[test]
    fn a_request_past_the_bound_is_refused() {
        let (stage, _, _) = ports(1000);
        let failure = stage.call("echo", vec![0; PORT_BYTES_MAX + 1]).unwrap_err();
        assert!(matches!(failure, CallFailure::TooLarge { .. }));
    }

    #[test]
    fn an_unbound_stage_says_so() {
        let mut routes = BTreeMap::new();
        routes.insert(
            "echo".to_string(),
            Route {
                place: "guest",
                port: GuestRoute::new(Arc::new(Echo)),
                deadline: Duration::from_secs(1),
            },
        );
        let stage = StagePorts::new(1, routes);
        assert!(matches!(
            stage.call("echo", Vec::new()),
            Err(CallFailure::Unbound { .. })
        ));
    }
}
