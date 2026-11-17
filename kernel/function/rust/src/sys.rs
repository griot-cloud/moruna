//! The task's lane on the core: `moruna_sys::plane`'s rings and log, as the gate's tasks used
//! them (`images/gate/tasks` before 5.1): the plane taken once, the rings attached from its slots
//! 0 and 1, the log from its start slot.

use std::ptr::NonNull;
use std::sync::{Mutex, Once, PoisonError};

use moruna_sys::plane::{Log, Plane, Rings};

use crate::code;
use crate::lane::{Lane, Posted, Unattached, Window};

/// The task's log, taken from its start slot the first time a line is written.
static LOG: Mutex<Option<Log>> = Mutex::new(None);
static TAKEN: Once = Once::new();

fn with_log(f: impl FnOnce(&mut Log)) {
    TAKEN.call_once(|| *LOG.lock().unwrap_or_else(PoisonError::into_inner) = Log::take());
    if let Some(log) = LOG.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
        f(log);
    }
}

/// A line of the task's log; a line with newlines is several.
pub(crate) fn log_line(line: &str) {
    with_log(|log| {
        if line.is_empty() {
            log.line(b"");
        }
        for part in line.lines() {
            log.line(part.as_bytes());
        }
    });
}

/// Says how many lines were dropped, if any: before the task ends.
fn flush_log() {
    with_log(Log::finish);
}

/// The task's rings.
pub(crate) struct SysLane {
    rings: Rings,
    name: &'static str,
    /// Holds the plane's slots for as long as the rings are mapped.
    _plane: Plane,
}

/// The task's plane and its rings, once.
pub(crate) fn attach() -> Result<SysLane, Unattached> {
    let name = moruna_sys::start::process_name();
    let Some(plane) = Plane::take() else {
        return Err(Unattached {
            code: code::NO_PLANE,
            line: format!("{name}: no plane: the task was not started on the data plane"),
        });
    };
    match Rings::attach(&plane) {
        Ok(rings) => Ok(SysLane {
            rings,
            name,
            _plane: plane,
        }),
        Err(e) => Err(Unattached {
            code: code::NO_RINGS,
            line: format!("{name}: no rings: {e:?}"),
        }),
    }
}

impl Lane for SysLane {
    fn name(&self) -> &str {
        self.name
    }

    fn batch(&mut self) -> Result<Option<Posted>, String> {
        match self.rings.batch() {
            Ok(Some((e, bytes))) => Ok(Some(Posted {
                cookie: e.cookie,
                addr: NonNull::from(bytes).cast(),
                len: bytes.len(),
                keep: None,
            })),
            Ok(None) => Ok(None),
            Err(e) => Err(format!("{e:?}")),
        }
    }

    fn ended(&mut self) -> bool {
        self.rings.ended()
    }

    fn wait_input(&mut self) {
        self.rings.wait_input();
    }

    fn window_bytes(&self) -> u64 {
        self.rings.half().geometry.window_bytes
    }

    fn window(&mut self) -> Result<Option<Window>, String> {
        match self.rings.window() {
            Ok(Some((w, bytes))) => Ok(Some(Window {
                number: w,
                len: bytes.len(),
                addr: NonNull::from(bytes).cast(),
            })),
            Ok(None) => Ok(None),
            Err(e) => Err(format!("{e:?}")),
        }
    }

    fn wait_window(&mut self) -> bool {
        self.rings.wait_window();
        true
    }

    fn post(&mut self, window: u64, len: u64, cookie: u64) -> bool {
        self.rings.post(window, len, cookie)
    }

    fn release(&mut self) {
        self.rings.release();
    }

    fn exit(&mut self, code: i64) -> i64 {
        flush_log();
        self.rings.note();
        self.rings.exit(code)
    }

    fn log(&mut self, line: &str) {
        log_line(line);
    }
}
