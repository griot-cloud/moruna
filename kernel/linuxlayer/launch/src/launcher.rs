// SPDX-License-Identifier: MPL-2.0

//! The launcher, on Moruna's core: a created process given the personality.

use alloc::vec::Vec;

use moruna_sys::{
    Channel, Error, Handle, Memory, Process, Result, Rights, map, personality, plane,
};

use crate::{PAGE, Plan, REGION_BASE, REGION_LEN, STACK_SIZE, segment_flags};

/// Why a launch failed.
#[derive(Debug)]
pub enum LaunchError {
    /// The image cannot be planned.
    Plan(crate::PlanError),
    /// A call failed: what it was doing, and the error.
    Call(&'static str, Error),
}

fn call<T>(r: Result<T>, what: &'static str) -> core::result::Result<T, LaunchError> {
    r.map_err(|e| LaunchError::Call(what, e))
}

/// Creates a process named `name` from the process resource `resource`, gives it the
/// personality image `image` (its ELF bytes) and the handles `grants` (each moved in under
/// its slot name, a slot with no handle left out; the process's own handle is added as
/// `self`), and starts its first thread in the personality.
pub fn launch(
    resource: &Handle,
    name: &str,
    image: &[u8],
    grants: &mut [(Option<Handle>, &str)],
) -> core::result::Result<Process, LaunchError> {
    launch_with(resource, name, image, grants, Options::default()).map(|(p, _)| p)
}

/// What runs with a task's process made, just before it starts.
pub type BeforeStart<'a> = &'a mut dyn FnMut(&Process) -> Result<()>;

/// How a task's process is made, beyond its personality and handles.
#[derive(Default)]
pub struct Options<'a> {
    /// The account the process is charged to (`process_create`'s), the caller's own when
    /// `None`.
    pub account: Option<&'a Handle>,
    /// The task's data plane.
    pub plane: Option<Plane<'a>>,
    /// Called with the process made and given everything, just before it starts: a
    /// refusal stops the launch.
    pub before_start: Option<BeforeStart<'a>>,
}

/// A task's data plane: its slots, numbered in order (input first), and its port.
pub struct Plane<'a> {
    pub slots: &'a [&'a plane::Slot],
    pub port: Option<Channel>,
}

/// As [`launch`], and gives the process `plane` first (`process_plane`). The caller grants
/// the personality its own handles to the slots too, as `slot::INPUT` and `slot::OUTPUT`,
/// to wait on them.
pub fn launch_task(
    resource: &Handle,
    name: &str,
    image: &[u8],
    grants: &mut [(Option<Handle>, &str)],
    plane: Option<Plane<'_>>,
) -> core::result::Result<Process, LaunchError> {
    let options = Options {
        plane,
        ..Options::default()
    };
    launch_with(resource, name, image, grants, options).map(|(p, _)| p)
}

/// As [`launch`], with `options`; the process and its first thread's id.
pub fn launch_with(
    resource: &Handle,
    name: &str,
    image: &[u8],
    grants: &mut [(Option<Handle>, &str)],
    options: Options<'_>,
) -> core::result::Result<(Process, u64), LaunchError> {
    let Options {
        account,
        plane,
        before_start,
    } = options;
    let plan = Plan::new(image).map_err(LaunchError::Plan)?;
    let process = match account {
        Some(account) => call(
            Process::create_in(resource, name, account),
            "process_create",
        )?,
        None => call(Process::create(resource, name), "process_create")?,
    };
    if let Some(p) = plane {
        call(
            plane::attach(&process, p.slots, p.port).map_err(|(e, _)| e),
            "process_plane",
        )?;
    }

    // The image's segments, each an object of its own that every process of the container
    // shares: the personality's state is the container's.
    for s in &plan.segments {
        let len = s.page_end() - s.page_start();
        let object = call(Memory::create(len), "a segment's object")?;
        {
            let m = call(object.map(0, len, map::WRITE), "mapping a segment's object")?;
            let file = &image[s.offset as usize..(s.offset + s.filesz) as usize];
            call(
                m.write((s.vaddr - s.page_start()) as usize, file),
                "writing a segment",
            )?;
        }
        call(
            process.map(Some(&object.0), 0, len, s.page_start(), segment_flags(s)),
            "mapping a segment",
        )?;
    }

    // The first thread's stack: zero pages, whose block the thread writes itself.
    call(
        process.map(None, 0, STACK_SIZE, plan.stack, map::READ | map::WRITE),
        "mapping the first stack",
    )?;

    let own = call(
        process
            .0
            .duplicate(Rights::READ | Rights::WRITE | Rights::TRANSFER),
        "duplicating the process's own handle",
    )?;
    let mut all: Vec<(Option<Handle>, &str)> = Vec::new();
    for (handle, name) in grants.iter_mut() {
        if let Some(handle) = handle.take() {
            all.push((Some(handle), *name));
        }
    }
    all.push((Some(own), crate::slot::SELF));
    let start_page = call(process.grant(&mut all), "process_grant")?;

    let mut args = [0u8; crate::STACK_ARGS as usize];
    args[..8].copy_from_slice(&start_page.to_le_bytes());
    call(
        process.write(plan.stack_pointer(), &args),
        "writing the first thread's arguments",
    )?;
    call(
        personality::personality_set_with_copy(
            &process,
            REGION_BASE,
            REGION_LEN,
            plan.copy.clone(),
            plan.recovery,
        ),
        "personality_set",
    )?;
    if let Some(before_start) = before_start {
        call(before_start(&process), "before the start")?;
    }
    let thread = call(
        process.launch(plan.entry, plan.stack_pointer(), 0),
        "process_launch",
    )?;
    Ok((process, thread))
}

/// The length of `object` in bytes, whole pages: the longest mapping from its start the core
/// accepts (a mapping past the end is `OutOfRange`), found by doubling, then halving.
pub fn object_len(object: &Memory) -> u64 {
    let fits = |pages: u64| object.map(0, pages * PAGE, 0).is_ok();
    if !fits(1) {
        return 0;
    }
    let mut lo = 1;
    let mut hi = 2;
    while fits(hi) {
        lo = hi;
        hi *= 2;
    }
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo * PAGE
}
