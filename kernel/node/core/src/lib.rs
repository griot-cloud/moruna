//! Standalone node policy. Starting a process does not assert service readiness.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
use alloc::{string::String, vec::Vec};

pub mod restart;
pub use restart::Restart;

pub const MAX_CONFIG: usize = 4096;
pub const MAX_SERVICES: usize = 16;
pub const MAX_JOB_GRANTS: usize = 24;

/// The boot capability the core makes only on Hyper-V (ABI 2.7; system design 1F): a
/// service granted it runs only on a machine where the supervisor holds it
/// ([`Config::on_machine`]), which is how `vmbus` is started on Azure and nowhere else.
pub const HYPERV: &str = "root.hyperv";

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

/// `link LEFT LEFT_SLOT RIGHT RIGHT_SLOT`: a fresh channel pair, one end to each. The left
/// service serves on its end and the right one is its client, so a service that is not on
/// a machine takes its clients with it ([`Config::on_machine`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The service that serves.
    pub left: String,
    pub left_slot: String,
    /// Its client.
    pub right: String,
    pub right_slot: String,
}

/// The services that run only on Hyper-V: each granted [`HYPERV`], and each client of one
/// of those, and so on (the right side of a link whose left side is one).
fn hyperv_only(grants: &[Grant], links: &[Link]) -> Vec<String> {
    let mut only: Vec<String> = grants
        .iter()
        .filter(|grant| grant.source == HYPERV)
        .map(|grant| grant.service.clone())
        .collect();
    // Every pass adds at least one client, or ends.
    loop {
        let clients: Vec<String> = links
            .iter()
            .filter(|link| only.contains(&link.left) && !only.contains(&link.right))
            .map(|link| link.right.clone())
            .collect();
        if clients.is_empty() {
            return only;
        }
        for client in clients {
            if !only.contains(&client) {
                only.push(client);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub services: Vec<Service>,
    pub grants: Vec<Grant>,
    pub links: Vec<Link>,
    pub bootstraps: Vec<(Bootstrap, String)>,
    pub admission: Option<String>,
    /// The one service the supervisor starts again when it dies (`restart`, item 2.2c).
    pub restart: Option<Restart>,
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

pub(crate) fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.' || b == b'_'
        })
}

impl Config {
    /// A deliberately bounded, versioned boot format. Every non-comment record is
    /// `service NAME IMAGE_HANDLE ACCOUNT_BYTES`, `grant SERVICE SLOT SOURCE`,
    /// `link SERVER SLOT CLIENT SLOT` (the left side serves, [`Link`]), or a `bootstrap`;
    /// unknown records fail closed.
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
        let mut restart: Option<Restart> = None;
        for line in lines {
            let words: Vec<_> = line.split_ascii_whitespace().collect();
            match words.as_slice() {
                ["restart", record @ ..] => {
                    if restart.replace(Restart::parse(record)?).is_some() {
                        return Err(ConfigError::Duplicate);
                    }
                    continue;
                }
                ["bootstrap", "admission", service] if identifier(service) => {
                    if admission.replace((*service).into()).is_some() {
                        return Err(ConfigError::Duplicate);
                    }
                    continue;
                }
                ["bootstrap", kind, service, flags @ ..] if identifier(service) => {
                    let kind = match (*kind, flags) {
                        ("devices", []) => Bootstrap::Devices {
                            until_stopped: false,
                        },
                        // System design 1C: a node's device manager lives as long as the
                        // node, not until its last client leaves.
                        ("devices", ["until-stopped"]) => Bootstrap::Devices {
                            until_stopped: true,
                        },
                        ("linux", []) => Bootstrap::Linux,
                        ("platform", []) => Bootstrap::Platform,
                        _ => return Err(ConfigError::Syntax),
                    };
                    if bootstraps.iter().any(|(k, _)| k.same_kind(kind)) {
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
            // The node's memory, as the platform bootstrap carries it (item 3.2).
            destinations.push((service, "supervisor"));
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
        if let Some(record) = &restart {
            // A service started again has only what a start makes afresh: no grant, link
            // or bootstrap of node.conf's could be given to it a second time.
            if destinations
                .iter()
                .any(|(service, _)| *service == record.service)
            {
                return Err(ConfigError::Syntax);
            }
            destinations.push((&record.service, &record.slot));
            destinations.push((&record.server, &record.granting));
            if bootstraps
                .iter()
                .any(|(kind, _)| *kind == Bootstrap::Platform)
            {
                destinations.push((&record.service, restart::PLATFORM_SLOT));
            }
        }
        // A service that runs only on Hyper-V (granted root.hyperv, or a client of one) is never one
        // the node cannot run without, nor the one restarted or the server it mints from (item 2.2c).
        let only = hyperv_only(&grants, &links);
        if admission
            .as_ref()
            .is_some_and(|service| only.contains(service))
            || bootstraps.iter().any(|(_, service)| only.contains(service))
            || restart.as_ref().is_some_and(|record| {
                only.contains(&record.service) || only.contains(&record.server)
            })
        {
            return Err(ConfigError::Syntax);
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
            restart,
        })
    }

    /// The node as this machine runs it. With `hyperv` (the supervisor holds [`HYPERV`]),
    /// all of it; without, every service granted [`HYPERV`] is left out, and so is each of
    /// its clients, and theirs: a service whose server is not on this machine is not on it
    /// either (`storvsc`, a client of `vmbus`). A server is never left out for losing a
    /// client: its end of the link simply has none (`devices`, which `storvsc` registers
    /// with). Each service left out goes with its grants and its links, and the boot
    /// capabilities it would have taken are not taken. No such service is the admission
    /// service or a bootstrap's (`parse` refuses it).
    pub fn on_machine(mut self, hyperv: bool) -> Config {
        if hyperv {
            return self;
        }
        let left_out = hyperv_only(&self.grants, &self.links);
        let kept = |name: &String| !left_out.contains(name);
        self.services.retain(|service| kept(&service.name));
        self.grants.retain(|grant| kept(&grant.service));
        self.links
            .retain(|link| kept(&link.left) && kept(&link.right));
        self
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
    /// The service at `index`, which has died, is to be started again: back to pending, and
    /// the node not failed. Only a running service is.
    pub fn restarting(&mut self, index: usize) -> bool {
        if self.failed || self.states.get(index) != Some(&State::Running) {
            return false;
        }
        self.states[index] = State::Pending;
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
    /// How many services the node has.
    pub fn count(&self) -> usize {
        self.states.len()
    }
}

/// The services limit (`architecture/moruna-os-system.md`, section 1B): every service has
/// started within this long of the supervisor's own start.
pub const START_LIMIT_NS: u64 = 5_000_000_000;

/// Why the node's services did not all start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartError<E> {
    /// Service `index` could not be started. No service after it was tried.
    Refused { index: usize, error: E },
    /// Service `index` had not started when the limit passed. It may be running, and its
    /// lifecycle says so, so that stopping the node ends it; no service after it was tried.
    Late { index: usize },
}

/// Starts the node's services in order, within the services limit.
///
/// `began_ns` is the clock at the supervisor's own start and `now_ns` reads that clock;
/// `start` starts service `index`. A service has started in time when the clock, read once
/// its start has returned, is at most [`START_LIMIT_NS`] past `began_ns`. The first that
/// has not ends the starting with [`StartError::Late`], as the first that `start` refuses
/// ends it with [`StartError::Refused`]. Starting a process does not assert its readiness.
pub fn start_services<E>(
    lifecycle: &mut Lifecycle,
    began_ns: u64,
    mut now_ns: impl FnMut() -> u64,
    mut start: impl FnMut(usize) -> Result<(), E>,
) -> Result<(), StartError<E>> {
    for index in 0..lifecycle.count() {
        start(index).map_err(|error| StartError::Refused { index, error })?;
        lifecycle.started(index);
        if now_ns().saturating_sub(began_ns) > START_LIMIT_NS {
            return Err(StartError::Late { index });
        }
    }
    Ok(())
}

/// The supervisor's failure lines on the node's console
/// (`architecture/moruna-os-system.md`, section 1B). Each is one line.
pub mod line {
    use core::fmt::{self, Display};

    use moruna_abi::boot_line::{Fatal, SERVICES};

    /// `moruna: fatal services: <reason>`: the supervisor cannot go on.
    pub fn fatal<R: Display>(reason: R) -> Fatal<&'static str, R> {
        Fatal {
            stage: SERVICES,
            reason,
        }
    }

    /// `moruna: fatal services: <name> did not start within 5 s`: the services limit
    /// ([`START_LIMIT_NS`](super::START_LIMIT_NS)) passed before the service had started.
    pub struct Late<'a>(pub &'a str);

    impl Display for Late<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            fatal(format_args!("{} did not start within 5 s", self.0)).fmt(f)
        }
    }

    /// A service has gone, which stops the node: its own name is the stage (1B: "the name
    /// of the service that can't go on"). `moruna: fatal <name>: ended`, or `ended with code
    /// <n>` when the supervisor knows the code; `moruna: fatal <name>: closed its channel to
    /// the supervisor` when it did so with its process still running.
    pub struct Gone<'a>(pub &'a str, pub super::Gone);

    impl Display for Gone<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self.1 {
                super::Gone::Ended(None) => Fatal {
                    stage: self.0,
                    reason: "ended",
                }
                .fmt(f),
                super::Gone::Ended(Some(code)) => Fatal {
                    stage: self.0,
                    reason: format_args!("ended with code {code}"),
                }
                .fmt(f),
                super::Gone::LeftSupervision => Fatal {
                    stage: self.0,
                    reason: "closed its channel to the supervisor",
                }
                .fmt(f),
            }
        }
    }
}

