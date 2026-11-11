//! `moruna-vhost-vsock --socket PATH --cid CID --uds PATH` (SDD 30 f.11): the vsock device as
//! a vhost-user back end for QEMU's `vhost-user-vsock-pci`, its host side the muxer's Unix
//! sockets (a host process connects to the `--uds` path and writes `CONNECT <port>\n`; a guest
//! connection to host port `P` reaches `<uds>_P`). It serves the one front end that connects
//! and exits when it goes: 0, or 1 with the reason on standard error. Before it binds anything
//! it confines itself with its own system-call filter (`moruna_vmm::jail::seccomp`); a call
//! outside it kills the process.

fn main() {
    let code = run();
    // The server has dropped its resources. Exit directly so foreign atexit
    // handlers cannot attempt file or process operations after confinement.
    // SAFETY: _exit terminates this process and never accesses Rust memory.
    unsafe { libc::_exit(code) }
}

const USAGE: &str = "usage: moruna-vhost-vsock --socket PATH --cid CID --uds PATH";

#[cfg(target_os = "linux")]
fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut socket, mut cid, mut uds) = (None, None, None);
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match (flag.as_str(), it.next()) {
            ("--socket", Some(v)) => socket = Some(std::path::PathBuf::from(v)),
            ("--uds", Some(v)) => uds = Some(std::path::PathBuf::from(v)),
            ("--cid", Some(v)) => match v.parse::<u32>() {
                Ok(c) if c > 2 => cid = Some(c),
                _ => {
                    eprintln!("moruna-vhost-vsock: --cid {v}: a guest's cid is a number above 2");
                    return 2;
                }
            },
            _ => {
                eprintln!("{USAGE}");
                return 2;
            }
        }
    }
    let (Some(socket), Some(cid), Some(uds)) = (socket, cid, uds) else {
        eprintln!("{USAGE}");
        return 2;
    };
    if let Err(e) = moruna_vmm::jail::seccomp::confine_vsock_backend() {
        eprintln!("moruna-vhost-vsock: {e}");
        return 1;
    }
    match moruna_vmm::vhost_user::backend::serve(&socket, cid, &uds) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("moruna-vhost-vsock: {e}");
            1
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn run() -> i32 {
    eprintln!(
        "moruna-vhost-vsock: a vhost-user back end maps the guest's memory from the descriptors \
         QEMU shares, which is Linux's; this build is not for Linux ({USAGE})"
    );
    1
}
