// SPDX-License-Identifier: MPL-2.0

//! The socket calls, in a personality with no network (data-os.md §4: sockets stay out
//! of 0.7.0). No descriptor is ever a socket, so a socket cannot be made (`EAFNOSUPPORT`,
//! as Linux answers for a family it lacks), and a call on a descriptor is `EBADF` for
//! one that is not open and `ENOTSOCK` for one that is.

use super::SyscallReturn;
use crate::{fs::file::file_table::FileDesc, prelude::*};

fn not_a_socket(fd: FileDesc, ctx: &Context) -> Result<SyscallReturn> {
    let file_table = ctx.thread_local.borrow_file_table();
    let _file = file_table.unwrap().read().get_file(fd)?.clone();
    return_errno_with_message!(Errno::ENOTSOCK, "the file is not a socket")
}

fn no_family() -> Result<SyscallReturn> {
    return_errno_with_message!(Errno::EAFNOSUPPORT, "the personality has no sockets")
}

pub(super) fn sys_socket(_domain: i32, _type: i32, _protocol: i32, _ctx: &Context) -> Result<SyscallReturn> {
    no_family()
}

pub(super) fn sys_socketpair(
    _domain: i32,
    _type: i32,
    _protocol: i32,
    _sv: Vaddr,
    _ctx: &Context,
) -> Result<SyscallReturn> {
    no_family()
}

macro_rules! on_a_descriptor {
    ($($name:ident($($arg:ident),*);)*) => {$(
        pub(super) fn $name(fd: FileDesc, $($arg: u64,)* ctx: &Context) -> Result<SyscallReturn> {
            $(let _ = $arg;)*
            not_a_socket(fd, ctx)
        }
    )*};
}

on_a_descriptor! {
    sys_accept(a, b);
    sys_accept4(a, b, c);
    sys_bind(a, b);
    sys_connect(a, b);
    sys_getpeername(a, b);
    sys_getsockname(a, b);
    sys_getsockopt(a, b, c, d);
    sys_listen(a);
    sys_recvfrom(a, b, c, d, e);
    sys_recvmsg(a, b);
    sys_sendmmsg(a, b, c);
    sys_sendmsg(a, b);
    sys_sendto(a, b, c, d, e);
    sys_setsockopt(a, b, c, d);
    sys_shutdown(a);
}