/// How one of the node's services went: each stops the node (1B).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gone {
    /// Its process ended, with its code when the supervisor knows it.
    Ended(Option<i64>),
    /// It closed its channel to the supervisor (a bootstrap's, or admission's guardian)
    /// while its process still runs: the supervisor can no longer watch over it.
    LeftSupervision,
}

impl Gone {
    /// What a service's closed channel to the supervisor means. A service that ends closes
    /// its channels a moment before its process is seen to have ended, so the supervisor
    /// looks for its process's end first: `terminated` says whether it saw it. Either way
    /// the line names the service, never the channel.
    pub fn closed_channel(terminated: bool) -> Gone {
        if terminated {
            Gone::Ended(None)
        } else {
            Gone::LeftSupervision
        }
    }
}

impl Config {
    /// The services that hold a channel to the supervisor it watches, in the order the
    /// supervisor makes those channels: each bootstrap's but `platform`'s, which makes no
    /// process, then the admission service's guardian. When one closes, the service at its
    /// place has gone.
    pub fn guardians(&self) -> Vec<&str> {
        let bootstraps = self
            .bootstraps
            .iter()
            .filter(|(kind, _)| *kind != Bootstrap::Platform)
            .map(|(_, service)| service.as_str());
        bootstraps.chain(self.admission.as_deref()).collect()
    }
}

