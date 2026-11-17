// SPDX-License-Identifier: MPL-2.0

//! Kernel initialization.

use core::sync::atomic::{AtomicU8, Ordering};

use aster_cmdline::INIT_PROC_ARGS;
use component::InitStage;
use ostd::cpu::CpuId;
use spin::once::Once;

use crate::{
    fs::{
        initramfs, rootfs,
        vfs::path::{FsPath, MountNamespace, Path, PathResolver},
    },
    prelude::*,
    process::{Process, spawn_init_process},
    sched::SchedPolicy,
    thread::kernel_thread::ThreadOptions,
};

pub(super) fn main() {
    // Initialize the global states for all CPUs.
    ostd::early_println!("OSTD initialized. Preparing components.");
    component::init_all(InitStage::Bootstrap, component::parse_metadata!()).unwrap();
    init();

    // Initialize the per-CPU states for BSP.
    init_on_each_cpu();

    // User space: no other CPU boots here; the core runs the personality's threads on all
    // of them.

    // Give the control of the BSP to the idle thread.
    ThreadOptions::new(bsp_idle_loop)
        .cpu_affinity(CpuId::bsp().into())
        .sched_policy(SchedPolicy::Idle)
        .spawn();
}

pub(super) fn on_first_process_startup(ctx: &Context) {
    component::init_all(InitStage::Process, component::parse_metadata!()).unwrap();
    crate::device::init_in_first_process().unwrap();
    crate::fs::init_in_first_process(ctx);
}

fn init() {
    crate::power::init();
    crate::arch::init();
    crate::thread::init();
    crate::util::random::init();
    crate::time::init();
    crate::sched::init();
    crate::process::init();
    crate::fs::init();
    crate::security::init();
}

fn init_on_each_cpu() {
    crate::sched::init_on_each_cpu();
    crate::process::init_on_each_cpu();
    crate::fs::init_on_each_cpu();
    crate::time::init_on_each_cpu();
}

//--------------------------------------------------------------------------
// Per-CPU idle threads
//--------------------------------------------------------------------------

// Note: Keep the code in the idle loop to the bare minimum.
//
// We do not want the idle loop to
// rely on the APIs of other kernel subsystems for two reasons.
// First, the idle task must never sleep or block.
// This property is relied upon by the scheduler.
// Second, the idle task is spawned before the kernel is fully initialized.
// So other subsystems may not be ready, yet.
//
// In addition,
// doing more work in the idle task may have negative impact on
// the latency to switching from the idle task to a useful, runnable one.

fn bsp_idle_loop() {
    ostd::info!("Idle thread for CPU #0 started");

    // Spawn the first non-idle kernel thread on BSP.
    ThreadOptions::new(first_kthread)
        .cpu_affinity(CpuId::bsp().into())
        .sched_policy(SchedPolicy::default())
        .spawn();

    // Wait till the init process is spawned.
    let init_process = loop {
        if let Some(init_process) = INIT_PROCESS.get() {
            break init_process;
        };

        ostd::task::halt_cpu();
    };

    // Wait till the init process becomes zombie.
    while !init_process.status().is_zombie() {
        ostd::task::halt_cpu();
    }

    // User space: the personality's work ends with its init process.
    crate::syscall::refused::report_task();
    let code = init_process.status().exit_code();
    crate::start::finish(crate::start::Ending::Ran(code));
    ostd::info!("The init process terminates with code {:?}", code);
    ostd::power::poweroff(if code == 0 {
        ostd::power::ExitCode::Success
    } else {
        ostd::power::ExitCode::Failure
    });
}

//--------------------------------------------------------------------------
// The first kernel thread
//--------------------------------------------------------------------------

// The main function of the first (non-idle) kernel thread
fn first_kthread() {
    println!("Spawn the first kernel thread");

    let init_mnt_ns = MountNamespace::get_init_singleton();
    let fs_resolver = init_mnt_ns.new_path_resolver();
    init_in_first_kthread(&fs_resolver);
    let boot_init = prepare_boot_init(fs_resolver).unwrap_or_else(|e| no_program(e));

    print_banner();

    INIT_PROCESS.call_once(|| {
        let (argv, envp) = match crate::start::command() {
            // A task's command, exactly as its launcher gave it.
            Some(command) => (
                c_strings(&command.argv).unwrap_or_else(|e| no_program(e)),
                c_strings(&command.env).unwrap_or_else(|e| no_program(e)),
            ),
            None => {
                let karg = INIT_PROC_ARGS.get().unwrap();
                (karg.argv().to_vec(), karg.envp().to_vec())
            }
        };
        boot_init
            .spawn(argv, envp)
            .unwrap_or_else(|e| no_program(e))
    });
}

