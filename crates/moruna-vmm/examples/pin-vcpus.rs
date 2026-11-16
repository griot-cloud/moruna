//! The launcher's vCPU placement (`moruna_vmm::placement::place`) on a QEMU started paused
//! elsewhere (`-S`, `-name ...,debug-threads=on`, a QMP socket), then the guest let run: what
//! stream D measures a pinned worker CPU's loss with where the launcher itself cannot start
//! QEMU (`kernel/node/HOST.md`).
//!
//!   pin-vcpus QEMU_PID BACKEND_PID QMP_SOCKET [CPU_LIST]
//!
//! Without a CPU list the guest is only let run, unplaced.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use moruna_vmm::placement::{self, Placement};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let run = || -> Result<(), String> {
        let [qemu, backend, socket, rest @ ..] = args.as_slice() else {
            return Err("usage: pin-vcpus QEMU_PID BACKEND_PID QMP_SOCKET [CPU_LIST]".into());
        };
        let qemu: i32 = qemu.parse().map_err(|_| "QEMU_PID is a number")?;
        let backend: i32 = backend.parse().map_err(|_| "BACKEND_PID is a number")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        if let Some(list) = rest.first() {
            let cpus = placement::parse_cpu_list(list).map_err(|e| e.to_string())?;
            let allowed = placement::allowed_cpus().map_err(|e| e.to_string())?;
            let wanted =
                Placement::check(&cpus, cpus.len() as u32, &allowed).map_err(|e| e.to_string())?;
            let alive = || std::path::Path::new(&format!("/proc/{qemu}")).exists();
            let placed = placement::place(&wanted, qemu, &[backend], deadline, &alive)
                .map_err(|e| e.to_string())?;
            for (vcpu, tid, cpu) in &placed.vcpus {
                println!("vCPU {vcpu}: host thread {tid} on host CPU {cpu}");
            }
            println!("{} other threads kept off those CPUs", placed.others);
        }
        let stream = loop {
            match UnixStream::connect(socket) {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => return Err(format!("{socket}: {e}")),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;
        let mut reader = BufReader::new(stream);
        let answer = |reader: &mut BufReader<UnixStream>| -> Result<String, String> {
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).map_err(|e| e.to_string())?;
                if line.is_empty() {
                    return Err("QMP closed".into());
                }
                if !line.contains("\"event\"") {
                    return Ok(line);
                }
            }
        };
        answer(&mut reader)?;
        for command in ["qmp_capabilities", "cont"] {
            reader
                .get_mut()
                .write_all(format!("{{\"execute\":\"{command}\"}}\n").as_bytes())
                .map_err(|e| e.to_string())?;
            let reply = answer(&mut reader)?;
            if !reply.contains("\"return\"") {
                return Err(format!("QMP refused {command}: {reply}"));
            }
        }
        println!("the guest runs");
        Ok(())
    };
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pin-vcpus: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("pin-vcpus: Linux only");
    std::process::exit(2);
}