#[cfg(test)]
mod start_tests {
    use alloc::{format, vec::Vec};

    use super::*;

    const SECOND: u64 = 1_000_000_000;

    /// Starts `count` services whose starts return at `clock`'s readings, in order, and
    /// answers the result, the services whose start was tried, and the lifecycle.
    fn run(
        count: usize,
        began: u64,
        clock: &[u64],
        refuse: Option<usize>,
    ) -> (Result<(), StartError<&'static str>>, Vec<usize>, Lifecycle) {
        let mut lifecycle = Lifecycle::new(count);
        let mut tried = Vec::new();
        let mut readings = clock.iter().copied();
        let result = start_services(
            &mut lifecycle,
            began,
            || readings.next().expect("one reading for each start"),
            |index| {
                tried.push(index);
                if refuse == Some(index) {
                    Err("no such program")
                } else {
                    Ok(())
                }
            },
        );
        (result, tried, lifecycle)
    }

    #[test]
    fn the_limit_is_the_five_seconds_its_line_says() {
        assert_eq!(START_LIMIT_NS, 5 * SECOND);
        assert!(format!("{}", line::Late("x")).ends_with("within 5 s"));
    }

    #[test]
    fn services_that_start_within_the_limit_are_all_started() {
        let began = 7 * SECOND;
        let (result, tried, lifecycle) = run(
            3,
            began,
            &[began + 1, began + SECOND, began + 4 * SECOND],
            None,
        );
        assert_eq!(result, Ok(()));
        assert_eq!(tried, [0, 1, 2]);
        for index in 0..3 {
            assert_eq!(lifecycle.state(index), Some(State::Running));
        }
    }

    #[test]
    fn a_start_at_exactly_the_limit_is_in_time_and_one_past_it_is_late() {
        let began = 100;
        let (result, _, _) = run(1, began, &[began + START_LIMIT_NS], None);
        assert_eq!(result, Ok(()));
        let (result, _, lifecycle) = run(1, began, &[began + START_LIMIT_NS + 1], None);
        assert_eq!(result, Err(StartError::Late { index: 0 }));
        // It did start, late: stopping the node must end it.
        assert_eq!(lifecycle.state(0), Some(State::Running));
    }

    #[test]
    fn the_first_late_service_is_named_and_none_after_it_is_started() {
        let began = 3 * SECOND;
        let (result, tried, lifecycle) = run(
            4,
            began,
            &[began + SECOND, began + 6 * SECOND, began + 7 * SECOND],
            None,
        );
        assert_eq!(result, Err(StartError::Late { index: 1 }));
        assert_eq!(tried, [0, 1]);
        assert_eq!(lifecycle.state(0), Some(State::Running));
        assert_eq!(lifecycle.state(1), Some(State::Running));
        assert_eq!(lifecycle.state(2), Some(State::Pending));
        assert_eq!(lifecycle.state(3), Some(State::Pending));
    }