struct BootInit {
    path_resolver: PathResolver,
    init_path: (Path, &'static str),
}

#[repr(u8)]
enum BootSource {
    Initramfs = 0,
    Rootfs = 1,
}

pub(crate) fn booted_from_rootfs() -> bool {
    BOOT_SOURCE.load(Ordering::Relaxed) == BootSource::Rootfs as u8
}

impl BootInit {
    fn spawn(self, argv: Vec<CString>, envp: Vec<CString>) -> Result<Arc<Process>> {
        let Self {
            path_resolver,
            init_path: (init_path, init_name),
        } = self;

        println!("[kernel] running {} as the init process", init_name);
        // A task's command carries its own `argv[0]`.
        let argv = if crate::start::command().is_some() {
            argv
        } else {
            with_init_argv0(init_name, argv)
        };
        spawn_init_process(path_resolver, init_path, argv, envp)
    }
}

/// The program the personality runs first (data-os.md section 3a): a task's is the one its
/// command names on its image (`init=`), a booted test's is the initramfs's (`rdinit=`,
/// `/init` when not given). There is no default search and no shell to fall back on, so a
/// program that is not there is an error.
fn prepare_boot_init(mut path_resolver: PathResolver) -> Result<BootInit> {
    if rootfs::task_fs().is_some() {
        rootfs::switch_to_rootfs(&mut path_resolver)?;
        BOOT_SOURCE.store(BootSource::Rootfs as u8, Ordering::Relaxed);
        let init_path = match crate::start::command() {
            Some(command) => {
                let name: &'static str = String::from_utf8_lossy(&command.path).into_owned().leak();
                (path_resolver.lookup(&FsPath::try_from(name)?)?, name)
            }
            None => rootfs::find_init(&path_resolver)?.ok_or_else(|| {
                Error::with_message(Errno::ENOENT, "the task's command names no program")
            })?,
        };
        return Ok(BootInit {
            path_resolver,
            init_path,
        });
    }
    let init_path = initramfs::find_init(&path_resolver)?;
    Ok(BootInit {
        path_resolver,
        init_path,
    })
}

/// `strings` as C strings: none may hold a zero byte.
fn c_strings(strings: &[Vec<u8>]) -> Result<Vec<CString>> {
    strings
        .iter()
        .map(|s| {
            CString::new(s.clone())
                .map_err(|_| Error::with_message(Errno::EINVAL, "a zero byte in the command"))
        })
        .collect()
}

/// Ends the personality when its first program cannot run.
fn no_program(error: Error) -> ! {
    ostd::early_println!("linux: FAIL: the task's program cannot run: {:?}", error);
    crate::start::finish(crate::start::Ending::NotStarted(error.error()));
    crate::syscall::refused::report_task();
    ostd::power::poweroff(ostd::power::ExitCode::Failure)
}

fn with_init_argv0(init_name: &str, mut argv: Vec<CString>) -> Vec<CString> {
    // Linux prepends the init executable path as `argv[0]`.
    // Reference: <https://elixir.bootlin.com/linux/v6.19/source/init/main.c#L1491>.
    argv.insert(0, CString::new(init_name).unwrap());
    argv
}

static INIT_PROCESS: Once<Arc<Process>> = Once::new();

fn init_in_first_kthread(path_resolver: &PathResolver) {
    component::init_all(InitStage::Kthread, component::parse_metadata!()).unwrap();
    crate::thread::init_in_first_kthread();
    crate::fs::init_in_first_kthread(path_resolver);
    crate::device::init_in_first_kthread();
}

fn print_banner() {
    println!("");
    println!("Asterinas's Linux layer, as Moruna's personality");
}

static BOOT_SOURCE: AtomicU8 = AtomicU8::new(BootSource::Initramfs as u8);
