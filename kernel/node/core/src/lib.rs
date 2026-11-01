//! Standalone node policy. Starting a process does not assert service readiness.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
use alloc::{string::String, vec::Vec};

pub const MAX_CONFIG: usize = 4096;
pub const MAX_SERVICES: usize = 16;
pub const MAX_JOB_GRANTS: usize = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub name: String,
    pub image: String,
    pub account_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub service: String,
    pub slot: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub left: String,
    pub left_slot: String,
    pub right: String,
    pub right_slot: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub services: Vec<Service>,
    pub grants: Vec<Grant>,
    pub links: Vec<Link>,
    pub bootstraps: Vec<(Bootstrap, String)>,
    pub admission: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    TooLarge,
    Encoding,
    Version,
    Syntax,
    Duplicate,
    Budget,
    Empty,
}

fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.' || b == b'_'
        })
}

impl Config {
    /// A deliberately bounded, versioned boot format. Every non-comment record is
    /// `service NAME IMAGE_HANDLE ACCOUNT_BYTES`; unknown records fail closed.
    pub fn parse(bytes: &[u8], available: u64) -> Result<Self, ConfigError> {
        if bytes.len() > MAX_CONFIG {
            return Err(ConfigError::TooLarge);
        }
        let text = core::str::from_utf8(bytes).map_err(|_| ConfigError::Encoding)?;
        let mut lines = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'));
        if lines.next() != Some("moruna-node 1") {
            return Err(ConfigError::Version);
        }
        let mut services: Vec<Service> = Vec::new();
        let mut total = 0u64;
        let mut grants = Vec::new();
        let mut links = Vec::new();
        let mut bootstraps: Vec<(Bootstrap, String)> = Vec::new();
        let mut admission: Option<String> = None;
        for line in lines {
            let words: Vec<_> = line.split_ascii_whitespace().collect();
            match words.as_slice() {
                ["bootstrap", "admission", service] if identifier(service) => {
                    if admission.replace((*service).into()).is_some() {
                        return Err(ConfigError::Duplicate);
                    }
                    continue;
                }
                ["bootstrap", kind, service] if identifier(service) => {
                    let kind = match *kind {
                        "devices" => Bootstrap::Devices,
                        "linux" => Bootstrap::Linux,
                        _ => return Err(ConfigError::Syntax),
                    };
                    if bootstraps.iter().any(|(k, _)| *k == kind) {
                        return Err(ConfigError::Duplicate);
                    }
                    bootstraps.push((kind, (*service).into()));
                    continue;
                }
                ["grant", service, slot, source]
                    if identifier(service) && identifier(slot) && identifier(source) =>
                {
                    grants.push(Grant {
                        service: (*service).into(),
                        slot: (*slot).into(),
                        source: (*source).into(),
                    });
                    continue;
                }
                ["link", left, left_slot, right, right_slot]
                    if [left, left_slot, right, right_slot]
                        .iter()
                        .all(|s| identifier(s)) =>
                {
                    links.push(Link {
                        left: (*left).into(),
                        left_slot: (*left_slot).into(),
                        right: (*right).into(),
                        right_slot: (*right_slot).into(),
                    });
                    continue;
                }
                _ => {}
            }
            let mut fields = line.split_ascii_whitespace();
            let (Some("service"), Some(name), Some(image), Some(bytes), None) = (
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
            ) else {
                return Err(ConfigError::Syntax);
            };
            if !identifier(name) || !identifier(image) || services.len() == MAX_SERVICES {
                return Err(ConfigError::Syntax);
            }
            if services.iter().any(|s| s.name == name) {
                return Err(ConfigError::Duplicate);
            }
            let account_bytes = bytes.parse::<u64>().map_err(|_| ConfigError::Budget)?;
            if account_bytes == 0 || account_bytes % 4096 != 0 {
                return Err(ConfigError::Budget);
            }
            total = total
                .checked_add(account_bytes)
                .ok_or(ConfigError::Budget)?;
            if total > available {
                return Err(ConfigError::Budget);
            }
            services.push(Service {
                name: name.into(),
                image: image.into(),
                account_bytes,
            });
        }
        if services.is_empty() {
            return Err(ConfigError::Empty);
        }
        // Every destination slot and transferred boot capability has one owner.
        let mut destinations: Vec<(&str, &str)> = Vec::new();
        let mut sources: Vec<&str> = Vec::new();
        if let Some(service) = &admission {
            destinations.push((service, "guardian"));
            destinations.push((service, "control"));
        }
        for (_, service) in &bootstraps {
            destinations.push((service, "supervisor"));
        }
        for grant in &grants {
            if sources.contains(&grant.source.as_str()) {
                return Err(ConfigError::Duplicate);
            }
            sources.push(&grant.source);
            destinations.push((&grant.service, &grant.slot));
        }
        for link in &links {
            destinations.push((&link.left, &link.left_slot));
            destinations.push((&link.right, &link.right_slot));
        }
        for (index, (service, slot)) in destinations.iter().enumerate() {
            if !services.iter().any(|s| s.name == *service)
                || *slot == "account"
                || *slot == "console"
            {
                return Err(ConfigError::Syntax);
            }
            if destinations[..index].contains(&(*service, *slot)) {
                return Err(ConfigError::Duplicate);
            }
            if destinations.iter().filter(|(s, _)| s == service).count() > 30 {
                return Err(ConfigError::Syntax);
            }
        }
        Ok(Self {
            services,
            grants,
            links,
            bootstraps,
            admission,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Pending,
    Running,
    Stopping,
    Terminated,
}

/// The supervisor retains process handles, never spare channel endpoints. A
/// critical service termination ends the node; dead channels are not restartable.
pub struct Lifecycle {
    states: Vec<State>,
    failed: bool,
}
impl Lifecycle {
    pub fn new(count: usize) -> Self {
        Self {
            states: alloc::vec![State::Pending; count],
            failed: false,
        }
    }
    pub fn started(&mut self, index: usize) -> bool {
        if self.failed || self.states.get(index) != Some(&State::Pending) {
            return false;
        }
        self.states[index] = State::Running;
        true
    }
    pub fn terminated(&mut self, index: usize) -> bool {
        let Some(state) = self.states.get_mut(index) else {
            return false;
        };
        if *state == State::Terminated {
            return false;
        }
        if *state != State::Stopping {
            self.failed = true;
        }
        *state = State::Terminated;
        true
    }
    pub fn stop(&mut self) {
        for state in &mut self.states {
            if *state == State::Running {
                *state = State::Stopping;
            }
        }
    }
    pub fn failed(&self) -> bool {
        self.failed
    }
    pub fn state(&self, index: usize) -> Option<State> {
        self.states.get(index).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn separate_instances_share_only_immutable_program_not_account_or_slots() {
        let config=Config::parse(b"moruna-node 1\nservice home image.storage 4096\nservice metadata image.storage 8192\n",16384).unwrap();
        assert_eq!(config.services.len(), 2);
        assert_eq!(config.services[0].account_bytes, 4096);
        assert_eq!(config.services[1].account_bytes, 8192);
        assert!(Config::parse(b"moruna-node 1\nservice home image.storage 4096\nservice home image.storage 8192\n",16384).is_err());
    }
    #[test]
    fn budget_checked_before_launch() {
        let c = b"moruna-node 1\nservice devices image.devices 4096\nservice net image.net 4096\n";
        assert_eq!(Config::parse(c, 4096), Err(ConfigError::Budget));
        assert_eq!(Config::parse(c, 8192).unwrap().services.len(), 2);
    }
    #[test]
    fn ambiguity_and_unversioned_input_refused() {
        assert_eq!(
            Config::parse(b"service a b 4096", 8192),
            Err(ConfigError::Version)
        );
        assert_eq!(
            Config::parse(b"moruna-node 1\nservice a b 4096\nservice a c 4096", 8192),
            Err(ConfigError::Duplicate)
        );
        assert_eq!(
            Config::parse(b"moruna-node 1\nservice a b 1", 8192),
            Err(ConfigError::Budget)
        );
    }
    #[test]
    fn unexpected_death_prevents_later_launch() {
        let mut l = Lifecycle::new(2);
        assert!(l.started(0));
        assert!(l.terminated(0));
        assert!(l.failed());
        assert!(!l.started(1));
    }
    #[test]
    fn commanded_shutdown_is_not_unexpected_death() {
        let mut l = Lifecycle::new(1);
        assert!(l.started(0));
        l.stop();
        assert!(l.terminated(0));
        assert!(!l.failed());
        assert!(!l.terminated(0));
    }
}

/// The sole node root-transfer header. This is a boot message, not a service
/// protocol handshake. No unversioned/truncated/trailing message is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bootstrap {
    Devices,
    Linux,
}
impl Bootstrap {
    pub const fn header(self) -> &'static [u8; 8] {
        match self {
            Self::Devices => b"MNBT\x01\x00\x01\x05",
            Self::Linux => b"MNBT\x01\x00\x02\x01",
        }
    }
    pub const fn handles(self) -> usize {
        match self {
            Self::Devices => 5,
            Self::Linux => 1,
        }
    }
    pub fn accepts(self, bytes: &[u8], handles: usize) -> bool {
        bytes == self.header() && handles == self.handles()
    }
}

#[cfg(test)]
mod boot_tests {
    use super::*;
    #[test]
    fn bootstrap_requires_exact_version_kind_bytes_and_handle_count() {
        assert!(Bootstrap::Devices.accepts(Bootstrap::Devices.header(), 5));
        assert!(!Bootstrap::Devices.accepts(b"roots", 5));
        assert!(!Bootstrap::Devices.accepts(Bootstrap::Linux.header(), 5));
        assert!(!Bootstrap::Linux.accepts(Bootstrap::Linux.header(), 0));
        let mut invalid = *Bootstrap::Linux.header();
        invalid[4] = 2;
        assert!(!Bootstrap::Linux.accepts(&invalid, 1));
    }
    #[test]
    fn bootstrap_and_grants_cannot_alias_child_slots() {
        let config = b"moruna-node 1\nservice devices image.devices 4096\nbootstrap devices devices\ngrant devices supervisor spare\n";
        assert_eq!(Config::parse(config, 4096), Err(ConfigError::Duplicate));
        let config = b"moruna-node 1\nservice a image.a 4096\nlink a data unknown data\n";
        assert_eq!(Config::parse(config, 4096), Err(ConfigError::Syntax));
    }
}

/// A supervisor escrow record for a child process. The grant arrives before the
/// child is started; a failed start retracts it. Actual termination is a signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildRecord {
    pub id: u64,
    pub retain: bool,
}
impl ChildRecord {
    pub fn encode(self) -> [u8; 16] {
        let mut bytes = [0; 16];
        bytes[..4].copy_from_slice(b"MNCH");
        bytes[4] = 1;
        bytes[5] = u8::from(self.retain);
        bytes[8..].copy_from_slice(&self.id.to_le_bytes());
        bytes
    }
    pub fn decode(bytes: &[u8], handles: usize) -> Option<Self> {
        if bytes.len() != 16
            || &bytes[..4] != b"MNCH"
            || bytes[4] != 1
            || bytes[5] > 1
            || bytes[6..8] != [0, 0]
            || handles != usize::from(bytes[5])
        {
            return None;
        }
        let id = u64::from_le_bytes(bytes[8..].try_into().ok()?);
        (id != 0).then_some(Self {
            id,
            retain: bytes[5] == 1,
        })
    }
}

/// Native process creators escrow real handles before launch. Queue pressure is
/// bounded by a deadline, rather than silently creating an unowned descendant.
#[cfg(target_os = "none")]
pub struct Guardian {
    channel: moruna_sys::Channel,
    next: u64,
}
#[cfg(target_os = "none")]
impl Guardian {
    pub fn new(channel: moruna_sys::Channel) -> Self {
        Self { channel, next: 0 }
    }
    fn send(
        &self,
        record: ChildRecord,
        mut handles: [Option<moruna_sys::Handle>; 1],
    ) -> moruna_sys::Result<()> {
        let bytes = record.encode();
        let deadline = moruna_sys::now().saturating_add(5_000_000_000);
        let handles = if record.retain {
            &mut handles[..]
        } else {
            &mut handles[..0]
        };
        loop {
            match self.channel.write_with(&bytes, handles) {
                Ok(()) => return Ok(()),
                Err(moruna_sys::Error::ShouldWait) => {
                    moruna_sys::wait_one(
                        &self.channel.0,
                        moruna_sys::Signals::WRITABLE | moruna_sys::Signals::PEER_CLOSED,
                        deadline,
                    )?;
                }
                Err(e) => return Err(e),
            }
        }
    }
    pub fn publish(&mut self, process: &moruna_sys::Process) -> moruna_sys::Result<u64> {
        self.next = self.next.checked_add(1).ok_or(moruna_sys::Error::Limit)?;
        let handle = process.0.duplicate(
            moruna_sys::Rights::WRITE | moruna_sys::Rights::WAIT | moruna_sys::Rights::TRANSFER,
        )?;
        self.send(
            ChildRecord {
                id: self.next,
                retain: true,
            },
            [Some(handle)],
        )?;
        Ok(self.next)
    }
    pub fn retract(&self, id: u64) -> moruna_sys::Result<()> {
        self.send(ChildRecord { id, retain: false }, [None])
    }
}

/// One explicitly boot-authorized initial engine admission, using the same launch
/// path as later channel admissions. It contains no ambient paths or credentials.
#[derive(Debug, PartialEq, Eq)]
pub struct InitialGrant {
    pub slot: String,
    pub source: String,
    pub listener: bool,
}
#[derive(Debug, PartialEq, Eq)]
pub struct InitialJob {
    pub budget: u64,
    pub grants: Vec<InitialGrant>,
}
impl InitialJob {
    pub fn parse(bytes: &[u8]) -> Result<Self, ConfigError> {
        if bytes.len() >= MAX_CONFIG {
            return Err(ConfigError::TooLarge);
        }
        let text = core::str::from_utf8(bytes).map_err(|_| ConfigError::Encoding)?;
        let mut lines = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'));
        if lines.next() != Some("moruna-node-job 1") {
            return Err(ConfigError::Version);
        }
        let mut budget = None;
        let mut grants: Vec<InitialGrant> = Vec::new();
        for line in lines {
            let words: Vec<_> = line.split_ascii_whitespace().collect();
            match words.as_slice() {
                ["budget", bytes] if budget.is_none() => {
                    let bytes: u64 = bytes.parse().map_err(|_| ConfigError::Budget)?;
                    if bytes == 0 || bytes % 4096 != 0 {
                        return Err(ConfigError::Budget);
                    }
                    budget = Some(bytes);
                }
                [kind, slot, source]
                    if matches!(*kind, "grant" | "service")
                        && identifier(slot)
                        && identifier(source) =>
                {
                    if grants.len() >= MAX_JOB_GRANTS
                        || grants
                            .iter()
                            .any(|g| g.slot == *slot || g.source == *source)
                    {
                        return Err(ConfigError::Duplicate);
                    }
                    if ["console", "account", "config"].contains(slot) {
                        return Err(ConfigError::Syntax);
                    }
                    grants.push(InitialGrant {
                        slot: (*slot).into(),
                        source: (*source).into(),
                        listener: *kind == "service",
                    });
                }
                _ => return Err(ConfigError::Syntax),
            }
        }
        Ok(Self {
            budget: budget.ok_or(ConfigError::Budget)?,
            grants,
        })
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    #[test]
    fn escrow_records_require_exact_version_and_handle_count() {
        let record = ChildRecord {
            id: 7,
            retain: true,
        };
        assert_eq!(ChildRecord::decode(&record.encode(), 1), Some(record));
        assert_eq!(ChildRecord::decode(&record.encode(), 0), None);
        let mut bytes = record.encode();
        bytes[4] = 2;
        assert_eq!(ChildRecord::decode(&bytes, 1), None);
        assert_eq!(
            ChildRecord::decode(
                &ChildRecord {
                    id: 0,
                    retain: false
                }
                .encode(),
                0
            ),
            None
        );
    }
    #[test]
    fn initial_job_has_explicit_budget_and_unique_capability_sources() {
        assert!(
            InitialJob::parse(b"moruna-node-job 1\nbudget 8192\ngrant linux initial.linux\n")
                .is_ok()
        );
        assert_eq!(
            InitialJob::parse(b"moruna-node-job 1\ngrant linux initial.linux\n"),
            Err(ConfigError::Budget)
        );
        assert_eq!(
            InitialJob::parse(b"moruna-node-job 1\nbudget 8192\ngrant config secret\n"),
            Err(ConfigError::Syntax)
        );
        assert_eq!(
            InitialJob::parse(
                b"moruna-node-job 1\nbudget 8192\ngrant linux original\ngrant network original\n"
            ),
            Err(ConfigError::Duplicate)
        );
    }
}

/// Refusal from the one captured boot-policy clock domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockError {
    /// The observed monotonic clock moved backwards/wrapped; the epoch stays denied.
    Backwards,
    /// Zero or a sentinel/infinite validity is not a finite boot grant.
    InvalidLifetime,
    /// The actual anchor plus finite expiry exceeds the clock domain.
    Overflow,
}
/// One boot-policy monotonic epoch, anchored once to the real native nanosecond clock.
/// It supplies no clock or wall time; callers pass actual `moruna_svc::now()` readings.
pub struct BootClock {
    loaded: u64,
    last: u64,
    failed: bool,
}
impl BootClock {
    /// Capture exactly one successful trusted boot-policy load; no renewal method exists.
    pub fn new(now_ns: u64) -> Self {
        Self {
            loaded: now_ns,
            last: 0,
            failed: false,
        }
    }
    /// Checked elapsed nanoseconds; a backward/wrapped observation poisons this epoch.
    pub fn elapsed(&mut self, now_ns: u64) -> Result<u64, ClockError> {
        if self.failed {
            return Err(ClockError::Backwards);
        }
        let Some(elapsed) = now_ns.checked_sub(self.loaded) else {
            self.failed = true;
            return Err(ClockError::Backwards);
        };
        if elapsed < self.last {
            self.failed = true;
            return Err(ClockError::Backwards);
        }
        self.last = elapsed;
        Ok(elapsed)
    }
    /// A finite boot-relative expiry must also fit the real absolute clock domain.
    pub fn expires_at(&self, elapsed_ns: u64) -> Result<u64, ClockError> {
        if self.failed {
            return Err(ClockError::Backwards);
        }
        if elapsed_ns == 0 || elapsed_ns == u64::MAX {
            return Err(ClockError::InvalidLifetime);
        }
        self.loaded
            .checked_add(elapsed_ns)
            .ok_or(ClockError::Overflow)
    }
}
#[cfg(test)]
mod boot_clock_tests {
    use super::*;
    #[test]
    fn boot_policy_epoch_does_not_renew_after_backwards_time() {
        let mut clock = BootClock::new(100);
        assert_eq!(clock.elapsed(100), Ok(0));
        assert_eq!(clock.elapsed(120), Ok(20));
        assert!(clock.elapsed(119).is_err());
        assert!(clock.elapsed(140).is_err());
        assert!(clock.expires_at(1000).is_err());
    }
    #[test]
    fn boot_policy_time_wrap_and_infinite_or_overflow_expiry_refuse() {
        let mut clock = BootClock::new(u64::MAX - 10);
        assert_eq!(clock.expires_at(5), Ok(u64::MAX - 5));
        assert!(clock.expires_at(11).is_err());
        assert!(clock.expires_at(u64::MAX).is_err());
        assert!(clock.expires_at(0).is_err());
        assert!(clock.elapsed(1).is_err());
        assert!(clock.elapsed(u64::MAX).is_err());
    }
}
