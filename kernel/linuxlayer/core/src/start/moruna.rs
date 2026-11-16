// SPDX-License-Identifier: MPL-2.0

//! A task's start on the target: the handles its launcher grants, made into the ports its
//! filesystems and descriptors are built on, and given to `fs::rootfs` before the layer
//! boots.

use core::{mem::ManuallyDrop, sync::atomic::AtomicU64};

use imagefs_protocol::imagefs;
use moruna_sys::{
    Channel, DEADLINE_INFINITE, Error as CoreError, Handle, Mapping, Memory, Process, Signals,
    WaitItem, map,
    plane::{self, plane_get, plane_put},
};

use super::{
    image::ImageClient,
    plane::{Input, Output, Region, Slot},
    text::{RingBytes, RingText},
};
use crate::{
    fs::{
        rootfs::{TaskFs, set_task_fs},
        task_streams::{ConsoleText, PlaneInput, PlaneOutput, TaskStreams, TextSink},
    },
    prelude::*,
};

/// How long the image filesystem has to answer the task's hello.
const HELLO_WAIT_NS: u64 = 10_000_000_000;

/// What a task's launcher grants the personality.
pub struct TaskStart {
    /// The task's own connection to the image filesystem, its hello not yet said.
    pub filesystem: Channel,
    /// The bytes the scratch (`/tmp`) may hold: the scratch account's limit.
    pub scratch_quota: usize,
    /// The input slot, attached to the process's plane as slot 0.
    pub input: Option<Handle>,
    /// The output slot, attached after the input (slot 1, or 0 with no input).
    pub output: Option<Handle>,
    /// The engine's log ring and its length; the core's console without one.
    pub log: Option<(Memory, u64)>,
    /// The task's name in its log records.
    pub name: String,
    /// The task's program, as given; the command line's `init=` without one.
    pub command: Option<super::TaskCommand>,
    /// Where to write how the task ended (`linuxlayer_launch::report`), if anywhere.
    pub report: Option<Memory>,
    /// The handles granted to the task's program (`run_granted`, `run_plane`): every entry of
    /// the start page whose name is not the personality's (`linuxlayer_launch::slot`).
    pub program: Vec<ProgramGrant>,
}

/// A handle granted to the task's program, under its name on the program's start page.
pub struct ProgramGrant {
    pub name: String,
    pub handle: Handle,
    pub rights: moruna_sys::Rights,
}

/// The program's handles, held until they are moved into the program's own handle table
/// (`architecture/data-os.md` §4, a program's native calls); the program cannot reach them
/// here.
static PROGRAM: SpinLock<Vec<ProgramGrant>> = SpinLock::new(Vec::new());

/// The report's mapping, kept until the task ends.
static REPORT: SpinLock<Option<Mapping<'static>>> = SpinLock::new(None);

/// Writes `report` where the launcher reads it.
pub(super) fn write_report(report: &linuxlayer_launch::report::Report) {
    if let Some(mapping) = REPORT.lock().as_ref() {
        let _ = mapping.write(0, &linuxlayer_launch::report::encode(report));
    }
}

/// Why a task cannot start.
#[derive(Debug)]
pub enum StartError {
    /// The image filesystem did not answer the hello.
    Image,
    /// The log ring could not be mapped, or is not a ring.
    Log,
    /// The task's filesystems were given already.
    Given,
}

