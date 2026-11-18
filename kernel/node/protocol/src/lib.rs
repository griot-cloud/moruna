//! A listener capability authorizes admission; no ambient credentials are accepted.
#![no_std]
#![forbid(unsafe_code)]
extern crate alloc;
use alloc::{format, string::String, vec::Vec};
use devices_protocol::Block;
use moruna_svc::{Decoder, Encoder, Handle, Memory, Rights, Wire, WireError, map};

pub mod functions;

pub const MAX_JOBS: usize = 64;
pub use moruna_node::MAX_JOB_GRANTS as MAX_GRANTS;
pub const RUNNING: u32 = 0;
pub const TERMINATED: u32 = 1;
pub const CANCELLED: u32 = 2;

/// A check's account: the engine's `check` on one document, which runs beside a job
/// (system design 3A).
pub const CHECK_BYTES: u64 = 256 << 20;
/// What node-admission keeps of its own account for itself.
pub const ADMISSION_BYTES: u64 = 128 << 20;
/// What no job may have of node-admission's account: the check account and admission's own.
pub const RESERVED_BYTES: u64 = CHECK_BYTES + ADMISSION_BYTES;
/// The slot of the engine's channel to `api` for a job (`moruna serve --listen channel:engine`).
pub const ENGINE_CHANNEL: &str = "engine";
/// The slot of the engine's channel to `api` for a check (`moruna check-job --listen
/// channel:check`).
pub const CHECK_CHANNEL: &str = "check";

/// What a job needs of the node beyond its budget, as the engine's check answers it (item 3.2):
/// the hosts it reaches; each volume by its serial, `true` when the job writes it; and the
/// functions it uses, by name. `node-admission` keeps them with the job, for items 3.3, 3.4
/// and 3.6 to grant from.
pub type Needs = (Vec<String>, Vec<(String, bool)>, Vec<String>);

/// A function a job uses, as `api` registered it (system design 3E): its name, the `https://`
/// address its file is fetched from, and the file's SHA-256 as 64 lowercase hex digits. `launch`
/// is given one for each of the job's `needs.functions`, in that order (PM, item 3.6).
pub type Registered = (String, String, String);

/// How long `node-admission` gives one function's file to arrive (PM, item 3.6): five minutes.
pub const FETCH_NS: u64 = 300 * 1_000_000_000;

/// What `launch` answers (PM, item 3.6): the job's id, or why it was refused, a sentence that
/// travels with the refusal, which `api` fails the job with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launched {
    /// The engine started: the job's id.
    Started(u64),
    /// The job was refused.
    Refused(Refusal),
}

/// A refusal of `launch`: one of [`admission::Error`]'s codes, and a sentence saying why, which
/// names the function when one is the cause.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The code: `admission::Error`'s.
    pub code: u32,
    /// Why, in one sentence.
    pub message: String,
}

impl Refusal {
    /// `error`, saying `message`.
    pub fn new(error: admission::Error, message: impl Into<String>) -> Refusal {
        use moruna_svc::ProtocolError;
        Refusal {
            code: error.code(),
            message: message.into(),
        }
    }

    /// The error its code names, `None` for a code this build does not know.
    pub fn error(&self) -> Option<admission::Error> {
        use moruna_svc::ProtocolError;
        admission::Error::from_code(self.code)
    }
}

/// A refusal that is only its code, as the requests other than `launch` answer: its sentence
/// says what the code means for a job's start.
impl From<admission::Error> for Refusal {
    fn from(error: admission::Error) -> Refusal {
        let message = match error {
            admission::Error::Invalid => "node-admission refused the job's start as invalid",
            admission::Error::Quota => {
                "budget.memory_bytes: more than node-admission has for a job"
            }
            admission::Error::Unavailable => {
                "node-admission cannot start the job: a service it needs is not there"
            }
            admission::Error::NotFound => "node-admission has no such job",
            admission::Error::Limit => "node-admission runs as many engines as it may",
            admission::Error::Refused => "a function the job uses is refused",
        };
        Refusal::new(error, message)
    }
}