    #[test]
    fn a_supervisor_slow_before_its_first_start_names_the_first_service() {
        // Its own start counts: the limit is from the supervisor's start, not the first
        // service's.
        let (result, tried, _) = run(2, 0, &[9 * SECOND], None);
        assert_eq!(result, Err(StartError::Late { index: 0 }));
        assert_eq!(tried, [0]);
    }

    #[test]
    fn a_refused_start_ends_the_starting_and_is_not_marked_started() {
        let (result, tried, lifecycle) = run(3, 0, &[1], Some(1));
        assert_eq!(
            result,
            Err(StartError::Refused {
                index: 1,
                error: "no such program"
            })
        );
        assert_eq!(tried, [0, 1]);
        assert_eq!(lifecycle.state(0), Some(State::Running));
        assert_eq!(lifecycle.state(1), Some(State::Pending));
        assert_eq!(lifecycle.state(2), Some(State::Pending));
    }

    #[test]
    fn a_clock_read_before_the_supervisors_start_is_in_time() {
        let (result, _, _) = run(1, 10 * SECOND, &[SECOND], None);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn a_node_with_no_services_has_started_them_all() {
        let (result, tried, lifecycle) = run(0, 0, &[], None);
        assert_eq!(result, Ok(()));
        assert!(tried.is_empty());
        assert_eq!(lifecycle.count(), 0);
    }

    #[test]
    fn the_supervisors_failure_lines_are_exactly_the_system_designs() {
        assert_eq!(
            format!("{}", line::Gone("devices", Gone::Ended(Some(0)))),
            "moruna: fatal devices: ended with code 0"
        );
        assert_eq!(
            format!("{}", line::Gone("devices", Gone::Ended(None))),
            "moruna: fatal devices: ended"
        );
        assert_eq!(
            format!("{}", line::Gone("vmbus", Gone::Ended(Some(-3)))),
            "moruna: fatal vmbus: ended with code -3"
        );
        assert_eq!(
            format!("{}", line::Gone("admission", Gone::LeftSupervision)),
            "moruna: fatal admission: closed its channel to the supervisor"
        );
        assert_eq!(
            format!("{}", line::Late("netstack")),
            "moruna: fatal services: netstack did not start within 5 s"
        );
        assert_eq!(
            format!("{}", line::fatal("missing boot capability image.api")),
            "moruna: fatal services: missing boot capability image.api"
        );
        assert_eq!(
            format!(
                "{}",
                line::fatal(format_args!("config: {:?}", ConfigError::Version))
            ),
            "moruna: fatal services: config: Version"
        );
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

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
    // A link's left side serves and its right side is the client: `vmbus` serves `storvsc`,
    // which serves `probe`; `devices` serves `storvsc` (its registrations) and `other`; and
    // `other` serves `vmbus`.
    const CLOUD: &[u8] = b"moruna-node 1\nservice devices image.devices 4096\nservice vmbus image.vmbus 4096\nservice storvsc image.storvsc 4096\nservice probe image.probe 4096\nservice other image.other 4096\nbootstrap devices devices\ngrant devices drivers drivers\ngrant vmbus root.hyperv root.hyperv\ngrant vmbus dma dma.vmbus\ngrant storvsc dma dma.storvsc\nlink vmbus scsi storvsc vmbus\nlink devices register-storvsc storvsc devices\nlink storvsc check probe storvsc\nlink devices directory other directory\nlink other feed vmbus other\n";

    #[test]
    fn a_service_granted_root_hyperv_runs_only_where_the_core_made_it() {
        let config = Config::parse(CLOUD, 32768).unwrap();
        // On Hyper-V the node is as written.
        assert_eq!(config.clone().on_machine(true), config);
        // Elsewhere `vmbus` is left out, and so are its clients, and theirs: `storvsc`,
        // then `probe`. A server stays though it loses a client: `devices` loses `storvsc`,
        // and `other` loses `vmbus`.
        let elsewhere = config.on_machine(false);
        let names: Vec<_> = elsewhere.services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["devices", "other"]);
        assert_eq!(elsewhere.grants.len(), 1);
        assert_eq!(elsewhere.grants[0].source, "drivers");
        assert_eq!(elsewhere.links.len(), 1);
        assert_eq!(
            (
                elsewhere.links[0].left.as_str(),
                elsewhere.links[0].right.as_str()
            ),
            ("devices", "other")
        );
        assert_eq!(elsewhere.bootstraps.len(), 1);
        // The admission service stays when what it serves goes.
        let admission = b"moruna-node 1\nservice gate image.admission 4096\nservice vmbus image.vmbus 4096\nbootstrap admission gate\ngrant vmbus root.hyperv root.hyperv\nlink gate vmbus vmbus gate\n";
        let gate = Config::parse(admission, 8192).unwrap().on_machine(false);
        let names: Vec<_> = gate.services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["gate"]);
        assert!(gate.links.is_empty());
        assert_eq!(gate.admission.as_deref(), Some("gate"));
        // A node with no Hyper-V service is the same on every machine.
        let plain = Config::parse(b"moruna-node 1\nservice a b 4096\n", 8192).unwrap();
        assert_eq!(plain.clone().on_machine(false), plain);
        assert_eq!(HYPERV, "root.hyperv");
    }

