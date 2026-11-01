//! KB5-T2 to KB5-T4 (SDD 20 f.18, k): memory accounts, from a process the manifest gives
//! an account of its own of 64 MiB, as an engine is given one by its supervisor.
//!
//! - `KB5 accounts`: its own account is the manifest's; an object made in an account it
//!   carved is charged its pages and its record, the account's peak is exactly that, and
//!   freeing it gives the charge back with the peak kept; mapping it charges the mapper,
//!   not the account; children carved out of an account never sum past it (a third
//!   child, a page of memory and a child larger than its parent are each refused with
//!   `QuotaExceeded`), and a child gone gives its limit back;
//! - `KB5 quota`: three children from `quota-hog`, each in an account carved out of its
//!   own: one makes memory objects until the kernel refuses one with `QuotaExceeded`, one
//!   touches zero pages until the kernel ends it with its account named, and one goes on
//!   making, writing and freeing memory while the other two have filled theirs; this
//!   process reads each account's figures, and once they are gone every limit it carved
//!   has come back to it.
//!
//! Start slots: `console`, `hog-image` (the children's program), and `supervisor`, on
//! which the supervisor sends a process resource.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use moruna_kb1_programs::{console, ms, slot};
use moruna_sys::{
    Account, Channel, Console, Error, Handle, Memory, Process, Rights, Signals, Usage, charge, map,
    now, println, wait_one,
};

moruna_sys::main!(run);

const MIB: u64 = 1 << 20;
/// What the manifest gives this process.
const OWN_LIMIT: u64 = 64 * MIB;