impl Wire for Launched {
    fn encode(self, e: &mut Encoder) -> Result<(), WireError> {
        match self {
            Launched::Started(id) => {
                0u8.encode(e)?;
                id.encode(e)
            }
            Launched::Refused(refusal) => {
                1u8.encode(e)?;
                refusal.code.encode(e)?;
                refusal.message.encode(e)
            }
        }
    }

    fn decode(d: &mut Decoder<'_>) -> Result<Self, WireError> {
        match u8::decode(d)? {
            0 => Ok(Launched::Started(u64::decode(d)?)),
            1 => Ok(Launched::Refused(Refusal {
                code: u32::decode(d)?,
                message: String::decode(d)?,
            })),
            _ => Err(WireError::Invalid),
        }
    }
}

/// What of node-admission's account jobs are carved from: its limit less the function cache's
/// account, `cache` bytes, which it carves from its own when `api` sends `functions` (PM, item
/// 3.6), so the job bound ([`budget_max`] of this) is honest.
pub fn jobs_account(admission_account: u64, cache: u64) -> u64 {
    admission_account.saturating_sub(cache)
}

/// The function cache's account for `cache_bytes`, as node config gives it: whole pages, the
/// most of it an account may be (an account's limit is whole pages; the default, an eighth of
/// the node's memory, need not be).
pub fn cache_account(cache_bytes: u64) -> u64 {
    cache_bytes & !4095
}

/// The most of each kind of need a job may name.
pub const MAX_NEEDS: usize = 64;

/// Whether `needs` are a job's: at most [`MAX_NEEDS`] of each, each named once, a host 1 to
/// 253 bytes, a serial or a function 1 to 128, all printable ASCII without spaces, and no `/`
/// in a serial.
pub fn valid_needs(needs: &Needs) -> bool {
    let (hosts, volumes, functions) = needs;
    let named = |name: &str, most: usize| {
        (1..=most).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_graphic())
    };
    let once = |names: &[&str]| {
        names
            .iter()
            .enumerate()
            .all(|(i, n)| !names[..i].contains(n))
    };
    let serials: Vec<&str> = volumes.iter().map(|(serial, _)| serial.as_str()).collect();
    let hosts: Vec<&str> = hosts.iter().map(String::as_str).collect();
    let functions: Vec<&str> = functions.iter().map(String::as_str).collect();
    [hosts.len(), serials.len(), functions.len()]
        .iter()
        .all(|count| *count <= MAX_NEEDS)
        && hosts.iter().all(|h| named(h, 253))
        && serials.iter().all(|s| named(s, 128) && !s.contains('/'))
        && functions.iter().all(|f| named(f, 128))
        && once(&hosts)
        && once(&serials)
        && once(&functions)
}

/// The most a job's budget may be (system design 3A): node-admission's account, which node.conf
/// gives the rest of the node's memory, less [`RESERVED_BYTES`], in whole pages. A job whose
/// document names no budget has all of it: a node runs one job.
pub fn budget_max(admission_account: u64) -> u64 {
    admission_account.saturating_sub(RESERVED_BYTES) & !4095
}

/// Whether `launch` takes a job of `budget` bytes beside admission's account of
/// `admission_account` bytes: `Invalid` for no budget or one that is not whole pages, `Quota`
/// for more than [`budget_max`].
pub fn admits(budget: u64, admission_account: u64) -> Result<(), admission::Error> {
    if budget == 0 || !budget.is_multiple_of(4096) {
        return Err(admission::Error::Invalid);
    }
    if budget > budget_max(admission_account) {
        return Err(admission::Error::Quota);
    }
    Ok(())
}

