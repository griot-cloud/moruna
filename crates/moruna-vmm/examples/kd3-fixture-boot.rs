//! Explicit TEST-ONLY one-shot fixture boot; never the NativeRun server protocol.
#[cfg(feature = "kd3-conformance-proof")]
mod proof {
    use serde::Deserialize;
    use std::{
        fs,
        io::Write,
        os::unix::fs::OpenOptionsExt,
        path::PathBuf,
        time::{Duration, Instant},
    };
    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Config {
        descriptor: PathBuf,
        runtime_dir: PathBuf,
        qemu: PathBuf,
        backend: PathBuf,
        case: String,
        run: String,
        nonce: String,
        seconds: u64,
        result: PathBuf,
    }
    const MAX: usize = 8 * 1024 * 1024;
    struct Parts {
        total: Option<usize>,
        bytes: Vec<u8>,
        chunks: usize,
        ended: bool,
    }
    impl Parts {
        fn line(&mut self, line: &str, c: &Config) -> Result<()> {
            let Some((producer, frame)) = line.split_once(": ") else {
                return Ok(());
            };
            if !producer.starts_with("linux ") {
                return Ok(());
            }
            let words: Vec<_> = frame.split_whitespace().collect();
            if !matches!(words.first(), Some(&"KD3R") | Some(&"KD3E")) {
                return Ok(());
            }
            if words.len() != 7 && words.len() != 6 {
                return Err("malformed fixture frame".into());
            }
            if words[1..4] != [c.case.as_str(), c.run.as_str(), c.nonce.as_str()] {
                return Err("fixture frame identity differs".into());
            }
            let total: usize = words[4].parse()?;
            if total == 0
                || total > MAX
                || self.total.is_some_and(|prior| prior != total)
                || self.ended
            {
                return Err("invalid/conflicting fixture length/end".into());
            }
            self.total = Some(total);
            let index: usize = words[5].parse()?;
            if index != self.chunks {
                return Err("fixture frame gap/duplicate".into());
            }
            if words[0] == "KD3E" {
                if words.len() != 6 || self.bytes.len() != total {
                    return Err("incomplete fixture result".into());
                }
                self.ended = true;
            } else {
                if words.len() != 7
                    || words[6].is_empty()
                    || words[6].len() > 192
                    || words[6].len() % 2 != 0
                {
                    return Err("invalid fixture chunk".into());
                }
                for pair in words[6].as_bytes().chunks_exact(2) {
                    let pair = std::str::from_utf8(pair)?;
                    self.bytes.push(u8::from_str_radix(pair, 16)?);
                }
                if self.bytes.len() > total {
                    return Err("fixture bytes exceed declared total".into());
                }
                self.chunks += 1;
            }
            Ok(())
        }
    }
    // The console grows concurrently. A poll may end in the middle of a frame;
    // only a newline commits that frame to the strict receipt parser.
    fn complete_lines(serial: &str) -> impl Iterator<Item = &str> {
        serial
            .split_inclusive('\n')
            .filter_map(|line| line.strip_suffix('\n'))
    }