    #[test]
    fn the_cloud_profile_starts_the_vmbus_processes_on_hyper_v_only() {
        let profiles: [&[u8]; 2] = [
            include_bytes!("../../cloud/node.conf"),
            include_bytes!("../../cloud-check/node.conf"),
        ];
        for text in profiles {
            let config = Config::parse(text, 1 << 30).unwrap();
            let names =
                |c: &Config| -> Vec<String> { c.services.iter().map(|s| s.name.clone()).collect() };
            let all = names(&config);
            // vmbus, and its clients storvsc (item 1.10b) and netvsc (item 1.11b).
            let hyperv = ["vmbus", "storvsc", "netvsc"];
            assert!(hyperv.iter().all(|h| all.iter().any(|n| n == h)));
            assert_eq!(config.clone().on_machine(true), config);
            let elsewhere = config.on_machine(false);
            let left: Vec<String> = all
                .iter()
                .filter(|n| !hyperv.contains(&n.as_str()))
                .cloned()
                .collect();
            assert_eq!(
                names(&elsewhere),
                left,
                "devices, its clients and the rest stay"
            );
            assert!(elsewhere.links.iter().all(|l| {
                !hyperv.contains(&l.left.as_str()) && !hyperv.contains(&l.right.as_str())
            }));
            assert!(
                elsewhere
                    .grants
                    .iter()
                    .all(|g| g.source != "dma.storvsc" && g.source != "dma.netvsc")
            );
        }
    }

    #[test]
    fn a_service_the_node_cannot_run_without_is_never_hyper_v_only() {
        let bootstrap = b"moruna-node 1\nservice devices image.devices 4096\nbootstrap devices devices\ngrant devices root.hyperv root.hyperv\n";
        assert_eq!(Config::parse(bootstrap, 8192), Err(ConfigError::Syntax));
        let admission = b"moruna-node 1\nservice gate image.admission 4096\nbootstrap admission gate\ngrant gate root.hyperv root.hyperv\n";
        assert_eq!(Config::parse(admission, 8192), Err(ConfigError::Syntax));
        // Nor a client of a service that is, however far down.
        let client = b"moruna-node 1\nservice vmbus image.vmbus 4096\nservice disk image.disk 4096\nservice devices image.devices 4096\nbootstrap devices devices\ngrant vmbus root.hyperv root.hyperv\nlink vmbus scsi disk vmbus\nlink disk block devices disk\n";
        assert_eq!(Config::parse(client, 16384), Err(ConfigError::Syntax));
        let gate = b"moruna-node 1\nservice vmbus image.vmbus 4096\nservice gate image.admission 4096\nbootstrap admission gate\ngrant vmbus root.hyperv root.hyperv\nlink vmbus feed gate vmbus\n";
        assert_eq!(Config::parse(gate, 8192), Err(ConfigError::Syntax));
        // root.hyperv has one owner, as every boot capability does.
        let twice = b"moruna-node 1\nservice a b 4096\nservice c d 4096\ngrant a root.hyperv root.hyperv\ngrant c root.hyperv root.hyperv\n";
        assert_eq!(Config::parse(twice, 8192), Err(ConfigError::Duplicate));
    }