/// The engine's arguments for a job on a node whose job bound is `bound` bytes
/// ([`budget_max`]): `serve --listen channel:engine --budget-max N`.
pub fn serve_args(bound: u64) -> Vec<String> {
    alloc::vec![
        "serve".into(),
        "--listen".into(),
        alloc::format!("channel:{ENGINE_CHANNEL}"),
        "--budget-max".into(),
        alloc::format!("{bound}"),
    ]
}

/// The engine's arguments for a check on a node whose job bound is `bound` bytes:
/// `check-job --listen channel:check --node channel:engine --budget-max N --volumes
/// slot:volumes`, which reads the document and its secrets' names from the channel, the node's
/// volumes from the data `node-admission` grants every check ([`VOLUMES_SLOT`]), and answers the
/// refusals, the budget and the needs.
pub fn check_args(bound: u64) -> Vec<String> {
    alloc::vec![
        "check-job".into(),
        "--listen".into(),
        alloc::format!("channel:{CHECK_CHANNEL}"),
        "--node".into(),
        alloc::format!("channel:{ENGINE_CHANNEL}"),
        "--budget-max".into(),
        alloc::format!("{bound}"),
        "--volumes".into(),
        alloc::format!("slot:{VOLUMES_SLOT}"),
    ]
}

/// The slot of the data `node-admission` grants every check: the node's volumes, as
/// [`volume_lines`] writes them (system design 3C, item 3.3).
pub const VOLUMES_SLOT: &str = "volumes";

/// The node's volumes as a check reads them: a line for each disk the device manager's
/// directory lists, `<serial> <bytes> <ro|rw> <boot|data>`, in its order. A serial holds no
/// space (`Block::valid`).
pub fn volume_lines(blocks: &[Block]) -> String {
    let mut text = String::new();
    for b in blocks {
        text.push_str(&format!(
            "{} {} {} {}\n",
            b.serial,
            u64::from(b.sector_size) * b.sectors,
            if b.read_only { "ro" } else { "rw" },
            if b.boot { "boot" } else { "data" },
        ));
    }
    text
}

/// The rights of the node's volumes as data a check is granted: read-only, and duplicable,
/// since the engine's supervisor hands each run a read-only duplicate of every data handle it
/// grants, as of the boot's own files.
pub const VOLUMES_RIGHTS: Rights = Rights::READ
    .union(Rights::MAP)
    .union(Rights::TRANSFER)
    .union(Rights::DUPLICATE);

/// The node's volumes as the data a check is granted ([`VOLUMES_SLOT`]): [`volume_lines`] in a
/// memory object of whole pages, the text then zeros, with [`VOLUMES_RIGHTS`].
pub fn volumes_data(blocks: &[Block]) -> Result<Handle, moruna_svc::Error> {
    volumes_data_text(&volume_lines(blocks))
}

/// `text` as the data a check or a job is granted under [`VOLUMES_SLOT`]: a memory object of
/// whole pages, the text then zeros, with [`VOLUMES_RIGHTS`].
pub fn volumes_data_text(text: &str) -> Result<Handle, moruna_svc::Error> {
    let pages = (text.len() as u64 + 1).div_ceil(4096) * 4096;
    let memory = Memory::create(pages)?;
    memory
        .map(0, pages, map::READ | map::WRITE)
        .and_then(|m| m.write(0, text.as_bytes()))?;
    memory.0.duplicate(VOLUMES_RIGHTS)
}

/// The slot of a job's `n`th volume, a connection to its driver (`moruna.driver.block`), which
/// `node-admission` grants the engine at `launch`, in the order its needs name them.
pub fn volume_slot(n: usize) -> String {
    alloc::format!("volume.{n}")
}

/// The data `node-admission` grants a job's engine with its volumes, under [`VOLUMES_SLOT`]: a
/// line each, `<n> <serial> <source|sink>`, `n` its slot's ([`volume_slot`]). A source's
/// writer lease is `node-admission`'s for as long as the job is, so its driver refuses the
/// engine's writes below the engine (system design 3C).
pub fn volume_grants(volumes: &[(&Block, bool)]) -> String {
    let mut text = String::new();
    for (n, (block, write)) in volumes.iter().enumerate() {
        text.push_str(&alloc::format!(
            "{n} {} {}\n",
            block.serial,
            if *write { "sink" } else { "source" }
        ));
    }
    text
}