type Outcome = Result<(), (&'static str, Option<Error>)>;

fn own() -> Result<Usage, (&'static str, Option<Error>)> {
    Usage::own().map_err(|e| ("account_info on its own account", Some(e)))
}

fn usage(a: &Account, what: &'static str) -> Result<Usage, (&'static str, Option<Error>)> {
    a.usage().map_err(|e| (what, Some(e)))
}

fn want(ok: bool, what: &'static str) -> Outcome {
    if ok { Ok(()) } else { Err((what, None)) }
}

fn refused<T>(result: Result<T, Error>, error: Error, what: &'static str) -> Outcome {
    match result {
        Err(e) if e == error => Ok(()),
        Err(e) => Err((what, Some(e))),
        Ok(_) => Err((what, None)),
    }
}

/// KB5-T3, T4: the figures of an account, its peak, freeing, and the sum of children.
fn accounts() -> Result<(u64, u64), (&'static str, Option<Error>)> {
    let start = own()?;
    want(
        start.limit == OWN_LIMIT && start.name() == "quota",
        "its own account is not the manifest's 64 MiB `quota`",
    )?;
    let a = Account::carve(8 * MIB, "quota.a").map_err(|e| ("carving 8 MiB", Some(e)))?;
    want(
        own()?.reserved == start.reserved + 8 * MIB,
        "carving 8 MiB did not hold 8 MiB in its own account",
    )?;
    // An object made in `a` is charged exactly its pages and its record, to `a`.
    let object = charge::memory(MIB / 4096);
    let m = Memory::create_in(MIB, &a.0).map_err(|e| ("memory_create in an account", Some(e)))?;
    let u = usage(&a, "account_info")?;
    want(
        u.used == object && u.peak == object,
        "an object of 1 MiB was not charged its pages and its record exactly",
    )?;
    // Mapping it charges the mapper's account, not the one it was made in.
    let view = m
        .map(0, MIB, map::READ | map::WRITE)
        .map_err(|e| ("mapping the object", Some(e)))?;
    view.write64(0, 7)
        .map_err(|e| ("writing the object", Some(e)))?;
    want(
        usage(&a, "account_info")?.used == object,
        "mapping the object changed the account it was made in",
    )?;
    drop(view);
    drop(m);
    let u = usage(&a, "account_info")?;
    want(
        u.used == 0 && u.peak == object,
        "freeing the object did not give its charge back and keep the peak",
    )?;
    // Children never sum past their parent.
    let a1 = a
        .carve_from(4 * MIB, "quota.a1")
        .map_err(|e| ("carving 4 MiB from 8", Some(e)))?;
    let a2 = a
        .carve_from(4 * MIB, "quota.a2")
        .map_err(|e| ("carving 4 more MiB from 8", Some(e)))?;
    let u = usage(&a, "account_info")?;
    want(
        u.reserved == 8 * MIB && u.room() == 0,
        "two children of 4 MiB did not fill an account of 8",
    )?;
    refused(
        a.carve_from(4096, "quota.a3"),
        Error::QuotaExceeded,
        "a third child of a full account",
    )?;
    refused(
        Memory::create_in(4096, &a.0),
        Error::QuotaExceeded,
        "a page of memory in a full account",
    )?;
    refused(
        a1.carve_from(8 * MIB, "quota.a1.big"),
        Error::QuotaExceeded,
        "a child larger than its parent",
    )?;
    refused(
        Account::carve(own()?.room() + 4096, "quota.big"),
        Error::QuotaExceeded,
        "a child larger than what is left of its own account",
    )?;
    refused(
        Account::carve(4097, "quota.odd"),
        Error::InvalidArgs,
        "a limit that is not whole pages",
    )?;
    refused(
        Account::carve(0, "quota.zero"),
        Error::InvalidArgs,
        "a limit of nothing",
    )?;
    // A child's charges are its own: its parent holds its limit, not its use.
    let m2 = Memory::create_in(MIB, &a2.0).map_err(|e| ("memory_create in a child", Some(e)))?;
    want(
        usage(&a2, "account_info")?.used == object && usage(&a, "account_info")?.used == 0,
        "a child's charge reached its parent",
    )?;
    drop(m2);
    // A child gone gives its whole limit back.
    drop(a1);
    want(
        usage(&a, "account_info")?.reserved == 4 * MIB,
        "a child gone did not give its limit back",
    )?;
    let a3 = a
        .carve_from(4 * MIB, "quota.a3")
        .map_err(|e| ("carving into the room a child left", Some(e)))?;
    drop(a3);
    drop(a2);
    drop(a);
    want(
        own()?.reserved == start.reserved,
        "the accounts it carved did not give their limits back",
    )?;
    Ok((object, start.reserved))
}

/// The process resource the supervisor sends.
fn take_resource() -> Result<Handle, (&'static str, Option<Error>)> {
    let supervisor = Channel(slot("supervisor"));
    wait_one(&supervisor.0, Signals::READABLE, now() + ms(60_000))
        .map_err(|e| ("waiting for the process resource", Some(e)))?;
    let mut buf = [0u8; 16];
    let mut handles = [None];
    supervisor
        .read(&mut buf, &mut handles)
        .map_err(|e| ("reading the process resource", Some(e)))?;
    handles[0]
        .take()
        .ok_or(("the supervisor sent no process resource", None))
}

/// A child of `quota-hog` in an account of `limit` bytes carved out of this one's, not yet
/// started: its process, its entry, the account (a handle this process reads it by), and
/// this side of its `parent` channel.
struct Child {
    process: Process,
    entry: u64,
    account: Account,
    talk: Channel,
    theirs: Option<Channel>,
}

fn child(
    resource: &Handle,
    image: &Handle,
    name: &'static str,
    limit: u64,
) -> Result<Child, (&'static str, Option<Error>)> {
    let account =
        Account::carve(limit, name).map_err(|e| ("carving a child's account", Some(e)))?;
    let process = Process::create_in(resource, name, &account.0)
        .map_err(|e| ("process_create in a child's account", Some(e)))?;
    let entry = process
        .map_image(image)
        .map_err(|e| ("process_map_image", Some(e)))?;
    let (talk, theirs) = Channel::create().map_err(|e| ("channel_create", Some(e)))?;
    Ok(Child {
        process,
        entry,
        account,
        talk,
        theirs: Some(theirs),
    })
}

impl Child {
    fn start(&mut self, out: &Console, role: &[u8]) -> Outcome {
        let theirs = self.theirs.take().ok_or(("a child started twice", None))?;
        let console = out
            .0
            .duplicate(Rights::WRITE | Rights::TRANSFER)
            .map_err(|e| ("duplicating the console", Some(e)))?;
        if let Err((e, _)) = self.talk.write(role, []) {
            return Err(("telling a child its role", Some(e)));
        }
        let mut handles = [(Some(theirs.0), "parent"), (Some(console), "console")];
        self.process
            .start_with(self.entry, &mut handles)
            .map_err(|e| ("process_start", Some(e)))
    }

    fn report(&self, out: &mut [u8; 64]) -> Result<usize, (&'static str, Option<Error>)> {
        wait_one(&self.talk.0, Signals::READABLE, now() + ms(120_000))
            .map_err(|e| ("waiting for a child's report", Some(e)))?;
        self.talk
            .read(out, &mut [])
            .map(|(n, _)| n)
            .map_err(|e| ("reading a child's report", Some(e)))
    }

    fn ended(&self) -> Outcome {
        wait_one(&self.process.0, Signals::TERMINATED, now() + ms(120_000))
            .map(|_| ())
            .map_err(|e| ("waiting for a child to end", Some(e)))
    }
}

fn word(bytes: &[u8], at: usize) -> u64 {
    moruna_abi::read_u64(bytes, at).unwrap_or(0)
}

/// KB5-T7: a whole-area protection change and unmap must work at the exact
/// ceiling; a genuine split still needs room. Dropping an unstarted process
/// must also return all its area and page-table charges.
fn full_account_unmap(resource: &Handle) -> Outcome {
    let account = Account::carve(4 * MIB, "quota.full")
        .map_err(|e| ("carving the full-account test", Some(e)))?;
    let child = Process::create_in(resource, "quota-full", &account.0)
        .map_err(|e| ("creating the unstarted full-account process", Some(e)))?;
    let at = child
        .map(None, 0, 8192, 0, map::READ | map::WRITE)
        .map_err(|e| ("mapping the full-account area", Some(e)))?;
    let before = usage(&account, "reading the full-account baseline")?;
    let room = before.room();
    let per_page = charge::PAGE + charge::PAGE_RECORD;
    let (objects, pages) = (1..=256u64)
        .find_map(|objects| {
            let pages = room.checked_sub(objects * charge::OBJECT)?;
            (pages.is_multiple_of(per_page) && pages / per_page >= objects)
                .then_some((objects, pages / per_page))
        })
        .ok_or((
            "cannot fill the account exactly with the ABI's fixed charges",
            None,
        ))?;
    let mut held: [Option<Memory>; 256] = core::array::from_fn(|_| None);
    for i in 0..objects as usize {
        let pages = if i == 0 { pages - objects + 1 } else { 1 };
        held[i] = Some(
            Memory::create_in(pages * charge::PAGE, &account.0)
                .map_err(|e| ("filling the test account exactly", Some(e)))?,
        );
    }
    let full = usage(&account, "reading the exact full account")?;
    want(
        full.used == full.limit && full.peak == full.limit && full.room() == 0,
        "the test did not fill the account exactly",
    )?;
    child
        .protect(at, 8192, map::READ)
        .map_err(|e| ("whole-area protection at a full ceiling", Some(e)))?;
    refused(
        child.protect(at, 4096, map::READ | map::WRITE),
        Error::QuotaExceeded,
        "a real area split at a full ceiling",
    )?;
    child
        .unmap(at, 8192)
        .map_err(|e| ("freeing a whole area at a full ceiling", Some(e)))?;
    let freed = usage(&account, "reading the full account after unmap")?;
    want(
        freed.used < full.used && freed.peak == full.peak,
        "unmap did not return charges without increasing the exact peak",
    )?;
    child
        .map(None, 0, 8192, at, map::READ)
        .map_err(|e| ("remapping the released full-account area", Some(e)))?;
    drop(child);
    drop(held);
    want(
        usage(&account, "reading the dropped unstarted account")?.used == 0,
        "dropping an unstarted process leaked address-space charges",
    )
}

/// KB5-T2: a child that calls past its account is refused, one that touches past it is
/// ended with its account named, and one beside them runs on.
fn tasks(
    out: &Console,
    reserved_before: u64,
) -> Result<(u64, u64, u64), (&'static str, Option<Error>)> {
    let resource = take_resource()?;
    full_account_unmap(&resource)?;
    println!(
        out,
        "KB5 full account: ok: whole-area protection and freeing unmap at the exact ceiling; a real split refused; exact peak unchanged; unstarted process charges returned"
    );
    let image = slot("hog-image");
    let mut steady = child(&resource, &image, "quota-steady", 8 * MIB)?;
    steady.start(out, b"steady")?;

    // Calls past its account: the kernel refuses the object, and the child goes on.
    let mut call = child(&resource, &image, "quota-call", 8 * MIB)?;
    call.start(out, b"call")?;
    let mut r = [0u8; 64];
    call.report(&mut r)?;
    let (made, code, used, limit) = (word(&r, 0), word(&r, 8), word(&r, 16), word(&r, 24));
    let one = charge::memory(MIB / 4096);
    want(
        code == Error::QuotaExceeded.code() as u64,
        "the call child's last object was not refused with quota exceeded",
    )?;
    want(
        limit == 8 * MIB && made > 0 && used <= limit && used + one > limit,
        "the call child was refused with room left, or let past its limit",
    )?;
    let seen = call
        .process
        .usage()
        .map_err(|e| ("account_info through a process", Some(e)))?;
    want(
        seen.used == used && seen.name() == "quota-call",
        "the call child's account read through its process is not what it reported",
    )?;

    // Touches past its account: the kernel ends it at the first page it cannot charge.
    let mut touch = child(&resource, &image, "quota-touch", 4 * MIB)?;
    let span = 16 * MIB;
    let at = touch
        .process
        .map(None, 0, span, 0, map::READ | map::WRITE)
        .map_err(|e| ("mapping zero pages into the touch child", Some(e)))?;
    let mut role = [0u8; 24];
    role[..5].copy_from_slice(b"touch");
    role[8..16].copy_from_slice(&at.to_le_bytes());
    role[16..24].copy_from_slice(&span.to_le_bytes());
    touch.start(out, &role)?;
    touch.ended()?;
    let t = usage(&touch.account, "account_info of the touch child")?;
    want(
        t.peak <= t.limit && t.peak + charge::ZERO_PAGE > t.limit,
        "the touch child was not ended at its account's limit",
    )?;

    // Beside them, the steady child runs on, and the call child is still there.
    if let Err((e, _)) = steady.talk.write(b"go", []) {
        return Err(("telling the steady child to go", Some(e)));
    }
    steady.report(&mut r)?;
    want(
        word(&r, 0) == 32,
        "the steady child did not finish its rounds",
    )?;
    steady.ended()?;
    if let Err((e, _)) = call.talk.write(b"end", []) {
        return Err(("telling the call child to end", Some(e)));
    }
    call.ended()?;

    // Every child gone, each limit carved comes back.
    drop(steady);
    drop(call);
    drop(touch);
    want(
        own()?.reserved == reserved_before,
        "the children's accounts did not give their limits back",
    )?;
    Ok((made, used, t.peak))
}

fn failed(out: &Console, test: &str, what: &str, e: Option<Error>) {
    match e {
        Some(e) => println!(out, "{}: FAIL: {}: {}", test, what, e.name()),
        None => println!(out, "{}: FAIL: {}", test, what),
    }
}

fn run() -> i64 {
    let out = console();
    let mut ok = true;
    let reserved = match accounts() {
        Ok((object, reserved)) => {
            println!(
                out,
                "KB5 accounts: ok: its own account `quota` has the manifest's 64 MiB; an object of 1 MiB charged {} bytes to the account it was made in, whose peak read back the same and whose use went to 0 when it was freed; mapping it charged the mapper; children of 4 and 4 MiB filled an account of 8, and a third, a page of memory and a child larger than its parent were refused with quota exceeded; a child gone gave its limit back",
                object
            );
            reserved
        }
        Err((what, e)) => {
            failed(&out, "KB5 accounts", what, e);
            ok = false;
            0
        }
    };
    match tasks(&out, reserved) {
        Ok((made, used, peak)) => println!(
            out,
            "KB5 quota: ok: quota-call made {} objects of 1 MiB in its 8 MiB account ({} bytes charged) and the next was refused with quota exceeded; quota-touch wrote zero pages until the kernel ended it at its 4 MiB account's limit (peak {} bytes); quota-steady made, wrote and freed 32 MiB meanwhile and ended with 0; every limit carved came back",
            made,
            used,
            peak
        ),
        Err((what, e)) => {
            failed(&out, "KB5 quota", what, e);
            ok = false;
        }
    }
    if ok { 0 } else { 1 }
}
