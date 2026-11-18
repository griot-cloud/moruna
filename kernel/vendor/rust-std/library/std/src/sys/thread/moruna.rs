//! moruna: threads over Moruna's runtime (`moruna_sys::rt::thread`), as Motor OS's are over its own.

use crate::io;
use crate::num::NonZero;
use crate::thread::ThreadInit;
use crate::time::Duration;

pub const DEFAULT_MIN_STACK_SIZE: usize = moruna_sys::rt::thread::DEFAULT_MIN_STACK_SIZE;

pub struct Thread(moruna_sys::rt::thread::Thread);

impl Thread {
    pub unsafe fn new(stack: usize, init: Box<ThreadInit>) -> io::Result<Thread> {
        extern "C" fn thread_main(arg: usize) {
            // SAFETY: `arg` is the `ThreadInit` boxed below, given to this thread alone.
            let init = unsafe {
                Box::from_raw(crate::ptr::with_exposed_provenance_mut::<ThreadInit>(arg))
            };
            let rust_start = init.init();
            rust_start();
            // The thread's end is the runtime's (`moruna_sys::rt::thread::end_current`): the C
            // library's thread-local destructors, then std's (`sys::pal::moruna::thread_end`),
            // before it frees the thread's block.
        }

        let arg = Box::into_raw(init).expose_provenance();
        moruna_sys::rt::thread::spawn(stack, thread_main, arg).map(Thread).map_err(|error| {
            // SAFETY: no thread started, so the box is still this function's.
            drop(unsafe {
                Box::from_raw(crate::ptr::with_exposed_provenance_mut::<ThreadInit>(arg))
            });
            crate::sys::pal::error(error)
        })
    }

    pub fn join(self) {
        self.0.join()
    }
}

pub fn available_parallelism() -> io::Result<NonZero<usize>> {
    let cpus = moruna_sys::rt::thread::available_parallelism();
    Ok(NonZero::new(cpus).unwrap_or(NonZero::<usize>::MIN))
}

pub fn yield_now() {
    moruna_sys::rt::thread::yield_now()
}

pub fn sleep(dur: Duration) {
    moruna_sys::rt::thread::sleep(dur)
}
