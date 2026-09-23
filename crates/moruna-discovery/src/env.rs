//! Environment reading, the host profile variable (e.1) and size strings (f.3).
//!
//! Every environment lookup in the crate goes through [`EnvSource`], so a test supplies a map
//! instead of mutating the process environment (which is `unsafe` in the 2024 edition and is not
//! safe to do while other tests run).

#[cfg(test)]
use std::collections::BTreeMap;
use std::path::PathBuf;

use moruna_kernel::{MorunaError, Guarantee, HostProfile, Result};

/// Where the crate reads environment variables from.
pub(crate) trait EnvSource {
    /// The value of `key`, or `None` when it is unset or not valid Unicode.
    fn get(&self, key: &str) -> Option<String>;
}

/// The real process environment.
pub(crate) struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// A fixed map, for tests that need a controlled environment without touching the process.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MapEnv(BTreeMap<String, String>);

#[cfg(test)]
impl MapEnv {
    /// An environment with the given pairs.
    pub(crate) fn with(pairs: &[(&str, &str)]) -> MapEnv {
        let mut map = BTreeMap::new();
        for (k, v) in pairs {
            map.insert((*k).to_string(), (*v).to_string());
        }
        MapEnv(map)
    }
}

#[cfg(test)]
impl EnvSource for MapEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
}

/// A `Config` error naming the setting that was wrong.
pub(crate) fn config(name: &'static str, msg: impl Into<String>) -> MorunaError {
    MorunaError::Config {
        name,
        msg: msg.into(),
    }
}

/// Parse `MORUNA_HOST_PROFILE` (e.1). A comma separated list of `key=value` pairs; an unknown key,
/// an unknown value, a repeated key and a relative `staging_dir` are each a `Config` error naming
/// the key. A key that is absent stays `Guarantee::Unknown`, which `discover` then probes.
pub fn parse_profile(s: &str) -> Result<HostProfile> {
    let mut profile = HostProfile::default();
    let mut seen: Vec<&str> = Vec::new();
    for raw in s.split(',') {
        let pair = raw.trim();
        if pair.is_empty() {
            continue;
        }
        let Some((key, value)) = pair.split_once('=') else {
            return Err(config(
                "host_profile",
                format!("expected key=value, found `{pair}`"),
            ));
        };
        let key = key.trim();
        let value = value.trim();
        if seen.contains(&key) {
            return Err(config("host_profile", format!("duplicate key `{key}`")));
        }
        seen.push(key);
        match key {
            "huge_pages" => profile.huge_pages = declared(key, value)?,
            "memlock" => profile.memlock = declared(key, value)?,
            "io_uring" => profile.io_uring = declared(key, value)?,
            "direct_io" => profile.direct_io_staging = declared(key, value)?,
            "gds" => profile.gds = declared(key, value)?,
            "rdma" => profile.rdma = declared(key, value)?,
            "durable_staging" => profile.durable_staging = declared(key, value)?,
            "staging_dir" => {
                let path = PathBuf::from(value);
                if !path.is_absolute() {
                    return Err(config(
                        "host_profile",
                        format!("key `staging_dir` needs an absolute path, found `{value}`"),
                    ));
                }
                profile.staging_dir = Some(path);
            }
            other => {
                return Err(config("host_profile", format!("unknown key `{other}`")));
            }
        }
    }
    Ok(profile)
}

/// `present` or `absent` as the platform declares it; anything else names the key.
fn declared(key: &str, value: &str) -> Result<Guarantee> {
    match value {
        "present" => Ok(Guarantee::Present),
        "absent" => Ok(Guarantee::Absent),
        other => Err(config(
            "host_profile",
            format!("key `{key}`: expected `present` or `absent`, found `{other}`"),
        )),
    }
}

/// Parse a size (f.3): an integer of bytes, or a number immediately followed by one of `KiB`,
/// `MiB`, `GiB`, `TiB` (binary) or `KB`, `MB`, `GB`, `TB` (decimal). Case is ignored; a space
/// between the number and the unit is an error, as is any other suffix.
pub(crate) fn parse_size(name: &'static str, s: &str) -> Result<u64> {
    let text = s.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    if number.is_empty() {
        return Err(config(name, format!("`{s}` does not start with a number")));
    }
    let Ok(value) = number.parse::<f64>() else {
        return Err(config(name, format!("`{number}` is not a number")));
    };
    if !value.is_finite() || value < 0.0 {
        return Err(config(name, format!("`{number}` is not a size")));
    }
    let multiplier: f64 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        "kb" => 1_000.0,
        "mb" => 1_000_000.0,
        "gb" => 1_000_000_000.0,
        "tb" => 1_000_000_000_000.0,
        other => {
            return Err(config(
                name,
                format!("unknown unit `{other}` (use KiB, MiB, GiB, TiB, KB, MB, GB or TB)"),
            ));
        }
    };
    let bytes = value * multiplier;
    if bytes > u64::MAX as f64 {
        return Err(config(name, format!("`{s}` does not fit in 64 bits")));
    }
    Ok(bytes as u64)
}

