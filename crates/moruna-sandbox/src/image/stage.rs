//! A stage's instances on a Linux node (system design 3E, item 6.3f): an image function runs
//! one process for each CPU of the job unless it declares fewer, as it does on the Moruna
//! kernel, by the rule both follow (`moruna_adapters::instances`).
//!
//! - **How many:** the rule's count of the job's CPUs, bounded by memory. The job's CPUs are
//!   the run's CPU quota, rounded up, read when the run is bound: the figure the engine starts
//!   its workers from (its discovery's: the job's `budget.cpu`, else `MORUNA_CPU`, else its
//!   cgroup's `cpu.max`, else the machine's cores), so a stage has an instance for each
//!   worker. Each instance is funded with an equal share of the stage's ceiling, at least
//!   [`LEAST`], and the stage starts as many as that funds ([`funded`]).
//! - **One cgroup for the stage** carries the stage's ceiling (`memory.max`) and the run's CPU
//!   quota (`cpu.max`), with one cgroup for each instance under it ([`Stage`]). An instance is
//!   carved as the node carves its task (item 6.3v, [`Carve`]): a measured stage's at its
//!   line's account at the stage's measured piece, under half of what the other instances'
//!   limits leave of the ceiling; an unmeasured stage's at the equal share; one its cgroup
//!   killed, at the account it grew to. Their limits never sum past the ceiling as they are
//!   carved.
//! - **What the stage holds** outside the engine's process is the sum over its instances
//!   ([`Stage::memory_current`]), and limits that moved are written to the stage and to every
//!   instance carved by the share ([`Stage::write_limits`]); an account of an instance's own
//!   stays.
//! - **What one instance takes** (system design 3A): a measured stage's measured piece, or for
//!   an unmeasured one what its share holds, its share over its factor. The controller holds
//!   the stage's morsels to it, and a larger batch is cut into slices of its rows ([`cut`]),
//!   whose answers are joined in order.
//! - **What the instances did** is the run report's ([`Busy`]): how many held a batch at once,
//!   the batches each answered, and the batches cut.
//!
//! The instances and the engine's workers have that one source, and still differ where they
//! are meant to:
//! - `budget.elastic.cpu_max`: the engine makes that many workers and starts as many as the
//!   quota; the instances are the quota's.
//! - More than `instances::MAX` CPUs: a stage runs at most 64 instances.
//! - An image that declares a ceiling (`MORUNA_PROCESSES`) runs no more than it.
//! - Memory that funds fewer instances than the rule asks for.
//! - Limits that move while the run goes: the engine's workers follow them, and the instances
//!   are those the stage started with.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use moruna_adapters::instances;

use super::lock;
use crate::cgroup::StageCgroup;

/// The least an instance's share is: the least a stage's task is started in on the Moruna
/// kernel (its `TASK_MIN`, SDD 30 f.11), a process, its threads, its image's writable pages and
/// its heap. It bounds how many instances a stage starts and refuses none.
pub(super) const LEAST: u64 = 16 << 20;

/// The page a share is whole in.
const PAGE: u64 = 4096;

/// How many of the `asked` instances `room` funds at `need` each, at least [`LEAST`]
/// (`instances::fund`): all of them when it funds them all, otherwise as many as it funds. One
/// when it funds none: an instance in the whole ceiling, as a stage ran before it had
/// instances, because memory bounds the instances and never fails a job that ran before.
pub(super) fn funded(room: u64, asked: usize, need: u64) -> usize {
    match instances::fund(room, &[asked], PAGE, need.max(LEAST)) {
        Ok(Some(funded)) => funded.total().max(1),
        _ => 1,
    }
}

/// Each instance's share of `ceiling` among `count` instances: the whole of it for one,
/// otherwise equal shares in whole pages, as `instances::fund` gives them.
pub(super) fn share(ceiling: u64, count: usize) -> u64 {
    match count {
        0 | 1 => ceiling,
        n => (ceiling / n as u64) & !(PAGE - 1),
    }
}