/// Why a job's volume is not one the node grants it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VolumeRefusal {
    /// No disk of the node has the serial.
    Unknown(String),
    /// The serial is the disk the node booted from, which is never a volume.
    Boot(String),
    /// A sink's volume that its driver serves read only.
    ReadOnly(String),
}

/// A job's volumes, each `(serial, written)` as its needs name it, resolved to the node's disks
/// (`blocks`, the directory's list), in order; refused at the first that names no disk, the boot
/// disk, or, for a sink's, a disk served read only.
pub fn resolve<'a>(
    volumes: &[(String, bool)],
    blocks: &'a [Block],
) -> Result<Vec<(&'a Block, bool)>, VolumeRefusal> {
    volumes
        .iter()
        .map(|(serial, write)| {
            let block = blocks
                .iter()
                .find(|b| b.serial == *serial)
                .ok_or_else(|| VolumeRefusal::Unknown(serial.clone()))?;
            if block.boot {
                return Err(VolumeRefusal::Boot(serial.clone()));
            }
            if *write && block.read_only {
                return Err(VolumeRefusal::ReadOnly(serial.clone()));
            }
            Ok((block, *write))
        })
        .collect()
}

moruna_svc::protocol! {
    /// Jobs execute the node's configured engine image. Every launch supplies a
    /// writable account capability and an explicit carved child budget. The server
    /// grants no filesystem, network, or process root implicitly to the engine.
    pub protocol admission {
        name = "moruna.node.admission";
        major = 1;
        minor = 3;
        requests {
            /// Launch one engine; names and grants correspond exactly. Config is
            /// the existing KD2 engine configuration; names cannot shadow reserved
            /// console/account/config slots. Linux/directory are explicit grants.
            /// `needs` are the job's as its check answered them (`Needs`), kept with
            /// the job for the grants of items 3.3, 3.4 and 3.6; `Invalid` when they
            /// are not `valid_needs`. The job's account of `budget` bytes is carved from
            /// the server's own; a budget past `budget_max` of it is `Quota`.
            /// (1.3, item 3.6) `functions` are the job's `needs.functions` as registered, in
            /// that order: each file is fetched, within `FETCH_NS`, unless the cache keeps it,
            /// checked against its SHA-256, and given the engine as `function.N` with the
            /// `functions` index. The answer is `Launched`: the job's id, or a refusal with
            /// its sentence, which names the function that is the cause.
            1 => fn launch(budget: u64, config: Handle, names: Vec<String>, grants: Vec<Handle>, services: Vec<String>, needs: Needs, functions: Vec<Registered>) -> Launched;
            /// Actual process state; termination does not claim a successful exit.
            2 => fn status(job: u64) -> u32;
            /// Kill the caller's job. A returned success means kill was accepted;
            /// status/wait reports actual termination separately.
            3 => fn cancel(job: u64) -> ();
            /// Complete only when actual process termination is observed.
            4 => fn wait(job: u64) -> u32;
            /// Release a completed job's retained metadata/account handle. Running
            /// jobs must be cancelled and observed terminated before release.
            5 => fn release(job: u64) -> ();
            /// Start the engine's check on one document (system design 3A), as `launch`
            /// starts a job, in a `CHECK_BYTES` account carved from the server's own check
            /// account, so that it runs beside a job. One check at a time: `Limit` until the
            /// last one is released. Status, cancel, wait and release take its id as a job's.
            6 => fn check(config: Handle, names: Vec<String>, grants: Vec<Handle>, services: Vec<String>) -> u64;
            /// The most a job's budget may be: `budget_max` of the server's own account, which
            /// `api` reports as the node's `job_memory_bytes` and starts every engine with.
            /// `Unavailable` when the server has no account of its own.
            7 since 2 => fn bound() -> u64;
            /// (1.3, item 3.6) The function cache: an account of `cache_account(cache_bytes)`
            /// carved from the server's own, node config's `functions.cache_bytes`, which
            /// `api` sends once at its start, before it asks `bound`, which then counts it.
            /// A restarted `api` sends the same again, which changes nothing; another size is
            /// `Invalid`, and an account with no room for it `Quota`.
            8 since 3 => fn functions(cache_bytes: u64) -> ();
        }
        events {}
        errors {
            1 => Invalid;
            2 => Quota;
            3 => Unavailable;
            4 => NotFound;
            5 => Limit;
            /// (1.3, item 3.6) A function the job uses is refused: changed since it was
            /// registered, larger than the cache, not fetched in time, or not kept.
            6 => Refused;
        }
    }
}

