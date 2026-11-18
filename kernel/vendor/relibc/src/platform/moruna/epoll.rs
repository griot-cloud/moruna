//! epoll: three streams that are always ready or invalid need none; `ENOSYS`.

use super::{Sys, nosys};
use crate::{
    error::Result,
    header::{bits_sigset_t::sigset_t, sys_epoll::epoll_event},
    platform::{PalEpoll, types::c_int},
};

impl PalEpoll for Sys {
    fn epoll_create1(_flags: c_int) -> Result<c_int> {
        nosys()
    }

    fn epoll_ctl(
        _epfd: c_int,
        _op: c_int,
        _fd: c_int,
        _event: Option<&mut epoll_event>,
    ) -> Result<()> {
        nosys()
    }

    fn epoll_pwait(
        _epfd: c_int,
        _events: &mut [epoll_event],
        _timeout: c_int,
        _sigmask: Option<&sigset_t>,
    ) -> Result<usize> {
        nosys()
    }
}