/// The slices of `rows` rows, `len` bytes in all, that an instance taking at most `piece` bytes
/// is given, as `(first row, rows)` in order (system design 3A; the Moruna kernel's image task
/// cuts its pieces by the same rule): the whole batch when it fits, has one row or nothing
/// bounds it; else equal runs of rows sized for nine tenths of the piece, so rows of uneven
/// size still fit as a rule. The caller cuts again a slice that does not.
pub(super) fn cut(rows: usize, len: u64, piece: u64) -> Vec<(usize, usize)> {
    if len <= piece || rows <= 1 || piece == 0 {
        return vec![(0, rows)];
    }
    let target = piece - piece / 10;
    let count = usize::try_from(len.div_ceil(target.max(1)))
        .unwrap_or(rows)
        .clamp(1, rows);
    let each = rows.div_ceil(count);
    (0..rows)
        .step_by(each)
        .map(|at| (at, each.min(rows - at)))
        .collect()
}

/// The pieces a measured stage's instances are costed at when the run funds them, the largest
/// first: `largest` (the largest morsel the run gives), halved while above `least` (its least
/// morsel), and `least` last, as `instances::fund_measured` takes them: the piece shrinks
/// first, and only at the least do fewer instances run. At least one byte.
pub(super) fn ladder(largest: u64, least: u64) -> Vec<u64> {
    let least = least.max(1);
    let mut out = Vec::new();
    let mut piece = largest;
    while piece > least {
        out.push(piece);
        piece /= 2;
    }
    out.push(least);
    out
}

/// What a stage's limits were last written as.
struct Limits {
    ceiling: u64,
    /// The CPU quota's bits, so a quota that did not move compares equal.
    cores: u64,
    share: u64,
}

/// How an instance's `memory.max` is carved as it enters its stage (system design 3A, 3E; PM,
/// 2026-11-19, 6.3v: a Linux instance is carved as the node carves its task).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Carve {
    /// A stage with no measurement: 6.3f's equal share of the stage's ceiling, which follows
    /// the ceiling and the count as they are written; never more than the other instances'
    /// limits leave of the ceiling.
    Share,
    /// A measured stage's instance, at the line's account at the stage's measured piece:
    /// `instances::measured_task_limit(account, room)`, the room the stage's ceiling less the
    /// other instances' limits. Its own from then on.
    Measured(u64),
    /// An instance its cgroup killed, started again at the account it grew to
    /// (`instances::restarted_limit`, from what the others leave). Its own from then on.
    Grown(u64),
}

/// A live instance of the stage: its cgroup, and its `memory.max` as last written.
struct Held {
    id: u64,
    cgroup: Arc<StageCgroup>,
    limit: u64,
    /// Carved at an account of its own (measured, or grown by a restart), which the stage's
    /// share does not move.
    own: bool,
}

/// One stage's cgroups: the stage's own, which carries its ceiling, and each live instance's
/// under it. The stage's is removed when the last of them is.
pub(super) struct Stage {
    /// Declared before `cgroup`, so an instance's cgroup still listed goes before the stage's.
    instances: Mutex<Vec<Held>>,
    cgroup: StageCgroup,
    /// The instances the ceiling is shared among.
    count: AtomicUsize,
    limits: Mutex<Limits>,
    /// Names the instances' cgroups apart: an instance started again takes a new one.
    started: AtomicU64,
}

impl Stage {
    /// Create the stage's cgroup `<parent>/<name>` with `ceiling` and a quota of `cores`, for
    /// `count` instances to share.
    pub(super) fn create(
        parent: &Path,
        name: &str,
        ceiling: u64,
        cores: f64,
        count: usize,
    ) -> io::Result<Stage> {
        let cgroup = StageCgroup::create(parent, name, ceiling, cores)?;
        cgroup.enable_children()?;
        let count = count.max(1);
        Ok(Stage {
            instances: Mutex::new(Vec::new()),
            cgroup,
            count: AtomicUsize::new(count),
            limits: Mutex::new(Limits {
                ceiling,
                cores: cores.to_bits(),
                share: share(ceiling, count),
            }),
            started: AtomicU64::new(0),
        })
    }

    /// The stage's cgroup's directory.
    pub(super) fn dir(&self) -> &Path {
        self.cgroup.dir()
    }

    /// Each instance's share of the ceiling, as last written.
    pub(super) fn share(&self) -> u64 {
        lock(&self.limits).share
    }

    /// The stage's ceiling, as last written.
    pub(super) fn ceiling(&self) -> u64 {
        lock(&self.limits).ceiling
    }