    #[test]
    fn a_closed_supervisor_channel_names_the_service_that_held_it() {
        // Devices' bootstrap channel is a guardian, platform's is not, and a service with no
        // bootstrap has none. An admission service's guardian comes after every bootstrap's.
        let node = concat!(
            "moruna-node 1\n",
            "service devices image.devices 4096\n",
            "service platform image.platform 4096\n",
            "service gate image.admission 4096\n",
            "service linux image.linux 4096\n",
            "service vmbus image.vmbus 4096\n",
            "bootstrap platform platform\n",
            "bootstrap devices devices until-stopped\n",
            "bootstrap admission gate\n",
            "bootstrap linux linux\n",
        );
        let config = Config::parse(node.as_bytes(), 32768).unwrap();
        assert_eq!(config.guardians(), ["devices", "linux", "gate"]);
        let plain = Config::parse(b"moruna-node 1\nservice a b 4096\n", 8192).unwrap();
        assert!(plain.guardians().is_empty());
        // Whether its process has been seen to end decides the line; it is never the
        // channel's error.
        assert_eq!(Gone::closed_channel(true), Gone::Ended(None));
        assert_eq!(Gone::closed_channel(false), Gone::LeftSupervision);
        for gone in [Gone::closed_channel(true), Gone::closed_channel(false)] {
            let text = format!("{}", line::Gone("devices", gone));
            assert!(text.starts_with("moruna: fatal devices: "), "{text}");
            assert!(!text.contains("guardian") && !text.contains("peer closed"));
        }
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
///
/// Its eight bytes: `MNBT`, the version (1), the flags, the kind (1 devices, 2 Linux,
/// 3 platform) and the handles it carries. The one flag is the device manager's: bit 0,
/// it serves until it is stopped (node.conf's `bootstrap devices NAME until-stopped`,
/// system design 1C). Without it, it ends once its last client has gone, as CI's boot
/// has it.
///
/// `Platform` transfers no root: its message is the header and the node's memory
/// ([`Bootstrap::platform_message`]), the machine account's limit as the supervisor reads
/// it (system design 1D; item 1.4, PM 2026-11-18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bootstrap {
    Devices { until_stopped: bool },
    Linux,
    Platform,
}
impl Bootstrap {
    pub const fn header(self) -> &'static [u8; 8] {
        match self {
            Self::Devices {
                until_stopped: false,
            } => b"MNBT\x01\x00\x01\x05",
            Self::Devices {
                until_stopped: true,
            } => b"MNBT\x01\x01\x01\x05",
            Self::Linux => b"MNBT\x01\x00\x02\x01",
            Self::Platform => b"MNBT\x01\x00\x03\x00",
        }
    }
    pub const fn handles(self) -> usize {
        match self {
            Self::Devices { .. } => 5,
            Self::Linux => 1,
            Self::Platform => 0,
        }
    }
    pub fn accepts(self, bytes: &[u8], handles: usize) -> bool {
        match self {
            Self::Platform => Self::platform_memory(bytes, handles).is_some(),
            _ => bytes == self.header() && handles == self.handles(),
        }
    }
    /// The platform bootstrap's message: its header, then the node's memory in bytes,
    /// little-endian.
    pub fn platform_message(memory_bytes: u64) -> [u8; 16] {
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(Self::Platform.header());
        bytes[8..].copy_from_slice(&memory_bytes.to_le_bytes());
        bytes
    }
    /// The node's memory a platform bootstrap message carries: exactly the header and
    /// eight bytes, no handle, and more than zero.
    pub fn platform_memory(bytes: &[u8], handles: usize) -> Option<u64> {
        let (header, memory) = bytes.split_first_chunk::<8>()?;
        if header != Self::Platform.header() || handles != 0 {
            return None;
        }
        let memory = u64::from_le_bytes(memory.try_into().ok()?);
        (memory != 0).then_some(memory)
    }
    /// The bootstrap a message is, when it is exactly one.
    pub fn decode(bytes: &[u8], handles: usize) -> Option<Bootstrap> {
        [
            Self::Devices {
                until_stopped: false,
            },
            Self::Devices {
                until_stopped: true,
            },
            Self::Linux,
            Self::Platform,
        ]
        .into_iter()
        .find(|b| b.accepts(bytes, handles))
    }
    /// Whether two bootstraps are of one kind, whatever their flags: a node has at
    /// most one of each kind.
    pub fn same_kind(self, other: Bootstrap) -> bool {
        core::mem::discriminant(&self) == core::mem::discriminant(&other)
    }
}