    fn successful_termination(line: &str) -> bool {
        let Some(status) = line
            .trim_end()
            .strip_prefix("KD2 engine: run 1 of 1: the engine exited with code ")
        else {
            return false;
        };
        status == "0"
            || status
                .strip_prefix("0; ")
                .is_some_and(|stats| !stats.is_empty())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        fn config() -> Config {
            Config {
                descriptor: "descriptor".into(),
                runtime_dir: "runtime".into(),
                qemu: "qemu".into(),
                backend: "backend".into(),
                case: "source.tensor".into(),
                run: "1".repeat(32),
                nonce: "2".repeat(32),
                seconds: 30,
                result: "result".into(),
            }
        }
        fn parts() -> Parts {
            Parts {
                total: None,
                bytes: vec![],
                chunks: 0,
                ended: false,
            }
        }
        fn frame(c: &Config, kind: &str, total: usize, index: usize, hex: &str) -> String {
            format!(
                "linux moruna-kd3-fixture: {kind} {} {} {} {total} {index} {hex}",
                c.case, c.run, c.nonce
            )
        }
        #[test]
        fn complete_console_frames_require_exact_identity_length_and_end() {
            let c = config();
            let mut p = parts();
            p.line(&frame(&c, "KD3R", 2, 0, "7b7d"), &c).unwrap();
            assert!(!p.ended);
            p.line(&frame(&c, "KD3E", 2, 1, ""), &c).unwrap();
            assert!(p.ended);
            assert_eq!(p.bytes, b"{}");
            assert!(p.line(&frame(&c, "KD3E", 2, 1, ""), &c).is_err());
        }
        #[test]
        fn partial_live_tail_waits_for_newline_and_is_counted_once_per_snapshot() {
            let c = config();
            let first = format!("{}\n", frame(&c, "KD3R", 2, 0, "7b"));
            let second = format!("{}\n", frame(&c, "KD3R", 2, 1, "7d"));
            let end = format!("{}\n", frame(&c, "KD3E", 2, 2, ""));
            // Production reparses each complete snapshot from scratch. Every
            // intermediate byte prefix must ignore its uncommitted final line.
            let serial = format!("{first}{second}{end}");
            for limit in 0..=serial.len() {
                let snapshot = &serial[..limit];
                let mut p = parts();
                for line in complete_lines(snapshot) {
                    p.line(line, &c).expect("incomplete tail is not a frame");
                }
                let expected = usize::from(limit >= first.len())
                    + usize::from(limit >= first.len() + second.len());
                assert_eq!(p.chunks, expected);
                assert_eq!(p.bytes.len(), expected);
                assert_eq!(p.ended, limit == serial.len());
            }
            let malformed = format!("{}\n", frame(&c, "KD3R", 2, 0, "7"));
            assert!(
                parts()
                    .line(complete_lines(&malformed).next().unwrap(), &c)
                    .is_err()
            );
        }

        #[test]
        fn termination_requires_exact_zero_token_and_allows_service_diagnostics() {
            let prefix = "KD2 engine: run 1 of 1: the engine ";
            for tail in [
                "exited with code 0",
                "exited with code 0\r\n",
                "exited with code 0; 638 calls served; refused: socket (41): 2\r\n",
            ] {
                assert!(successful_termination(&format!("{prefix}{tail}")), "{tail}");
            }
            for tail in [
                "exited with code 1",
                "exited with code 10",
                "exited with code 00",
                "exited with code 0garbage",
                "exited with code 0;",
                "ended by signal 0",
            ] {
                assert!(
                    !successful_termination(&format!("{prefix}{tail}")),
                    "{tail}"
                );
            }
            assert!(!successful_termination("exited with code 0"));
        }

        #[test]
        fn gaps_wrong_nonce_short_results_and_oversized_chunks_refuse() {
            let c = config();
            assert!(parts().line(&frame(&c, "KD3R", 2, 1, "7b7d"), &c).is_err());
            let mut wrong = config();
            wrong.nonce = "3".repeat(32);
            assert!(
                parts()
                    .line(&frame(&wrong, "KD3R", 2, 0, "7b7d"), &c)
                    .is_err()
            );
            assert!(parts().line(&frame(&c, "KD3E", 2, 0, ""), &c).is_err());
            assert!(
                parts()
                    .line(&frame(&c, "KD3R", 97, 0, &"ab".repeat(97)), &c)
                    .is_err()
            );
            assert!(
                parts()
                    .line(&frame(&c, "KD3R", MAX + 1, 0, "00"), &c)
                    .is_err()
            );
        }
    }