    /// The place of instance `instance`: its cgroup, made under the stage's with the
    /// `memory.max` its `carve` gives it out of what the other instances' limits leave of the
    /// ceiling, so their limits never sum past it. Refused, in words, when that leaves not a
    /// page.
    pub(super) fn enter(self: &Arc<Self>, instance: usize, carve: Carve) -> io::Result<Place> {
        // The limits are held, so a share written meanwhile reaches this instance too.
        let limits = lock(&self.limits);
        let room = self.room_beside(&limits, None);
        let (limit, own) = match carve {
            Carve::Share => (Some(limits.share.min(room)), false),
            Carve::Measured(account) => (instances::measured_task_limit(account, room), true),
            Carve::Grown(account) => (Some(account), true),
        };
        let limit = limit.filter(|&limit| limit >= PAGE).ok_or_else(|| {
            io::Error::other(format!(
                "the stage's ceiling of {} bytes leaves {room} beside its other instances' \
                 limits, too few for instance {instance}",
                limits.ceiling
            ))
        })?;
        let id = self.started.fetch_add(1, Ordering::Relaxed);
        let name = format!("instance-{instance}-{id}");
        let cgroup = Arc::new(self.cgroup.child(&name, limit)?);
        lock(&self.instances).push(Held {
            id,
            cgroup: Arc::clone(&cgroup),
            limit,
            own,
        });
        Ok(Place {
            id,
            cgroup,
            stage: Arc::clone(self),
        })
    }

    /// What the stage's ceiling as last written leaves beside the limits of its live
    /// instances other than the one numbered `except` (system design 3A: the room an instance
    /// is carved from, or that a restart grows it in, with its own limit returned).
    fn room_beside(&self, limits: &Limits, except: Option<u64>) -> u64 {
        let others = lock(&self.instances)
            .iter()
            .filter(|held| Some(held.id) != except)
            .fold(0u64, |sum, held| sum.saturating_add(held.limit));
        limits.ceiling.saturating_sub(others)
    }

    /// What the stage's ceiling leaves an instance numbered `id` beside the others' limits,
    /// its own returned.
    fn room_for(&self, id: u64) -> u64 {
        self.room_beside(&lock(&self.limits), Some(id))
    }

    /// The `memory.max` of the instance numbered `id` as last written; `None` when it is gone.
    fn limit_of(&self, id: u64) -> Option<u64> {
        lock(&self.instances)
            .iter()
            .find(|held| held.id == id)
            .map(|held| held.limit)
    }

    /// The instance numbered `id` is gone: its cgroup is no longer the stage's to read or
    /// write.
    fn leave(&self, id: u64) {
        let mut held = lock(&self.instances);
        let gone = held
            .iter()
            .position(|held| held.id == id)
            .map(|at| held.swap_remove(at));
        drop(held);
        // Outside the lock: removing a cgroup waits for its processes to be gone.
        drop(gone);
    }

    /// What the stage's instances hold now, summed (SDD 19 IM-I7); zero with none.
    pub(super) fn memory_current(&self) -> io::Result<u64> {
        let mut sum = 0u64;
        for held in lock(&self.instances).iter() {
            sum = sum.saturating_add(held.cgroup.memory_current()?);
        }
        Ok(sum)
    }

    /// Every live instance carved by the share has `share` as its `memory.max`; one carved at an
    /// account of its own keeps it, as a task's account on the Moruna kernel does.
    fn write_shares(&self, share: u64) -> io::Result<()> {
        for held in lock(&self.instances).iter_mut().filter(|held| !held.own) {
            held.cgroup.set_memory_max(share)?;
            held.limit = share;
        }
        Ok(())
    }

    /// The stage's ceiling is shared among `count` instances from now on (system design 3A: the
    /// count a warm-up's measurement funds), at least one: every live instance carved by the
    /// share has its new share as its `memory.max`, and an instance started later by the share
    /// is given it. The share.
    pub(super) fn recount(&self, count: usize) -> io::Result<u64> {
        let mut limits = lock(&self.limits);
        let count = count.max(1);
        let share = share(limits.ceiling, count);
        self.write_shares(share)?;
        self.count.store(count, Ordering::Relaxed);
        limits.share = share;
        Ok(share)
    }

    /// The stage's `memory.max` and `cpu.max`, and the share of the ceiling as the `memory.max`
    /// of every instance carved by it, written when either moved since they were last written.
    /// The share.
    pub(super) fn write_limits(&self, ceiling: u64, cores: f64) -> io::Result<u64> {
        let mut limits = lock(&self.limits);
        if limits.ceiling == ceiling && limits.cores == cores.to_bits() {
            return Ok(limits.share);
        }
        let share = share(ceiling, self.count.load(Ordering::Relaxed));
        self.cgroup.set_memory_max(ceiling)?;
        self.cgroup.set_cpu_max(cores)?;
        self.write_shares(share)?;
        *limits = Limits {
            ceiling,
            cores: cores.to_bits(),
            share,
        };
        Ok(share)
    }
}

