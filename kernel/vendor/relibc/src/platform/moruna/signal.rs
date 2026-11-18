//! Signals, within the process: nothing outside a task sends it one. `sigaction` and
//! `sigprocmask` keep what they are given; `raise` (and `kill` of the process itself) runs the
//! handler installed, ignores a signal set to be ignored, and otherwise takes the default
//! action, which for every signal that has one ends the process with 128 plus the signal, as a
//! shell reports a process a signal ended. Timers that deliver signals answer `ENOSYS`.

use super::{Sys, nosys};
#[expect(deprecated)]
use crate::header::sys_time::itimerval;
use crate::{
    error::{Errno, Result},
    header::{
        bits_sigset_t::sigset_t,
        errno::{EINVAL, ESRCH},
        signal::{
            SIG_BLOCK, SIG_DFL, SIG_IGN, SIG_SETMASK, SIG_UNBLOCK, SIGCHLD, SIGCONT, SIGKILL,
            SIGRTMAX, SIGSTOP, SIGURG, SIGWINCH, SS_DISABLE, sigaction, siginfo_t, sigval, stack_t,
        },
        time::timespec,
    },
    platform::{
        Pal, PalSignal,
        types::{c_int, pid_t},
    },
    sync::Mutex,
};

/// Each signal's action, as `sigaction` last set it; none is the default.
static ACTIONS: Mutex<[Option<sigaction>; SIGRTMAX + 1]> =
    Mutex::new([const { None }; SIGRTMAX + 1]);

/// The signals blocked, which nothing delivers anyway.
static MASK: Mutex<sigset_t> = Mutex::new(0);

fn number(sig: c_int) -> Result<usize> {
    match usize::try_from(sig) {
        Ok(n) if (1..=SIGRTMAX).contains(&n) => Ok(n),
        _ => Err(Errno(EINVAL)),
    }
}

/// Whether a signal's default action leaves the process running.
fn ignored_by_default(sig: usize) -> bool {
    matches!(sig, SIGCHLD | SIGCONT | SIGURG | SIGWINCH)
}

/// Delivers `sig` to the calling thread: its handler, nothing, or the end of the process.
fn deliver(sig: usize) -> Result<()> {
    let action = ACTIONS.lock()[sig].clone();
    let handler = action.and_then(|a| a.sa_handler).map(|h| h as usize);
    match handler {
        Some(SIG_IGN) => Ok(()),
        Some(h) if h != SIG_DFL => {
            // SAFETY: a handler `sigaction` was given is a `void (*)(int)`.
            let f: extern "C" fn(c_int) = unsafe { core::mem::transmute(h) };
            f(sig as c_int);
            Ok(())
        }
        _ if ignored_by_default(sig) => Ok(()),
        _ => Sys::exit(128 + sig as c_int),
    }
}

impl PalSignal for Sys {
    #[expect(deprecated)]
    fn getitimer(_which: c_int, _out: &mut itimerval) -> Result<()> {
        nosys()
    }

    fn kill(pid: pid_t, sig: c_int) -> Result<()> {
        if pid != 0 && pid != -1 && pid != Self::getpid() {
            return Err(Errno(ESRCH));
        }
        Self::raise(sig)
    }

    fn sigqueue(pid: pid_t, sig: c_int, _val: sigval) -> Result<()> {
        Self::kill(pid, sig)
    }

    fn killpg(pgrp: pid_t, sig: c_int) -> Result<()> {
        Self::kill(pgrp, sig)
    }

    fn raise(sig: c_int) -> Result<()> {
        // Signal 0 checks that the process exists: it does.
        if sig == 0 {
            return Ok(());
        }
        deliver(number(sig)?)
    }

    #[expect(deprecated)]
    fn setitimer(_which: c_int, _new: &itimerval, _old: Option<&mut itimerval>) -> Result<()> {
        nosys()
    }

    fn sigaction(sig: c_int, act: Option<&sigaction>, oact: Option<&mut sigaction>) -> Result<()> {
        let n = number(sig)?;
        if act.is_some() && (n == SIGKILL || n == SIGSTOP) {
            return Err(Errno(EINVAL));
        }
        let mut actions = ACTIONS.lock();
        if let Some(oact) = oact {
            *oact = actions[n].clone().unwrap_or(sigaction {
                sa_handler: None,
                sa_flags: 0,
                sa_restorer: None,
                sa_mask: 0,
            });
        }
        if let Some(act) = act {
            actions[n] = Some(act.clone());
        }
        Ok(())
    }

    unsafe fn sigaltstack(_ss: Option<&stack_t>, old_ss: Option<&mut stack_t>) -> Result<()> {
        // No signal arrives on a stack of its own: the alternate stack is never used.
        if let Some(old) = old_ss {
            old.ss_sp = core::ptr::null_mut();
            old.ss_size = 0;
            old.ss_flags = SS_DISABLE as c_int;
        }
        Ok(())
    }

    fn sigpending(set: &mut sigset_t) -> Result<()> {
        *set = 0;
        Ok(())
    }

    fn sigprocmask(how: c_int, set: Option<&sigset_t>, oset: Option<&mut sigset_t>) -> Result<()> {
        let mut mask = MASK.lock();
        if let Some(oset) = oset {
            *oset = *mask;
        }
        if let Some(set) = set {
            *mask = match how {
                SIG_BLOCK => *mask | set,
                SIG_UNBLOCK => *mask & !set,
                SIG_SETMASK => *set,
                _ => return Err(Errno(EINVAL)),
            };
        }
        Ok(())
    }

    fn sigsuspend(_mask: &sigset_t) -> Errno {
        // No signal would ever end the wait.
        Errno(crate::header::errno::ENOSYS)
    }

    fn sigtimedwait(
        _set: &sigset_t,
        _sig: Option<&mut siginfo_t>,
        tp: Option<&timespec>,
    ) -> Result<c_int> {
        // None arrives: the wait lasts its timeout, then finds nothing.
        match tp {
            Some(t) => {
                Self::nanosleep(t, None)?;
                Err(Errno(crate::header::errno::EAGAIN))
            }
            None => nosys(),
        }
    }
}