    pub fn run() -> Result<()> {
        let args: Vec<_> = std::env::args().skip(1).collect();
        if args.len() != 1 {
            return Err("usage: kd3-fixture-boot CONFIG.json".into());
        }
        let cfg: Config = serde_json::from_slice(&fs::read(&args[0])?)?;
        for id in [&cfg.run, &cfg.nonce] {
            if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("invalid explicit run/nonce".into());
            }
        }
        if cfg.case.is_empty()
            || cfg.case.len() > 96
            || !cfg
                .case
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || !(10..=600).contains(&cfg.seconds)
        {
            return Err("invalid fixture identity/deadline".into());
        }
        let artifacts = moruna_vmm::microkernel::inspect(&cfg.descriptor)?;
        let r = &artifacts.resources;
        // Launch admits a job budget, then derives physical VM RAM from the descriptor.
        let budget = moruna_vmm::session::Budget {
            memory_bytes: r.job_memory_bytes,
            memory_max_bytes: r.job_memory_bytes,
            cpus: r.cpus,
            cpus_max: r.cpus,
        };
        let mut node = moruna_vmm::microkernel::launch_tcg_proof(
            &cfg.descriptor,
            budget,
            &cfg.runtime_dir,
            &cfg.qemu,
            &cfg.backend,
        )?;
        let deadline = Instant::now() + Duration::from_secs(cfg.seconds);
        let outcome = (|| -> Result<serde_json::Value> {
            loop {
                let serial_path = cfg.runtime_dir.join("serial.log");
                if !serial_path.exists() {
                    if Instant::now() >= deadline || node.try_wait()?.is_some() {
                        return Err("fixture console unavailable before deadline/VM exit".into());
                    }
                    std::thread::sleep(Duration::from_millis(25));
                    continue;
                }
                if fs::metadata(&serial_path)?.len() > 64 * 1024 * 1024 {
                    return Err("serial exceeds proof bound".into());
                }
                let bytes = fs::read(serial_path)?;
                if bytes.len() > 64 * 1024 * 1024 {
                    return Err("serial exceeds proof bound".into());
                }
                let serial = std::str::from_utf8(&bytes)?;
                let mut parts = Parts {
                    total: None,
                    bytes: Vec::new(),
                    chunks: 0,
                    ended: false,
                };
                for line in complete_lines(serial) {
                    parts.line(line, &cfg)?;
                }
                let ended = complete_lines(serial)
                    .filter(|line| line.contains("KD2 engine: run 1 of 1: the engine "))
                    .collect::<Vec<_>>();
                if ended.len() > 1 {
                    return Err("conflicting actual engine termination".into());
                }
                if let Some(line) = ended.first() {
                    if !serial.contains(&format!("kk: {} CPUs online", r.cpus))
                        || serial.to_lowercase().contains("panic")
                        || serial.contains("FAIL:")
                    {
                        return Err(
                            "actual CPU-online proof missing or native boot failure observed"
                                .into(),
                        );
                    }
                    if !successful_termination(line) {
                        return Err(format!("actual fixture failed: {line}").into());
                    }
                    if !parts.ended {
                        return Err("engine exited without complete fixture receipt".into());
                    }
                    let result: serde_json::Value = serde_json::from_slice(&parts.bytes)?;
                    if result["run_id"].as_str() != Some(cfg.run.as_str())
                        || !result["receipts"].is_array()
                    {
                        return Err("actual fixture payload run/receipt envelope differs".into());
                    }
                    return Ok(result);
                }
                if node.try_wait()?.is_some() {
                    return Err("VM exited before actual fixture termination".into());
                }
                if Instant::now() >= deadline {
                    return Err("fixture boot deadline expired".into());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        })();
        let cleanup = node.shutdown();
        cleanup?;
        let result = outcome?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&cfg.result)?;
        serde_json::to_writer(
            &mut file,
            &serde_json::json!({"case":cfg.case,"run":cfg.run,"nonce":cfg.nonce,"acceleration":"TEST_ONLY_TCG","kvm":"NOT_RUN","cleanup":true,"result":result}),
        )?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(())
    }
}
fn main() {
    #[cfg(feature = "kd3-conformance-proof")]
    if let Err(error) = proof::run() {
        eprintln!("KD3 fixture boot: {error}");
        std::process::exit(1);
    }
    #[cfg(not(feature = "kd3-conformance-proof"))]
    {
        eprintln!("requires explicit kd3-conformance-proof feature");
        std::process::exit(2);
    }
}