/// An instance's place in its stage ([`Stage::enter`]), held by the instance's state after
/// its process. When it is dropped the instance's cgroup goes, and then, with the stage's
/// last instance, the stage's.
pub(super) struct Place {
    id: u64,
    /// Declared before `stage`, so the instance's cgroup goes before the stage's.
    cgroup: Arc<StageCgroup>,
    stage: Arc<Stage>,
}

impl Place {
    /// The instance's cgroup, which its process joins.
    pub(super) fn cgroup(&self) -> &Arc<StageCgroup> {
        &self.cgroup
    }

    /// The instance's `memory.max`, as last written.
    pub(super) fn limit(&self) -> u64 {
        self.stage
            .limit_of(self.id)
            .unwrap_or_else(|| self.stage.share())
    }

    /// What the stage's ceiling leaves this instance beside the others' limits, its own
    /// returned.
    pub(super) fn room(&self) -> u64 {
        self.stage.room_for(self.id)
    }

    /// The stage.
    pub(super) fn stage(&self) -> &Stage {
        &self.stage
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        self.stage.leave(self.id);
    }
}

/// What a stage's instances did, for the run's report.
#[derive(Default)]
pub(super) struct Busy {
    /// Instances holding a batch now, and the most at once.
    now: AtomicUsize,
    peak: AtomicUsize,
    /// The batches each instance answered, by its index.
    batches: Mutex<Vec<u64>>,
    /// The batches past an instance's cap that were cut, and the slices they were cut into.
    cut: AtomicU64,
    slices: AtomicU64,
}

/// An instance holding a batch, until it is dropped.
pub(super) struct Holding(Arc<Busy>);

impl Busy {
    /// An instance holds a batch from now until what is returned is dropped.
    pub(super) fn hold(self: &Arc<Self>) -> Holding {
        let now = self.now.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak.fetch_max(now, Ordering::Relaxed);
        Holding(Arc::clone(self))
    }

    /// Instance `instance` answered a batch.
    pub(super) fn answered(&self, instance: usize) {
        let mut batches = lock(&self.batches);
        if batches.len() <= instance {
            batches.resize(instance + 1, 0);
        }
        batches[instance] += 1;
    }

    /// A batch past an instance's cap was cut into `slices`.
    pub(super) fn cut(&self, slices: usize) {
        self.cut.fetch_add(1, Ordering::Relaxed);
        self.slices.fetch_add(slices as u64, Ordering::Relaxed);
    }

    /// The most instances that held a batch at once.
    pub(super) fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }

    /// The run report's note of stage `name`'s `count` instances: the most that held a batch at
    /// once (`busy_peak`, as the Moruna kernel's stage says it) and the batches each answered;
    /// and, when memory funded fewer than the `asked` the rule gives, that it did
    /// (`instances::short_note`).
    pub(super) fn note(&self, name: &str, count: usize, asked: usize) -> String {
        let batches = lock(&self.batches);
        let mut note = format!(
            "image `{name}`: {count} instances, busy_peak {} (the most holding a batch at once)",
            self.peak()
        );
        for instance in 0..count {
            let answered = batches.get(instance).copied().unwrap_or(0);
            note.push_str(&format!("; instance {instance}: {answered} batches"));
        }
        let cut = self.cut.load(Ordering::Relaxed);
        if cut > 0 {
            note.push_str(&format!(
                "; {cut} batches past an instance's cap cut into {} slices",
                self.slices.load(Ordering::Relaxed)
            ));
        }
        if let Some(short) = instances::short_note(name, count, asked) {
            note.push_str(&format!("; {short}"));
        }
        note
    }
}

impl Drop for Holding {
    fn drop(&mut self) {
        self.0.now.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    const MIB: u64 = 1 << 20;

    /// A plain directory that stands in for a cgroup v2 parent with `memory` and `cpu`
    /// delegated, as the cgroup's own tests use one.
    fn parent(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("moruna-stage-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cgroup.subtree_control"), "cpu memory\n").unwrap();
        dir
    }

