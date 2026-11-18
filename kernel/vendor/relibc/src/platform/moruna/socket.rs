//! Sockets: a function's data comes through the plane, and its network (system design 3D) is
//! not a socket layer, so every call answers `ENOSYS`.

use super::{Sys, nosys};
use crate::{
    error::Result,
    header::sys_socket::{msghdr, socklen_t},
    out::Out,
    platform::{PalSocket, types::c_int},
};

impl PalSocket for Sys {
    fn accept(_socket: c_int, _address_dst: Option<&mut [u8]>) -> Result<(c_int, socklen_t)> {
        nosys()
    }

    fn bind(_socket: c_int, _address_raw: &[u8]) -> Result<()> {
        nosys()
    }

    fn connect(_socket: c_int, _address_raw: &[u8]) -> Result<c_int> {
        nosys()
    }

    fn getpeername(_socket: c_int, _address_dst: &mut [u8]) -> Result<socklen_t> {
        nosys()
    }

    fn getsockname(_socket: c_int, _address_dst: &mut [u8]) -> Result<socklen_t> {
        nosys()
    }

    fn getsockopt(
        _socket: c_int,
        _level: c_int,
        _option_name: c_int,
        _option_value: &mut [u8],
    ) -> Result<socklen_t> {
        nosys()
    }

    fn listen(_socket: c_int, _backlog: c_int) -> Result<()> {
        nosys()
    }

    fn recvfrom(
        _socket: c_int,
        _buf: Out<[u8]>,
        _flags: c_int,
        _address_raw: Option<&mut [u8]>,
    ) -> Result<(usize, socklen_t)> {
        nosys()
    }

    unsafe fn recvmsg(_socket: c_int, _msg: *mut msghdr, _flags: c_int) -> Result<usize> {
        nosys()
    }

    unsafe fn sendmsg(_socket: c_int, _msg: *const msghdr, _flags: c_int) -> Result<usize> {
        nosys()
    }

    fn sendto(
        _socket: c_int,
        _buf: &[u8],
        _flags: c_int,
        _dest_addr: Option<&[u8]>,
    ) -> Result<usize> {
        nosys()
    }

    fn setsockopt(
        _socket: c_int,
        _level: c_int,
        _option_name: c_int,
        _option_value: &[u8],
    ) -> Result<()> {
        nosys()
    }

    fn shutdown(_socket: c_int, _how: c_int) -> Result<()> {
        nosys()
    }

    fn socket(_domain: c_int, _kind: c_int, _protocol: c_int) -> Result<c_int> {
        nosys()
    }

    fn socketpair(
        _domain: c_int,
        _kind: c_int,
        _protocol: c_int,
        _sv: &mut [c_int; 2],
    ) -> Result<()> {
        nosys()
    }
}