pub fn valid_grants(names: &[String], count: usize) -> bool {
    names.len() == count
        && count <= MAX_GRANTS
        && names.iter().enumerate().all(|(i, n)| {
            !n.is_empty()
                && n.len() <= 32
                && n.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
                && !["console", "account", "config"].contains(&n.as_str())
                && !names[..i].contains(n)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launch_answers_its_job_or_its_refusal_with_the_sentence() {
        let refused = Refusal::new(
            admission::Error::Refused,
            "the function `gate-score`'s image has changed since it was registered",
        );
        assert_eq!(refused.error(), Some(admission::Error::Refused));
        for answer in [Launched::Started(7), Launched::Refused(refused)] {
            let mut e = Encoder::new();
            answer.clone().encode(&mut e).unwrap();
            let bytes = e.into_control_bytes().unwrap();
            let mut d = Decoder::new(&bytes, &mut []);
            assert_eq!(Launched::decode(&mut d), Ok(answer));
            assert_eq!(d.finish(), Ok(()));
        }
        let mut d = Decoder::new(&[2], &mut []);
        assert_eq!(Launched::decode(&mut d), Err(WireError::Invalid));
        assert_eq!(
            Refusal {
                code: 99,
                message: String::new()
            }
            .error(),
            None
        );
    }

    #[test]
    fn the_function_cache_comes_out_of_the_job_bound() {
        let account = 8u64 << 30;
        // Nothing carved: the bound as before.
        assert_eq!(jobs_account(account, 0), account);
        // A 1 GiB cache is a GiB less for jobs.
        assert_eq!(
            budget_max(jobs_account(account, 1 << 30)),
            budget_max(account) - (1 << 30)
        );
        assert_eq!(jobs_account(1 << 20, 1 << 30), 0);
        // QEMU's 2 GiB node: the default, an eighth of it, is not whole pages; its account is.
        let default = 2_032_979_968 / 8;
        assert_eq!(cache_account(default), 254_119_936);
        assert_eq!(cache_account(default) % 4096, 0);
        assert_eq!(cache_account(1 << 30), 1 << 30);
        assert_eq!(FETCH_NS / 1_000_000_000, 300, "five minutes an image");
    }

    #[test]
    fn a_jobs_budget_is_admissions_account_less_384_mib() {
        assert_eq!(RESERVED_BYTES, 384 << 20);
        let account = 8u64 << 30;
        assert_eq!(budget_max(account), (8 << 30) - (384 << 20));
        // Whole pages, and nothing for an account smaller than what it keeps.
        assert_eq!(budget_max(RESERVED_BYTES + 4096 + 100), 4096);
        assert_eq!(budget_max(RESERVED_BYTES), 0);
        assert_eq!(budget_max(1 << 20), 0);
        assert_eq!(admits(budget_max(account), account), Ok(()));
        assert_eq!(admits(4096, account), Ok(()));
        assert_eq!(
            admits(budget_max(account) + 4096, account),
            Err(admission::Error::Quota)
        );
        assert_eq!(admits(0, account), Err(admission::Error::Invalid));
        assert_eq!(admits(4097, account), Err(admission::Error::Invalid));
        assert_eq!(admits(4096, RESERVED_BYTES), Err(admission::Error::Quota));
    }

    #[test]
    fn a_jobs_needs_are_bounded_and_each_named_once() {
        let s = |names: &[&str]| -> Vec<String> { names.iter().map(|n| (*n).into()).collect() };
        let none: Needs = (Vec::new(), Vec::new(), Vec::new());
        assert!(valid_needs(&none));
        let every: Needs = (
            s(&["s3.example.com", "acct.blob.core.windows.net"]),
            alloc::vec![("vol-in".into(), false), ("vol-out".into(), true)],
            s(&["std", "taxi.clean"]),
        );
        assert!(valid_needs(&every));
        let with = |change: &dyn Fn(&mut Needs)| {
            let mut needs = every.clone();
            change(&mut needs);
            valid_needs(&needs)
        };
        assert!(!with(&|n| n.0.push("s3.example.com".into())));
        assert!(!with(&|n| n.1.push(("vol-in".into(), true))));
        assert!(!with(&|n| n.2.push("std".into())));
        assert!(!with(&|n| n.0.push(String::new())));
        assert!(!with(&|n| n.0.push("a b".into())));
        assert!(!with(&|n| n.0.push("h".repeat(254))));
        assert!(with(&|n| n.0.push("h".repeat(253))));
        assert!(!with(&|n| n.1.push(("a/b".into(), false))));
        assert!(!with(&|n| n.2.push("f".repeat(129))));
        let many = |count: usize| -> Needs {
            (
                (0..count).map(|i| alloc::format!("h{i}")).collect(),
                Vec::new(),
                Vec::new(),
            )
        };
        assert!(valid_needs(&many(MAX_NEEDS)));
        assert!(!valid_needs(&many(MAX_NEEDS + 1)));
    }

    #[test]
    fn the_engine_serves_and_checks_on_its_channels_with_the_nodes_bound() {
        let node = 4u64 << 30;
        let max = alloc::format!("{node}");
        assert_eq!(
            serve_args(node),
            [
                "serve",
                "--listen",
                "channel:engine",
                "--budget-max",
                max.as_str()
            ]
        );
        assert_eq!(
            check_args(node),
            [
                "check-job",
                "--listen",
                "channel:check",
                "--node",
                "channel:engine",
                "--budget-max",
                max.as_str(),
                "--volumes",
                "slot:volumes"
            ]
        );
    }

    fn disk(serial: &str, read_only: bool, boot: bool) -> Block {
        Block {
            serial: serial.into(),
            sector_size: 512,
            sectors: 2048,
            read_only,
            boot,
        }
    }

    #[test]
    fn a_check_reads_the_nodes_volumes_one_line_a_disk() {
        let blocks = [
            disk("vol-a", false, false),
            Block {
                sector_size: 4096,
                ..disk("lun-0", true, false)
            },
            disk("boot", true, true),
        ];
        assert_eq!(
            volume_lines(&blocks),
            "vol-a 1048576 rw data\nlun-0 8388608 ro data\nboot 1048576 ro boot\n"
        );
        assert_eq!(volume_lines(&[]), "");
    }

    /// The data a check is granted holds the lines, is read-only, and can be duplicated
    /// read-only, as the engine's supervisor duplicates it for its run (a boot found the
    /// duplicate refused without DUPLICATE, item 3.2).
    #[test]
    fn a_checks_volumes_are_read_only_data_its_supervisor_can_duplicate() {
        let blocks = [disk("vol-a", false, false), disk("boot", true, true)];
        let handle = volumes_data(&blocks).expect("the data");
        let (kind, rights) = handle.info().expect("its info");
        assert_eq!(kind, moruna_svc::ObjectType::Memory);
        assert_eq!(rights, VOLUMES_RIGHTS);
        assert!(!rights.contains(Rights::WRITE));
        let run = handle
            .duplicate(Rights::READ | Rights::MAP | Rights::TRANSFER)
            .expect("the supervisor's duplicate for its run");
        let memory = Memory(run);
        let mapping = memory.map(0, 4096, map::READ).expect("it maps");
        let mut bytes = alloc::vec![0u8; 4096];
        mapping.read(0, &mut bytes).expect("it reads");
        let end = bytes
            .iter()
            .position(|b| *b == 0)
            .expect("zeros after the text");
        assert_eq!(&bytes[..end], volume_lines(&blocks).as_bytes());
        // No disk: one page of zeros.
        let none = Memory(volumes_data(&[]).expect("no volumes"));
        let mut first = [1u8; 1];
        none.map(0, 4096, map::READ)
            .and_then(|m| m.read(0, &mut first))
            .expect("it reads");
        assert_eq!(first, [0]);
    }

    #[test]
    fn a_jobs_volumes_are_granted_by_slot_and_listed_source_or_sink() {
        let a = disk("vol-in", true, false);
        let b = disk("vol-out", false, false);
        assert_eq!(volume_slot(0), "volume.0");
        assert_eq!(volume_slot(12), "volume.12");
        assert_eq!(
            volume_grants(&[(&a, false), (&b, true)]),
            "0 vol-in source\n1 vol-out sink\n"
        );
        assert_eq!(volume_grants(&[]), "");
        assert!(valid_grants(&[volume_slot(0), volume_slot(1)], 2));
    }

    #[test]
    fn a_jobs_volumes_are_the_nodes_disks_never_its_boot_disk() {
        let blocks = [
            disk("vol-in", true, false),
            disk("vol-out", false, false),
            disk("boot", true, true),
        ];
        let v = |serial: &str, write: bool| (String::from(serial), write);
        let resolved = resolve(&[v("vol-in", false), v("vol-out", true)], &blocks).unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(
            (resolved[0].0.serial.as_str(), resolved[0].1),
            ("vol-in", false)
        );
        assert_eq!(
            (resolved[1].0.serial.as_str(), resolved[1].1),
            ("vol-out", true)
        );
        assert_eq!(resolve(&[], &blocks), Ok(alloc::vec![]));
        assert_eq!(
            resolve(&[v("vol-in", false), v("vol-x", false)], &blocks),
            Err(VolumeRefusal::Unknown("vol-x".into()))
        );
        assert_eq!(
            resolve(&[v("boot", false)], &blocks),
            Err(VolumeRefusal::Boot("boot".into()))
        );
        assert_eq!(
            resolve(&[v("vol-in", true)], &blocks),
            Err(VolumeRefusal::ReadOnly("vol-in".into()))
        );
        assert_eq!(
            resolve(&[v("vol-in", false)], &[]),
            Err(VolumeRefusal::Unknown("vol-in".into()))
        );
    }

    #[test]
    fn grant_names_cannot_shadow_authority_or_alias() {
        assert!(!valid_grants(&["account".into()], 1));
        assert!(!valid_grants(&["linux".into(), "linux".into()], 2));
        assert!(!valid_grants(&["linux".into()], 0));
        assert!(valid_grants(&["linux".into(), "directory".into()], 2));
    }
}

moruna_svc::protocol! {
    /// Private boot-granted node owner control, never an admission client grant.
    pub protocol control {
        name = "moruna.node.control";
        major = 1;
        minor = 0;
        requests {
            /// Stop admissions, kill all owned engines and reply only after their
            /// actual process termination signals. The supervisor owns fallback
            /// escrow handles if this service itself has failed.
            1 => fn shutdown() -> ();
            /// Revoke one explicit trusted boot grant. Never installs new rights.
            2 => fn revoke(grant_id:u64) -> ();
        }
        events {}
        errors { 1 => Unavailable; }
    }
}