/// Gives the layer the task's filesystems and descriptors from `start`; then the layer's
/// boot runs the task's program on its image.
pub fn start_task(start: TaskStart) -> core::result::Result<(), StartError> {
    if let Some(command) = start.command {
        super::set_command(command);
    }
    *PROGRAM.lock() = start.program;
    if let Some(memory) = start.report {
        let memory: &'static Memory = Box::leak(Box::new(memory));
        let len = linuxlayer_launch::report::LEN as u64;
        if let Ok(mapping) = memory.map_detached(0, len, map::READ | map::WRITE) {
            *REPORT.lock() = Some(mapping);
        }
    }
    let client = imagefs::Client::connect(start.filesystem, moruna_svc::deadline_in(HELLO_WAIT_NS))
        .map_err(|_| StartError::Image)?;
    let text: Arc<dyn TextSink> = match start.log {
        None => Arc::new(ConsoleText),
        Some((memory, len)) => {
            let memory: &'static Memory = Box::leak(Box::new(memory));
            let mapping = memory
                .map_detached(0, len, map::READ | map::WRITE)
                .map_err(|_| StartError::Log)?;
            Arc::new(
                RingText::new(MappedRing(SpinLock::new(mapping)), start.name)
                    .map_err(|_| StartError::Log)?,
            )
        }
    };
    let input_number = 0;
    let output_number = u32::from(start.input.is_some());
    let streams = TaskStreams {
        input: start.input.map(|handle| {
            Arc::new(Input::new(CoreSlot {
                number: input_number,
                handle,
            })) as Arc<dyn PlaneInput>
        }),
        output: start.output.map(|handle| {
            Arc::new(Output::new(CoreSlot {
                number: output_number,
                handle,
            })) as Arc<dyn PlaneOutput>
        }),
        text,
        dropped: Arc::new(AtomicU64::new(0)),
    };
    set_task_fs(TaskFs {
        image: Arc::new(ImageClient::new(client)),
        scratch_quota: start.scratch_quota,
        streams,
    })
    .map_err(|_| StartError::Given)
}

/// The log ring's mapping, kept for as long as the personality runs.
struct MappedRing(SpinLock<Mapping<'static>>);

impl RingBytes for MappedRing {
    fn with<R>(&self, f: impl FnOnce(&mut [u8]) -> R) -> Option<R> {
        let mut mapping = self.0.lock();
        mapping.shared_region().ok().map(f)
    }
}

/// The personality's own process, whose plane the slots are.
fn own_process() -> ManuallyDrop<Process> {
    let raw =
        ostd::backend::self_process().expect("the launcher grants the process its own handle");
    ManuallyDrop::new(Process(Handle::from_raw(raw)))
}

fn core_failed(_: CoreError) -> Error {
    Error::with_message(Errno::EIO, "the core refused a plane call")
}

/// A batch or a window the core mapped.
struct CoreRegion(plane::Region);

impl Region for CoreRegion {
    fn len(&self) -> u64 {
        self.0.len()
    }

    fn read(&self, at: u64, out: &mut [u8]) -> Result<()> {
        self.0.read(at, out).map_err(core_failed)
    }

    fn write(&self, at: u64, data: &[u8]) -> Result<()> {
        self.0.write(at, data).map_err(core_failed)
    }
}

/// One of the process's plane slots: its number there, and a handle to wait on it.
struct CoreSlot {
    number: u32,
    handle: Handle,
}

impl Slot for CoreSlot {
    type Region = CoreRegion;

    fn wait(&self) -> Result<()> {
        match moruna_sys::wait_one(&self.handle, Signals::SIGNALED, DEADLINE_INFINITE) {
            Ok(_) => Ok(()),
            // A signal for the thread (`Task::interrupt`): the call ends with `EINTR`.
            Err(CoreError::Interrupted) => return_errno!(Errno::EINTR),
            Err(e) => Err(core_failed(e)),
        }
    }

    fn ready(&self) -> bool {
        let mut items = [WaitItem::raw(self.handle.raw(), Signals::SIGNALED)];
        moruna_sys::wait(&mut items, moruna_sys::now()).is_ok()
            && items[0].observed().contains(Signals::SIGNALED)
    }

    fn get(&self) -> Result<Option<CoreRegion>> {
        Ok(plane_get(&own_process(), self.number)
            .map_err(core_failed)?
            .map(CoreRegion))
    }

    fn put(&self, window: CoreRegion, len: u64) -> Result<()> {
        plane_put(&own_process(), self.number, window.0, 0, len).map_err(core_failed)
    }
}
