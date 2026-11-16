//! One boot of a node descriptor through the production KVM launcher, its vCPUs placed or not,
//! until the guest powers off: what stream D measures a pinned worker CPU's loss with
//! (`kernel/node/HOST.md`). The guest's serial log is copied out before the run's directory goes.
//!
//!   placed-boot DESCRIPTOR RUNTIME_DIR QEMU BACKEND SERIAL_OUT SECONDS [CPU_LIST]
//!
//! It prints QEMU's status and each vCPU's host thread and CPU; it exits 0 when the guest powered
//! off with success (QEMU status 33) and 1 otherwise.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use moruna_vmm::{Budget, data::RunDevices, jail::Cgroup, microkernel, placement};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    let args: Vec<String> = std::env::args().skip(1).collect();
    if !(6..=7).contains(&args.len()) {
        eprintln!(
            "usage: placed-boot DESCRIPTOR RUNTIME_DIR QEMU BACKEND SERIAL_OUT SECONDS [CPU_LIST]"
        );
        return std::process::ExitCode::from(2);
    }
    let run = || -> Result<bool, String> {
        let descriptor = PathBuf::from(&args[0]);
        let seconds: u64 = args[5].parse().map_err(|_| "SECONDS is a number")?;
        let vcpu_cpus = match args.get(6) {
            Some(list) => placement::parse_cpu_list(list).map_err(|e| e.to_string())?,
            None => Vec::new(),
        };
        let artifacts = microkernel::inspect(&descriptor).map_err(|e| e.to_string())?;
        let r = &artifacts.resources;
        let budget = Budget {
            memory_bytes: r.job_memory_bytes,
            memory_max_bytes: r.job_memory_bytes,
            cpus: r.cpus,
            cpus_max: r.cpus,
        };
        // No disk of this boot is granted to a run, so any authority is checked against none.
        let authority = microkernel::RunAuthority {
            job_digest: String::new(),
            run_id: "0".repeat(32),
            attempt: 0,
        };
        let started = Instant::now();
        let mut node = microkernel::launch_with(
            &descriptor,
            budget,
            &PathBuf::from(&args[1]),
            &PathBuf::from(&args[2]),
            &PathBuf::from(&args[3]),
            &Cgroup::Undelegated,
            &authority,
            &RunDevices {
                vcpu_cpus,
                ..RunDevices::default()
            },
        )
        .map_err(|e| e.to_string())?;
        println!("launched in {} ms", started.elapsed().as_millis());
        if let Some(placed) = node.placed() {
            for (vcpu, tid, cpu) in &placed.vcpus {
                println!("vCPU {vcpu}: host thread {tid} on host CPU {cpu}");
            }
            println!("{} other threads kept off those CPUs", placed.others);
        }
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let status = loop {
            match node.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Ok(None) => break None,
                Err(e) => {
                    println!("the node ended badly: {e}");
                    break None;
                }
            }
        };
        let serial = node.runtime_dir().join("serial.log");
        std::fs::copy(&serial, &args[4]).map_err(|e| format!("{}: {e}", serial.display()))?;
        println!(
            "QEMU status {:?} after {} ms",
            status.and_then(|s| s.code()),
            started.elapsed().as_millis()
        );
        node.shutdown().map_err(|e| e.to_string())?;
        Ok(status.and_then(|s| s.code()) == Some(33))
    };
    match run() {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::FAILURE,
        Err(e) => {
            eprintln!("placed-boot: {e}");
            std::process::ExitCode::from(2)
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("placed-boot: the launcher boots on Linux with KVM only");
    std::process::exit(2);
}