#[cfg(test)]
mod boot_tests {
    use super::*;
    const DEVICES: Bootstrap = Bootstrap::Devices {
        until_stopped: false,
    };
    const UNTIL_STOPPED: Bootstrap = Bootstrap::Devices {
        until_stopped: true,
    };
    #[test]
    fn bootstrap_requires_exact_version_kind_bytes_and_handle_count() {
        assert!(DEVICES.accepts(DEVICES.header(), 5));
        assert!(!DEVICES.accepts(b"roots", 5));
        assert!(!DEVICES.accepts(Bootstrap::Linux.header(), 5));
        assert!(!Bootstrap::Linux.accepts(Bootstrap::Linux.header(), 0));
        let mut invalid = *Bootstrap::Linux.header();
        invalid[4] = 2;
        assert!(!Bootstrap::Linux.accepts(&invalid, 1));
    }
    #[test]
    fn the_device_manager_s_flag_is_its_own_header() {
        // CI's boot sends the header it always has; a node's supervisor may set the flag.
        assert_eq!(DEVICES.header(), b"MNBT\x01\x00\x01\x05");
        assert_eq!(UNTIL_STOPPED.header(), b"MNBT\x01\x01\x01\x05");
        assert_eq!(UNTIL_STOPPED.handles(), 5);
        assert!(UNTIL_STOPPED.accepts(UNTIL_STOPPED.header(), 5));
        assert!(!UNTIL_STOPPED.accepts(DEVICES.header(), 5));
        assert!(!DEVICES.accepts(UNTIL_STOPPED.header(), 5));
        for kind in [DEVICES, UNTIL_STOPPED, Bootstrap::Linux] {
            assert_eq!(Bootstrap::decode(kind.header(), kind.handles()), Some(kind));
            assert_eq!(Bootstrap::decode(kind.header(), kind.handles() + 1), None);
        }
        // No flag is defined for Linux, and no other flag for the device manager.
        let mut linux = *Bootstrap::Linux.header();
        linux[5] = 1;
        assert_eq!(Bootstrap::decode(&linux, 1), None);
        let mut other = *DEVICES.header();
        other[5] = 2;
        assert_eq!(Bootstrap::decode(&other, 5), None);
        assert_eq!(Bootstrap::decode(&UNTIL_STOPPED.header()[..7], 5), None);
        assert!(DEVICES.same_kind(UNTIL_STOPPED));
        assert!(!DEVICES.same_kind(Bootstrap::Linux));
        let platform = Bootstrap::platform_message(1 << 30);
        assert_eq!(Bootstrap::decode(&platform, 0), Some(Bootstrap::Platform));
        assert!(!DEVICES.same_kind(Bootstrap::Platform));
    }
    #[test]
    fn until_stopped_is_a_form_of_the_devices_bootstrap_alone() {
        let node = |bootstraps: &str| {
            Config::parse(
                alloc::format!(
                    "moruna-node 1\nservice devices image.devices 4096\nservice linux image.linux 4096\n{bootstraps}"
                )
                .as_bytes(),
                1 << 20,
            )
        };
        let config =
            node("bootstrap devices devices until-stopped\nbootstrap linux linux\n").unwrap();
        assert_eq!(
            config.bootstraps,
            [
                (UNTIL_STOPPED, String::from("devices")),
                (Bootstrap::Linux, String::from("linux"))
            ]
        );
        let config = node("bootstrap devices devices\n").unwrap();
        assert_eq!(config.bootstraps, [(DEVICES, String::from("devices"))]);
        for refused in [
            "bootstrap linux linux until-stopped\n",
            "bootstrap devices devices until-started\n",
            "bootstrap devices devices until-stopped until-stopped\n",
            "bootstrap admission devices until-stopped\n",
        ] {
            assert_eq!(node(refused), Err(ConfigError::Syntax), "{refused:?}");
        }
        // One device manager, with the flag or without it.
        for twice in [
            "bootstrap devices devices\nbootstrap devices linux until-stopped\n",
            "bootstrap devices devices until-stopped\nbootstrap devices linux\n",
        ] {
            assert_eq!(node(twice), Err(ConfigError::Duplicate), "{twice:?}");
        }
    }
    #[test]
    fn the_platform_bootstrap_carries_the_node_s_memory_and_no_handle() {
        let memory = 24_526_286_848;
        let message = Bootstrap::platform_message(memory);
        assert_eq!(&message[..8], b"MNBT\x01\x00\x03\x00");
        assert_eq!(Bootstrap::platform_memory(&message, 0), Some(memory));
        assert!(Bootstrap::Platform.accepts(&message, 0));
        assert_eq!(Bootstrap::Platform.handles(), 0);
        // A handle, a header alone, a byte short or over, another kind's header, a later
        // version, and no memory at all are each refused.
        assert_eq!(Bootstrap::platform_memory(&message, 1), None);
        assert!(!Bootstrap::Platform.accepts(Bootstrap::Platform.header(), 0));
        assert_eq!(Bootstrap::platform_memory(&message[..15], 0), None);
        let mut longer = message.to_vec();
        longer.push(0);
        assert_eq!(Bootstrap::platform_memory(&longer, 0), None);
        let mut other = message;
        other[..8].copy_from_slice(Bootstrap::Linux.header());
        assert_eq!(Bootstrap::platform_memory(&other, 0), None);
        let mut later = message;
        later[4] = 2;
        assert_eq!(Bootstrap::platform_memory(&later, 0), None);
        assert_eq!(
            Bootstrap::platform_memory(&Bootstrap::platform_message(0), 0),
            None
        );
        // The other kinds do not take it.
        assert!(!Bootstrap::Linux.accepts(&message, 0));
        assert!(!DEVICES.accepts(&message, 0));
    }
    #[test]
    fn a_node_config_bootstraps_platform_once() {
        let config = b"moruna-node 1\nservice platform image.platform 16777216\nbootstrap platform platform\n";
        let node = Config::parse(config, 1 << 30).unwrap();
        assert_eq!(node.bootstraps, [(Bootstrap::Platform, "platform".into())]);
        let twice = b"moruna-node 1\nservice platform image.platform 16777216\nbootstrap platform platform\nbootstrap platform platform\n";
        assert_eq!(Config::parse(twice, 1 << 30), Err(ConfigError::Duplicate));
        // Its slot is `supervisor`, which no grant or link may also fill.
        let aliased = b"moruna-node 1\nservice platform image.platform 16777216\nbootstrap platform platform\ngrant platform supervisor spare\n";
        assert_eq!(Config::parse(aliased, 1 << 30), Err(ConfigError::Duplicate));
    }
    #[test]
    fn admission_is_told_the_nodes_memory_on_a_slot_of_its_own() {
        let config = b"moruna-node 1\nservice admission image.admission 16777216\nbootstrap admission admission\n";
        assert!(Config::parse(config, 1 << 30).is_ok());
        // `supervisor` is admission's: no grant, link or bootstrap may also fill it.
        let granted = b"moruna-node 1\nservice admission image.admission 16777216\nbootstrap admission admission\ngrant admission supervisor spare\n";
        assert_eq!(Config::parse(granted, 1 << 30), Err(ConfigError::Duplicate));
        let bootstrapped = b"moruna-node 1\nservice admission image.admission 16777216\nbootstrap admission admission\nbootstrap platform admission\n";
        assert_eq!(
            Config::parse(bootstrapped, 1 << 30),
            Err(ConfigError::Duplicate)
        );
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
                    if bytes == 0 || !bytes.is_multiple_of(4096) {
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
                    // Version 1 originally transferred these critical endpoints.
                    // Retain node ownership when reading those older records.
                    grants.push(InitialGrant {
                        slot: (*slot).into(),
                        source: (*source).into(),
                        listener: *kind == "service"
                            || matches!(
                                *slot,
                                "linux" | "network" | "storage.admission" | "local.admission"
                            ),
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
    use alloc::format;
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
    fn initial_services_retain_ownership_in_legacy_and_explicit_records() {
        for slot in ["linux", "network", "storage.admission", "local.admission"] {
            let legacy = InitialJob::parse(
                format!("moruna-node-job 1\nbudget 8192\ngrant {slot} initial.endpoint\n")
                    .as_bytes(),
            )
            .unwrap();
            let explicit = InitialJob::parse(
                format!("moruna-node-job 1\nbudget 8192\nservice {slot} initial.endpoint\n")
                    .as_bytes(),
            )
            .unwrap();
            assert_eq!(legacy, explicit);
            assert!(legacy.grants[0].listener);
        }
        let data = InitialJob::parse(
            b"moruna-node-job 1\nbudget 8192\ngrant engine.bindings initial.bindings\n",
        )
        .unwrap();
        assert!(!data.grants[0].listener);
    }
    /// Stream D: the data disks' hand-over, as configure.py renders it. The device manager's
    /// `control.engine` listener is linked to admission's `initial.handover`, which the initial
    /// job grants to the engine's supervisor as `handover`, a plain grant, not a retained
    /// service.
    #[test]
    fn the_data_disks_handover_is_linked_to_the_engine_alone() {
        let node = Config::parse(
            b"moruna-node 1\nservice devices image.devices 4096\nservice admission image.admission 8192\n\
              bootstrap devices devices\nbootstrap admission admission\n\
              link devices control.engine admission initial.handover\n",
            1 << 20,
        )
        .unwrap();
        assert_eq!(node.links.len(), 1);
        assert_eq!(node.links[0].left_slot, "control.engine");
        assert_eq!(node.links[0].right_slot, "initial.handover");
        let job = InitialJob::parse(
            b"moruna-node-job 1\nbudget 8192\ngrant directory initial.directory\n\
              grant handover initial.handover\n",
        )
        .unwrap();
        assert_eq!(job.grants[1].slot, "handover");
        assert!(!job.grants[1].listener);
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