/// Parse a CPU quota (`MORUNA_CPU`): a positive, finite number of cores.
pub(crate) fn parse_cpu(s: &str) -> Result<f64> {
    let Ok(value) = s.trim().parse::<f64>() else {
        return Err(config("cpu", format!("`{s}` is not a number of cores")));
    };
    if !value.is_finite() || value <= 0.0 {
        return Err(config("cpu", format!("`{s}` is not a positive core count")));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DS-T8 profile_parse (e.1): valid strings, unknown key, bad value, duplicate key; every
    /// error names the key.
    #[test]
    fn ds_t8_profile_parse() {
        let profile = parse_profile(
            "huge_pages=present,memlock=present,io_uring=present,direct_io=present,gds=absent,staging_dir=/scratch,durable_staging=present",
        )
        .expect("the Griot Cloud pod example parses");
        assert_eq!(profile.huge_pages, Guarantee::Present);
        assert_eq!(profile.memlock, Guarantee::Present);
        assert_eq!(profile.io_uring, Guarantee::Present);
        assert_eq!(profile.direct_io_staging, Guarantee::Present);
        assert_eq!(profile.gds, Guarantee::Absent);
        assert_eq!(profile.durable_staging, Guarantee::Present);
        assert_eq!(profile.staging_dir, Some(PathBuf::from("/scratch")));
        // rdma was not declared, so it stays Unknown until discover probes it.
        assert_eq!(profile.rdma, Guarantee::Unknown);

        // The empty string declares nothing.
        assert_eq!(
            parse_profile("").expect("empty is valid").huge_pages,
            Guarantee::Unknown
        );
        // Whitespace around keys and values is tolerated.
        assert_eq!(
            parse_profile(" rdma = absent ")
                .expect("whitespace is tolerated")
                .rdma,
            Guarantee::Absent
        );

        for (bad, needle) in [
            ("hugepages=present", "hugepages"),
            ("memlock=maybe", "memlock"),
            ("gds=present,gds=absent", "gds"),
            ("staging_dir=scratch", "staging_dir"),
            ("io_uring", "io_uring"),
        ] {
            let err = parse_profile(bad).expect_err("must be rejected");
            match err {
                MorunaError::Config { name, msg } => {
                    assert_eq!(name, "host_profile");
                    assert!(msg.contains(needle), "{msg} should name {needle}");
                }
                other => panic!("expected Config, got {other:?}"),
            }
        }
    }

    /// DS-T10 size_strings (f.3): `8GiB`, `8GB`, `8589934592`, `8 GiB` (space: error), `8gib`
    /// (case: accept).
    #[test]
    fn ds_t10_size_strings() {
        assert_eq!(
            parse_size("budget", "8GiB").expect("8GiB"),
            8 * 1024 * 1024 * 1024
        );
        assert_eq!(parse_size("budget", "8GB").expect("8GB"), 8_000_000_000);
        assert_eq!(
            parse_size("budget", "8589934592").expect("bytes"),
            8_589_934_592
        );
        assert_eq!(
            parse_size("budget", "8gib").expect("case"),
            8 * 1024 * 1024 * 1024
        );
        assert_eq!(
            parse_size("budget", "512MiB").expect("MiB"),
            512 * 1024 * 1024
        );
        assert_eq!(
            parse_size("budget", "2TiB").expect("TiB"),
            2 * 1024u64.pow(4)
        );
        assert_eq!(parse_size("budget", "4KiB").expect("KiB"), 4096);
        assert_eq!(parse_size("budget", "1KB").expect("KB"), 1000);
        assert_eq!(parse_size("budget", "1MB").expect("MB"), 1_000_000);
        assert_eq!(parse_size("budget", "1TB").expect("TB"), 1_000_000_000_000);
        assert_eq!(
            parse_size("budget", "1.5GiB").expect("fraction"),
            1_610_612_736
        );

        for bad in ["8 GiB", "eight", "8GiBx", "", "8Gi", "-1"] {
            assert!(
                parse_size("budget", bad).is_err(),
                "`{bad}` must be rejected"
            );
        }
        assert!(parse_size("budget", "999999999999999999999999GiB").is_err());
    }

    /// `MORUNA_CPU` accepts a positive number of cores and nothing else.
    #[test]
    fn cpu_strings() {
        assert!((parse_cpu("1.5").expect("1.5") - 1.5).abs() < f64::EPSILON);
        assert!(parse_cpu("0").is_err());
        assert!(parse_cpu("-2").is_err());
        assert!(parse_cpu("many").is_err());
    }

    /// The map environment stands in for the process environment.
    #[test]
    fn map_env_reads_back() {
        let env = MapEnv::with(&[("MORUNA_BUDGET", "1GiB")]);
        assert_eq!(env.get("MORUNA_BUDGET").as_deref(), Some("1GiB"));
        assert_eq!(env.get("MORUNA_CPU"), None);
        assert_eq!(MapEnv::default().get("MORUNA_BUDGET"), None);
        // The process environment is readable and returns None for a name nothing sets.
        assert_eq!(ProcessEnv.get("MORUNA_DEFINITELY_NOT_SET_9d1f"), None);
    }
}