    /// What the kernel answers a stage's `+memory +cpu` with: the controllers, by name.
    fn enabled(stage: &Stage) {
        let control = stage.dir().join("cgroup.subtree_control");
        assert_eq!(fs::read_to_string(&control).unwrap(), "+memory +cpu");
        fs::write(control, "cpu memory\n").unwrap();
    }

    fn read(dir: &Path, file: &str) -> String {
        fs::read_to_string(dir.join(file)).unwrap()
    }

    /// The stage's ceiling, as last written.
    fn ceiling(stage: &Stage) -> u64 {
        lock(&stage.limits).ceiling
    }

    /// The instances the stage's ceiling is shared among.
    fn count(stage: &Stage) -> usize {
        stage.count.load(Ordering::Relaxed)
    }

    /// A warm-up's measurement funds another count: every live instance's share is written
    /// at once, one started later is given it, and the ceiling's next move keeps the count.
    #[test]
    fn a_stage_shares_its_ceiling_among_the_count_it_is_given() {
        let dir = parent("recount");
        let stage = Arc::new(Stage::create(&dir, "stage", 1 << 30, 4.0, 4).unwrap());
        enabled(&stage);
        let first = stage.enter(0, Carve::Share).unwrap();
        assert_eq!(
            read(first.cgroup().dir(), "memory.max"),
            (256 * MIB).to_string()
        );
        assert_eq!(stage.recount(2).unwrap(), 512 * MIB);
        assert_eq!(count(&stage), 2);
        assert_eq!(
            read(first.cgroup().dir(), "memory.max"),
            (512 * MIB).to_string()
        );
        let second = stage.enter(1, Carve::Share).unwrap();
        assert_eq!(
            read(second.cgroup().dir(), "memory.max"),
            (512 * MIB).to_string()
        );
        assert_eq!(stage.write_limits(1 << 29, 4.0).unwrap(), 256 * MIB);
        // None is one, the whole ceiling.
        assert_eq!(stage.recount(0).unwrap(), 1 << 29);
        // An instance that refuses the write is an error.
        fs::remove_dir_all(second.cgroup().dir()).unwrap();
        assert!(stage.recount(3).is_err());
        gone(first);
        drop(second);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_warm_up_s_pieces_go_from_the_largest_morsel_down_to_the_least() {
        assert_eq!(
            ladder(512 * MIB, 4 * MIB),
            vec![
                512 * MIB,
                256 * MIB,
                128 * MIB,
                64 * MIB,
                32 * MIB,
                16 * MIB,
                8 * MIB,
                4 * MIB
            ]
        );
        // Not a power of two of the least: the least comes last.
        assert_eq!(
            ladder(20 * MIB, 4 * MIB),
            vec![20 * MIB, 10 * MIB, 5 * MIB, 4 * MIB]
        );
        // A largest at or under the least: the least alone; and never a zero piece.
        assert_eq!(ladder(MIB, 4 * MIB), vec![4 * MIB]);
        assert_eq!(ladder(0, 0), vec![1]);
    }

    /// An instance gone. The kernel removes a cgroup's files with it; the fixture's are
    /// plain files, removed here so the directory goes as a cgroup does.
    fn gone(place: Place) {
        fs::remove_dir_all(place.cgroup().dir()).unwrap();
        drop(place);
    }

    #[test]
    fn memory_bounds_the_instances_and_refuses_none() {
        // 4 CPUs, a gibibyte: every instance, a quarter each.
        assert_eq!(funded(1 << 30, 4, LEAST), 4);
        assert_eq!(share(1 << 30, 4), 256 * MIB);
        // 40 MiB funds two of four at the least.
        assert_eq!(funded(40 * MIB, 4, LEAST), 2);
        assert_eq!(share(40 * MIB, 2), 20 * MIB);
        // Exactly one least, and one page under two.
        assert_eq!(funded(LEAST, 4, LEAST), 1);
        assert_eq!(funded(2 * LEAST - 1, 4, LEAST), 1);
        assert_eq!(funded(2 * LEAST, 4, LEAST), 2);
        // Not one least: one instance in the whole ceiling, as before; never a refusal.
        assert_eq!(funded(LEAST - 1, 4, LEAST), 1);
        assert_eq!(funded(1, 64, LEAST), 1);
        assert_eq!(funded(0, 4, LEAST), 1);
        // A need above the least: 4 GiB at 2 GiB an instance funds two; under one, one.
        assert_eq!(funded(4096 * MIB, 4, 2048 * MIB), 2);
        assert_eq!(funded(2047 * MIB, 4, 2048 * MIB), 1);
        // A need under the least is the least.
        assert_eq!(funded(64 * MIB, 8, MIB), 4);
        assert_eq!(
            share(LEAST - 1, 1),
            LEAST - 1,
            "one holds the whole ceiling"
        );
        assert_eq!(share(7, 0), 7);
        // Shares are whole pages and never more than the ceiling together.
        let odd = 3 * LEAST + 4097;
        assert_eq!(funded(odd, 3, LEAST), 3);
        assert_eq!(share(odd, 3) % PAGE, 0);
        assert!(3 * share(odd, 3) <= odd);
        // A stage that asked for one is funded one.
        assert_eq!(funded(1 << 30, 1, LEAST), 1);
    }

    /// 6.3f's rule over the cgroup's files: one cgroup for the stage, which carries its ceiling
    /// and the quota, one under it for each instance with its share, what they hold summed,
    /// and limits that moved written to all of them.
    #[test]
    fn a_stage_holds_one_cgroup_an_instance_under_its_own() {
        let dir = parent("instances");
        let stage = Arc::new(Stage::create(&dir, "stage", 1 << 30, 4.0, 4).unwrap());
        enabled(&stage);
        assert_eq!(stage.dir(), dir.join("stage"));
        assert_eq!(read(stage.dir(), "memory.max"), (1u64 << 30).to_string());
        assert_eq!(read(stage.dir(), "cpu.max"), "400000 100000");
        assert_eq!((count(&stage), ceiling(&stage)), (4, 1 << 30));
        assert_eq!(stage.share(), 256 * MIB);
        assert_eq!(stage.memory_current().unwrap(), 0, "no instance yet");

        let mut places = Vec::new();
        for instance in 0..4 {
            let place = stage.enter(instance, Carve::Share).unwrap();
            assert_eq!(place.id, instance as u64);
            let at = place.cgroup().dir();
            assert_eq!(
                at,
                stage.dir().join(format!("instance-{instance}-{instance}"))
            );
            assert_eq!(read(at, "memory.max"), (256 * MIB).to_string());
            assert_eq!(read(at, "memory.swap.max"), "0");
            // No quota of its own: the stage's holds every instance.
            assert_eq!(read(at, "cpu.max"), "max 100000");
            fs::write(
                at.join("memory.current"),
                format!("{}\n", (instance as u64 + 1) * MIB),
            )
            .unwrap();
            places.push(place);
        }
        assert_eq!(count(places[0].stage()), 4);
        assert_eq!(stage.memory_current().unwrap(), 10 * MIB, "summed");

        // Nothing moved: nothing is written.
        fs::write(stage.dir().join("memory.max"), "untouched").unwrap();
        assert_eq!(stage.write_limits(1 << 30, 4.0).unwrap(), 256 * MIB);
        assert_eq!(read(stage.dir(), "memory.max"), "untouched");
        // The ceiling moved: the stage's and every instance's share of it.
        assert_eq!(stage.write_limits(1 << 29, 4.0).unwrap(), 128 * MIB);
        assert_eq!(read(stage.dir(), "memory.max"), (1u64 << 29).to_string());
        for place in &places {
            let at = place.cgroup().dir();
            assert_eq!(read(at, "memory.max"), (128 * MIB).to_string());
            assert_eq!(read(at, "cpu.max"), "max 100000");
        }
        // The quota moved alone: the stage's, and the shares as they were.
        assert_eq!(stage.write_limits(1 << 29, 2.0).unwrap(), 128 * MIB);
        assert_eq!(read(stage.dir(), "cpu.max"), "200000 100000");
        assert_eq!((ceiling(&stage), stage.share()), (1 << 29, 128 * MIB));

        // An instance that cannot be read fails the sum, never reads as zero.
        fs::write(places[2].cgroup().dir().join("memory.current"), "x").unwrap();
        assert!(stage.memory_current().is_err());
        // An instance gone is no longer read or written, and its cgroup goes with it; one
        // started again takes a new cgroup at the share in force.
        let third = places[2].cgroup().dir().to_path_buf();
        gone(places.remove(2));
        assert!(!third.exists());
        assert_eq!(stage.memory_current().unwrap(), 7 * MIB);
        let again = stage.enter(2, Carve::Share).unwrap();
        assert_eq!(again.id, 4);
        assert_eq!(again.cgroup().dir(), stage.dir().join("instance-2-4"));
        assert_eq!(
            read(again.cgroup().dir(), "memory.max"),
            (128 * MIB).to_string()
        );
        assert!(stage.memory_current().is_err(), "it has no memory.current");
        gone(again);
        stage.leave(4);
        assert_eq!(stage.memory_current().unwrap(), 7 * MIB);
        // A write an instance refuses is an error, and is tried again the next time.
        fs::remove_dir_all(places[2].cgroup().dir()).unwrap();
        assert!(stage.write_limits(1 << 28, 2.0).is_err());
        assert_eq!(ceiling(&stage), 1 << 29, "not taken as written");
        drop(places.pop());
        assert_eq!(stage.write_limits(1 << 28, 2.0).unwrap(), 64 * MIB);
        assert_eq!(
            read(places[0].cgroup().dir(), "memory.max"),
            (64 * MIB).to_string()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Item 6.3v (PM, 2026-11-19): an instance is carved as the node carves its task. A measured
    /// one at its account, in whole pages, under half of what the others' limits leave of the
    /// ceiling; one started again at the account it grew to; one by the share at the share, never
    /// past what the others leave. The share's writes pass over the accounts, which are the
    /// instances' own, and a carve that leaves not a page is refused in words.
    #[test]
    fn an_instance_is_carved_as_the_node_carves_its_task() {
        let dir = parent("carve");
        let stage = Arc::new(Stage::create(&dir, "stage", 1 << 30, 4.0, 4).unwrap());
        enabled(&stage);
        let first = stage.enter(0, Carve::Share).unwrap();
        assert_eq!(first.limit(), 256 * MIB);
        // Measured: its account in whole pages.
        let second = stage.enter(1, Carve::Measured(100 * MIB + 1)).unwrap();
        assert_eq!(second.limit(), 100 * MIB + PAGE);
        assert_eq!(
            read(second.cgroup().dir(), "memory.max"),
            (100 * MIB + PAGE).to_string()
        );
        // Under half of what the others leave: 1024 - 256 - 100 MiB and a page, halved.
        let third = stage.enter(2, Carve::Measured(600 * MIB)).unwrap();
        assert_eq!(third.limit(), 334 * MIB - PAGE);
        // An instance's room is the ceiling less the others' limits, its own returned.
        assert_eq!(second.room(), 1024 * MIB - 256 * MIB - (334 * MIB - PAGE));
        assert_eq!(first.room(), 1024 * MIB - 434 * MIB);
        // Started again: at the account it grew to, as given.
        let fourth = stage.enter(3, Carve::Grown(64 * MIB)).unwrap();
        assert_eq!(fourth.limit(), 64 * MIB);
        assert_eq!(
            read(fourth.cgroup().dir(), "memory.max"),
            (64 * MIB).to_string()
        );
        // The ceiling moved, and the count: only the share's instance follows.
        assert_eq!(stage.write_limits(1 << 29, 4.0).unwrap(), 128 * MIB);
        assert_eq!(first.limit(), 128 * MIB);
        assert_eq!(stage.recount(2).unwrap(), 256 * MIB);
        assert_eq!(
            read(first.cgroup().dir(), "memory.max"),
            (256 * MIB).to_string()
        );
        for (place, limit) in [
            (&second, 100 * MIB + PAGE),
            (&third, 334 * MIB - PAGE),
            (&fourth, 64 * MIB),
        ] {
            assert_eq!(place.limit(), limit);
            assert_eq!(read(place.cgroup().dir(), "memory.max"), limit.to_string());
        }
        // The instances hold more than the ceiling now leaves: nothing more is carved, by any
        // rule, and the share's instance has what the others leave it.
        assert_eq!(first.room(), 512 * MIB - 498 * MIB);
        for carve in [Carve::Measured(MIB), Carve::Share] {
            let refused = stage.enter(4, carve).err().expect("refused").to_string();
            assert!(
                refused.contains(
                    "leaves 0 beside its other instances' limits, too few for \
                                  instance 4"
                ),
                "{refused}"
            );
        }
        // An instance gone returns its limit to the room.
        gone(fourth);
        gone(third);
        assert_eq!(first.room(), 512 * MIB - (100 * MIB + PAGE));
        gone(second);
        gone(first);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stage_of_one_gives_it_the_whole_ceiling() {
        let dir = parent("one");
        let stage = Stage::create(&dir, "stage", 100 * MIB + 5, 0.0, 0).unwrap();
        enabled(&stage);
        assert_eq!(count(&stage), 1);
        let stage = Arc::new(stage);
        let place = stage.enter(0, Carve::Share).unwrap();
        let at = place.cgroup().dir();
        assert_eq!(read(at, "memory.max"), (100 * MIB + 5).to_string());
        assert_eq!(read(stage.dir(), "cpu.max"), "max 100000");
        assert_eq!(stage.write_limits(7, 0.0).unwrap(), 7);
        assert_eq!(read(at, "memory.max"), "7");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stage_that_cannot_hold_cgroups_is_refused() {
        // A parent without the controllers refuses the stage's cgroup.
        let dir = std::env::temp_dir().join(format!("moruna-stage-none-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(Stage::create(&dir, "stage", 1, 1.0, 2).is_err());
        let _ = fs::remove_dir_all(&dir);
        // An instance's cgroup under a stage whose controllers were not enabled for it (the
        // fixture's file still says `+memory +cpu`, which names none) is refused.
        let dir = parent("unenabled");
        let stage = Arc::new(Stage::create(&dir, "stage", 1 << 30, 1.0, 2).unwrap());
        let refused = stage
            .enter(0, Carve::Share)
            .err()
            .expect("refused")
            .to_string();
        assert!(refused.contains("`memory` controller"), "{refused}");
        assert_eq!(stage.memory_current().unwrap(), 0, "nothing was listed");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_report_says_what_the_instances_did() {
        let busy = Arc::new(Busy::default());
        assert_eq!(
            busy.note("score", 2, 2),
            "image `score`: 2 instances, busy_peak 0 (the most holding a batch at once); \
             instance 0: 0 batches; instance 1: 0 batches"
        );
        let first = busy.hold();
        let second = busy.hold();
        drop(first);
        let third = busy.hold();
        assert_eq!(busy.peak(), 2, "never three at once");
        drop((second, third));
        let fourth = busy.hold();
        assert_eq!(busy.peak(), 2);
        drop(fourth);
        busy.answered(1);
        busy.answered(1);
        busy.answered(0);
        busy.answered(3);
        assert_eq!(
            busy.note("score", 4, 4),
            "image `score`: 4 instances, busy_peak 2 (the most holding a batch at once); \
             instance 0: 1 batches; instance 1: 2 batches; instance 2: 0 batches; \
             instance 3: 1 batches"
        );
        // Memory funded fewer than the rule asked for: said, as the shared rule says it.
        let short = Busy::default().note("score", 2, 4);
        assert!(
            short.ends_with("; score: 2 of 4 instances: the share funds 2"),
            "{short}"
        );
        // Batches past an instance's cap: how many, and the slices they were cut into.
        busy.cut(3);
        busy.cut(2);
        assert!(
            busy.note("score", 1, 1)
                .ends_with("; 2 batches past an instance's cap cut into 5 slices"),
            "{}",
            busy.note("score", 1, 1)
        );
    }

    /// 6.3f, system design 3A: a batch past what an instance takes is cut into runs of its rows,
    /// in order and every row once, each sized for nine tenths of the piece.
    #[test]
    fn a_batch_past_the_cap_is_cut_into_runs_of_its_rows() {
        // Fits, one row, or nothing bounds it: the whole batch.
        assert_eq!(cut(100, 10, 10), vec![(0, 100)]);
        assert_eq!(cut(1, 1 << 30, 10), vec![(0, 1)]);
        assert_eq!(cut(0, 1 << 30, 10), vec![(0, 0)]);
        assert_eq!(cut(100, 1 << 30, 0), vec![(0, 100)]);
        // 100 MiB at a 25 MiB piece: nine tenths of it is 22.5 MiB, so five runs of 20 rows.
        let runs = cut(100, 100 * MIB, 25 * MIB);
        assert_eq!(runs, vec![(0, 20), (20, 20), (40, 20), (60, 20), (80, 20)]);
        // Uneven: the last run takes what is left, and every row is in one run, in order.
        let runs = cut(1_000_003, 64 * MIB, 4 * MIB);
        assert_eq!(runs.len(), 18);
        let mut next = 0;
        for (at, rows) in &runs {
            assert_eq!(*at, next);
            assert!(*rows > 0);
            next += rows;
        }
        assert_eq!(next, 1_000_003);
        // More slices than rows are never made: one row each.
        assert_eq!(cut(3, 1 << 30, 1), vec![(0, 1), (1, 1), (2, 1)]);
    }
}
