// SPDX-License-Identifier: MPL-2.0

//! Host tests of the user-space swaps: the futex locks, the wait queues' sleep, RCU's
//! epochs, the fault-tolerant copy, frames, the entry frame and the clocks.

use alloc::vec;
use core::{
    sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    time::Duration,
};

use crate::{
    Error,
    arch::cpu::context::{CpuException, PageFaultErrorCode, UserContext, frame},
    backend,
    cpu::{CpuId, PinCurrentCpu, all_cpus, num_cpus},
    mm::{
        FallibleVmRead, FallibleVmWrite, Frame, FrameAllocOptions, PAGE_SIZE, Segment, Split,
        UFrame, USegment, VmIo, VmReader, VmWriter,
        dma::{DmaCoherent, DmaStream, FromDevice, ToDevice},
        frame::AnyFrameMeta,
        io::util::HasVmReaderWriter,
    },
    prelude::*,
    sync::{Mutex, RcuOption, RwLock, SpinLock, WaitQueue, Waiter, raw::RawLock},
    task::{Task, TaskOptions, disable_preempt},
    user::{ReturnReason, UserContextApi, UserMode},
};

extern crate std;

fn spawn_n(n: usize, f: impl Fn(usize) + Send + Sync + 'static) {
    let f = Arc::new(f);
    let handles: Vec<_> = (0..n)
        .map(|i| {
            let f = f.clone();
            std::thread::spawn(move || f(i))
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

#[ktest]
fn spin_lock_excludes_under_contention() {
    let lock: Arc<SpinLock<u64>> = Arc::new(SpinLock::new(0u64));
    let l = lock.clone();
    spawn_n(8, move |_| {
        for _ in 0..20_000 {
            *l.lock() += 1;
        }
    });
    assert_eq!(*lock.lock(), 160_000);
    assert!(!lock.is_locked());
    let guard = lock.lock();
    assert!(lock.is_locked());
    assert!(lock.try_lock().is_none());
    drop(guard);
    assert!(lock.try_lock().is_some());
}

#[ktest]
fn raw_lock_sleeps_and_wakes() {
    let lock = Arc::new(RawLock::new(Vec::new()));
    let held = lock.lock();
    let l = lock.clone();
    let t = std::thread::spawn(move || l.lock().push(2));
    std::thread::sleep(Duration::from_millis(20));
    drop(held);
    t.join().unwrap();
    assert_eq!(*lock.lock(), vec![2]);
}

#[ktest]
fn rwlock_readers_writers_and_upgrade() {
    let lock: Arc<RwLock<u64>> = Arc::new(RwLock::new(0u64));
    let l = lock.clone();
    spawn_n(6, move |i| {
        for _ in 0..5_000 {
            if i % 2 == 0 {
                *l.write() += 1;
            } else {
                let _ = *l.read();
            }
        }
    });
    assert_eq!(*lock.read(), 15_000);
    let up = lock.upread();
    let reader = lock.read();
    let l = lock.clone();
    let t = std::thread::spawn(move || {
        *l.upread().upgrade() += 1;
    });
    drop(reader);
    let mut w = up.upgrade();
    *w += 1;
    let up = w.downgrade();
    assert_eq!(*up, 15_001);
    let reader = lock.read();
    drop((up, reader));
    t.join().unwrap();
    assert_eq!(*lock.read(), 15_002);
}

#[ktest]
fn mutex_and_wait_queue_across_threads() {
    let m = Arc::new(Mutex::new(0u32));
    let q = Arc::new(WaitQueue::new());
    let (m2, q2) = (m.clone(), q.clone());
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(10));
        *m2.lock() = 5;
        q2.wake_all();
    });
    let v = q.wait_until(|| {
        let v = *m.lock();
        (v == 5).then_some(v)
    });
    t.join().unwrap();
    assert_eq!(v, 5);
    assert!(!q.wake_one());
}

#[ktest]
fn waiter_deadline() {
    let (waiter, waker) = Waiter::new_pair();
    let start = backend::now();
    assert!(!waiter.wait_until_deadline(start + 5_000_000));
    assert!(backend::now() - start >= 5_000_000);
    assert!(waker.wake_up());
    assert!(waiter.wait_until_deadline(backend::now() + 1_000_000_000));
    let w = waker.clone();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(5));
        w.wake_up();
    });
    assert!(waiter.wait_until_deadline(backend::now() + 5_000_000_000));
    t.join().unwrap();
}

#[ktest]
fn rcu_defers_drop_past_readers() {
    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let flag = Arc::new(AtomicBool::new(false));
    let rcu = RcuOption::new(Some(Box::new(Dropped(flag.clone()))));
    let reader = disable_preempt();
    rcu.update(None);
    // The old value must outlive the reader that may still see it.
    std::thread::sleep(Duration::from_millis(20));
    assert!(!flag.load(Ordering::SeqCst));
    drop(reader);
    let start = backend::now();
    while !flag.load(Ordering::SeqCst) {
        assert!(backend::now() - start < 5_000_000_000, "never reclaimed");
        let _ = crate::sync::reclaim();
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[ktest]
fn fallible_copy_stops_at_a_fault() {
    let user = vec![7u8; 3 * PAGE_SIZE];
    let base = user.as_ptr() as usize;
    backend::fault_range(base + PAGE_SIZE..base + 2 * PAGE_SIZE);
    // SAFETY: the buffer is valid; the marked page stands for an unmapped one.
    let mut reader = unsafe { VmReader::from_user_space(user.as_ptr(), user.len()) };
    let mut out = vec![0u8; 3 * PAGE_SIZE];
    let mut writer = VmWriter::from(out.as_mut_slice());
    let (err, copied) = reader.read_fallible(&mut writer).unwrap_err();
    assert_eq!(err, Error::PageFault);
    assert_eq!(copied, PAGE_SIZE);
    assert!(out[..PAGE_SIZE].iter().all(|b| *b == 7));
    assert!(out[PAGE_SIZE..].iter().all(|b| *b == 0));

    let mut target = vec![0u8; 3 * PAGE_SIZE];
    let tbase = target.as_mut_ptr() as usize;
    backend::fault_range(tbase + 2 * PAGE_SIZE..tbase + 3 * PAGE_SIZE);
    // SAFETY: as above.
    let mut writer = unsafe { VmWriter::from_user_space(target.as_mut_ptr(), target.len()) };
    let src = vec![9u8; 3 * PAGE_SIZE];
    let mut reader = VmReader::from(src.as_slice());
    let (_, written) = writer.write_fallible(&mut reader).unwrap_err();
    assert_eq!(written, 2 * PAGE_SIZE);
    // SAFETY: as above.
    let mut writer = unsafe { VmWriter::from_user_space(target.as_mut_ptr(), target.len()) };
    assert_eq!(
        writer.fill_zeros(3 * PAGE_SIZE),
        Err((Error::PageFault, 2 * PAGE_SIZE))
    );

    let word = AtomicU32::new(5);
    let wptr = word.as_ptr();
    // SAFETY: as above.
    let reader = unsafe { VmReader::from_user_space(wptr as *const u8, 4) };
    assert_eq!(reader.atomic_load::<u32>(), Ok(5));
    // SAFETY: as above.
    let writer = unsafe { VmWriter::from_user_space(wptr as *mut u8, 4) };
    assert_eq!(
        writer.atomic_compare_exchange(&reader, 5u32, 6u32),
        Ok((5, true))
    );
    assert_eq!(word.load(Ordering::SeqCst), 6);
    backend::fault_range(wptr as usize..wptr as usize + 4);
    assert_eq!(reader.atomic_load::<u32>(), Err(Error::PageFault));
}

#[ktest]
fn user_to_user_copy() {
    let src = vec![3u8; 100];
    let mut dst = vec![0u8; 100];
    // SAFETY: both buffers are valid.
    let mut reader = unsafe { VmReader::from_user_space(src.as_ptr(), 100) };
    // SAFETY: as above.
    let mut writer = unsafe { VmWriter::from_user_space(dst.as_mut_ptr(), 100) };
    assert_eq!(reader.read_fallible(&mut writer), Ok(100));
    assert_eq!(dst, src);
}

#[derive(Debug, Default)]
struct Meta(AtomicUsize);
crate::impl_untyped_frame_meta_for!(Meta);

#[derive(Debug)]
struct Typed;
crate::impl_frame_meta_for!(Typed);

#[ktest]
fn frames_and_segments() {
    let frame = FrameAllocOptions::new().alloc_frame().unwrap();
    assert_eq!(frame.size(), PAGE_SIZE);
    assert!(frame.paddr().is_multiple_of(PAGE_SIZE));
    frame.write_val(8, &0x1234u32).unwrap();
    assert_eq!(frame.read_val::<u32>(8).unwrap(), 0x1234);
    assert_eq!(frame.read_val::<u32>(0).unwrap(), 0);
    let clone = frame.clone();
    assert_eq!(clone.reference_count(), 2);
    assert_eq!(clone, frame);
    let ufr: UFrame = clone.into();
    assert_eq!(ufr.reader().remain(), PAGE_SIZE);

    let with = FrameAllocOptions::new()
        .alloc_frame_with(Meta::default())
        .unwrap();
    with.meta().0.store(3, Ordering::Relaxed);
    let any: Frame<dyn AnyFrameMeta> = with.clone().into();
    assert!(any.dyn_meta().is_untyped());
    let back: Frame<Meta> = any.try_into().unwrap();
    assert_eq!(back.meta().0.load(Ordering::Relaxed), 3);
    let any: Frame<dyn AnyFrameMeta> = back.into();
    assert!(Frame::<Typed>::try_from(any).is_err());
    let typed = FrameAllocOptions::new().alloc_frame_with(Typed).unwrap();
    assert!(!typed.dyn_meta().is_untyped());

    let seg = FrameAllocOptions::new()
        .zeroed(false)
        .alloc_segment(4)
        .unwrap();
    assert_eq!(seg.size(), 4 * PAGE_SIZE);
    seg.write_bytes(PAGE_SIZE, &[1, 2, 3]).unwrap();
    let mid = seg.slice(&(PAGE_SIZE..3 * PAGE_SIZE));
    assert_eq!(mid.paddr(), seg.paddr() + PAGE_SIZE);
    let mut buf = [0u8; 3];
    mid.read_bytes(0, &mut buf).unwrap();
    assert_eq!(buf, [1, 2, 3]);
    let (a, b) = seg.clone().split(PAGE_SIZE);
    assert_eq!((a.size(), b.size()), (PAGE_SIZE, 3 * PAGE_SIZE));
    assert_eq!(seg.paddr_range().len(), 4 * PAGE_SIZE);
    let useg: USegment = seg.clone().into();
    assert_eq!(useg.into_iter().count(), 4);
    let dseg: Segment<dyn AnyFrameMeta> = seg.into();
    assert_eq!(dseg.size(), 4 * PAGE_SIZE);
    let one: Segment<()> = frame.into();
    assert_eq!(one.size(), PAGE_SIZE);
    let metas = FrameAllocOptions::new()
        .alloc_segment_with(2, |_| Meta::default())
        .unwrap();
    assert_eq!(metas.into_iter().count(), 2);
    assert!(FrameAllocOptions::new().alloc_segment(0).is_err());
}

#[ktest]
fn dma_buffers_are_memory() {
    let stream = DmaStream::<ToDevice>::alloc(2, false).unwrap();
    assert!(stream.writer().is_ok());
    assert!(stream.reader().is_err());
    assert!(stream.sync_to_device(0..PAGE_SIZE).is_ok());
    assert!(stream.sync_from_device(0..PAGE_SIZE).is_err());
    assert!(stream.sync_to_device(0..3 * PAGE_SIZE).is_err());
    let (a, b) = stream.split(PAGE_SIZE);
    assert_eq!(a.size() + b.size(), 2 * PAGE_SIZE);
    let from = DmaStream::<FromDevice>::alloc_uninit(1, false).unwrap();
    assert!(from.reader().is_ok());
    let coherent = DmaCoherent::alloc(1, true).unwrap();
    use crate::mm::HasDaddr;
    assert_eq!(coherent.daddr(), coherent.paddr());
    coherent.write_val(0, &7u64).unwrap();
    assert_eq!(coherent.read_val::<u64>(0).unwrap(), 7);
    let (c, d) = DmaCoherent::alloc_uninit(2, true).unwrap().split(PAGE_SIZE);
    assert_eq!(c.size(), d.size());
}

#[ktest]
fn entry_frame_round_trip() {
    let mut ctx = UserContext::default();
    ctx.set_rax(39);
    ctx.set_rdi(1);
    ctx.set_r15(15);
    ctx.set_instruction_pointer(0x40_1000);
    ctx.set_stack_pointer(0x7fff_0000);
    ctx.set_fs_base(0x1000);
    ctx.set_gs_base(0x2000);
    ctx.set_rflags(0x246 | 0x400);
    let mut f = [0u8; frame::LEN];
    ctx.to_frame(&mut f);
    let mut back = UserContext::default();
    f[frame::REASON..frame::REASON + 8].copy_from_slice(&frame::reason::SYSCALL.to_le_bytes());
    assert_eq!(back.from_frame(&f), ReturnReason::UserSyscall);
    assert_eq!(back.general_regs(), ctx.general_regs());
    assert_eq!(back.rip(), 0x40_1000);
    assert_eq!(back.fs_base(), 0x1000);
    assert_eq!(back.gs_base(), 0x2000);
    assert_eq!(back.rflags() & 0x400, 0x400);

    f[frame::REASON..frame::REASON + 8].copy_from_slice(&frame::reason::PAGE_FAULT.to_le_bytes());
    f[frame::VECTOR..frame::VECTOR + 8].copy_from_slice(&14u64.to_le_bytes());
    f[frame::ERROR_CODE..frame::ERROR_CODE + 8].copy_from_slice(&6u64.to_le_bytes());
    f[frame::FAULT_ADDRESS..frame::FAULT_ADDRESS + 8]
        .copy_from_slice(&0xdead_0000u64.to_le_bytes());
    assert_eq!(back.from_frame(&f), ReturnReason::UserException);
    match back.take_exception() {
        Some(CpuException::PageFault(info)) => {
            assert_eq!(info.addr, 0xdead_0000);
            assert!(
                info.error_code
                    .contains(PageFaultErrorCode::WRITE | PageFaultErrorCode::USER)
            );
        }
        other => panic!("{other:?}"),
    }
    f[frame::REASON..frame::REASON + 8].copy_from_slice(&frame::reason::INTERRUPTED.to_le_bytes());
    assert_eq!(back.from_frame(&f), ReturnReason::KernelEvent);
    for v in 0..32 {
        let _ = CpuException::new(v, 0, 0);
    }
    assert!(CpuException::new(99, 0, 0).is_none());
    assert!(matches!(
        CpuException::new(13, 0, 0),
        Some(CpuException::GeneralProtectionFault(None))
    ));
}

#[ktest]
fn program_driver_runs_the_program() {
    fn driver(ctx: &mut UserContext) -> ReturnReason {
        // A program that makes one call: `rax` 60.
        ctx.set_rax(60);
        ReturnReason::UserSyscall
    }
    crate::user::set_program_driver(driver);
    let mut mode = UserMode::new(UserContext::default());
    assert_eq!(
        mode.execute(&crate::user::DummyUserHooks),
        ReturnReason::UserSyscall
    );
    assert_eq!(mode.context().rax(), 60);
    mode.context_mut().set_rax(0);
    assert_eq!(mode.context().rax(), 0);
}

#[ktest]
fn fpu_context_saves_and_loads() {
    use crate::arch::cpu::context::FpuContext;
    let mut fpu = FpuContext::new();
    fpu.save();
    let copy = fpu.clone();
    assert_eq!(copy.as_bytes(), fpu.as_bytes());
    fpu.load();
    assert!(fpu.as_bytes().len() >= 512);
}

crate::cpu_local! {
    static PER_CPU: AtomicUsize = AtomicUsize::new(0);
}

crate::cpu_local_cell! {
    static CELL: u8 = 0;
    static FLAG: bool = false;
}

#[ktest]
fn cpu_local_state() {
    assert!(num_cpus() >= 1);
    assert_eq!(all_cpus().count(), num_cpus());
    let guard = disable_preempt();
    assert_eq!(guard.current_cpu(), CpuId::bsp());
    PER_CPU.get_with(&guard).fetch_add(1, Ordering::Relaxed);
    assert!(PER_CPU.get_on_cpu(CpuId::bsp()).load(Ordering::Relaxed) >= 1);
    drop(guard);
    CELL.store(250);
    CELL.add_assign(10);
    assert_eq!(CELL.load(), 4);
    CELL.sub_assign(5);
    assert_eq!(CELL.load(), 255);
    CELL.bitand_assign(0x0f);
    CELL.bitor_assign(0x30);
    CELL.bitxor_assign(0x01);
    assert_eq!(CELL.load(), 0x3e);
    FLAG.store(true);
    assert!(FLAG.load());
    let boxed = crate::alloc_cpu_local(|cpu| u32::from(cpu) + 1).unwrap();
    assert_eq!(*boxed.get_on_cpu(CpuId::bsp()), 1);
}

#[ktest]
fn ticks_and_jiffies() {
    use crate::timer::{Jiffies, register_callback_on_cpu, tick};
    let ticks = Arc::new(AtomicUsize::new(0));
    let t = ticks.clone();
    register_callback_on_cpu(move || {
        t.fetch_add(1, Ordering::Relaxed);
    });
    tick();
    assert!(ticks.load(Ordering::Relaxed) >= 1);
    let start = backend::now();
    while ticks.load(Ordering::Relaxed) < 3 {
        assert!(
            backend::now() - start < 5_000_000_000,
            "the ticker never ran"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let a = Jiffies::elapsed().as_u64();
    std::thread::sleep(Duration::from_millis(3));
    assert!(Jiffies::elapsed().as_u64() > a);
    assert_eq!(
        Jiffies::new(1500).as_duration(),
        Duration::from_millis(1500)
    );
    assert!(crate::timer::monotonic_nanos() > 0);
}

#[ktest]
fn tasks_are_threads() {
    let done = Arc::new(AtomicBool::new(false));
    let d = done.clone();
    let task = TaskOptions::new(move || {
        assert!(Task::current().is_some());
        assert!(!Task::need_yield());
        Task::yield_now();
        d.store(true, Ordering::Release);
    })
    .local_data(5u8)
    .spawn()
    .unwrap();
    let start = backend::now();
    while !done.load(Ordering::Acquire) {
        assert!(backend::now() - start < 5_000_000_000);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(task.schedule_info().cpu.get().is_none());
    task.schedule_info().cpu.set_anyway(CpuId::bsp());
    assert_eq!(task.schedule_info().cpu.get(), Some(CpuId::bsp()));
    let entered = Arc::new(TaskOptions::new(|| {}).local_data(9u32).build().unwrap());
    Task::enter(entered);
    assert_eq!(
        Task::current().unwrap().local_data().downcast_ref::<u32>(),
        Some(&9)
    );
    Task::leave();
}

#[ktest]
fn boot_info_and_power_types() {
    use crate::boot::{BootInfo, boot_info, memory_region::*, try_boot_info};
    // The test runner gave the start's information first; a second one is ignored.
    crate::boot::init(BootInfo::default());
    assert!(try_boot_info().is_some());
    assert_eq!(boot_info().memory_regions[0].len(), 1 << 30);
    assert!(crate::mm::frame::free_bytes() > 0);
    let region = MemoryRegion::new(0x1000, 0x2000, MemoryRegionType::Usable);
    assert_eq!(region.base(), 0x1000);
    assert!(!region.is_empty());
    assert_eq!(region.typ(), MemoryRegionType::Usable);
    assert_eq!(crate::arch::trap::USER_CS_VALUE, 0x33);
    assert!(crate::arch::read_tsc() > 0);
    assert!(crate::arch::read_random().is_some());
    let _ = crate::arch::tsc_freq();
    crate::early_println!("early console: {}", 1);
    crate::console::send(b"raw bytes\n");
    crate::log::init(crate::log::LevelFilter::Info);
    crate::info!("logged");
    let irq = crate::irq::disable_local();
    assert!(crate::irq::InterruptLevel::current().is_task_context());
    assert!(!crate::irq::InterruptLevel::current().is_interrupt_context());
    drop(irq);
}

#[ktest]
fn rw_mutex_paths() {
    use crate::sync::RwMutex;
    let mut m = RwMutex::new(1u32);
    *m.get_mut() = 2;
    {
        let r1 = m.read();
        let r2 = m.try_read().unwrap();
        assert_eq!(*r1 + *r2, 4);
        assert!(m.try_write().is_none());
        let up = m.try_upread().unwrap();
        assert!(m.try_upread().is_none());
        drop((r1, r2));
        let mut w = up.upgrade();
        *w = 3;
        let up = w.downgrade();
        assert_eq!(*up, 3);
    }
    *m.write() += 1;
    assert_eq!(*m.upread(), 4);
    let m = Arc::new(m);
    let m2 = m.clone();
    spawn_n(4, move |_| {
        for _ in 0..1_000 {
            *m2.write() += 1;
            let _ = *m2.read();
        }
    });
    assert_eq!(*m.read(), 4_004);
}

#[ktest]
fn rcu_cells() {
    use crate::sync::{Rcu, RcuDrop};
    let rcu = Rcu::new(Box::new(1u32));
    assert_eq!(**rcu.read().get(), 1);
    rcu.update(Box::new(2));
    let guard = rcu.read();
    assert_eq!(**guard.get(), 2);
    assert!(guard.compare_exchange(Box::new(3)).is_ok());
    let preempt = disable_preempt();
    assert_eq!(**rcu.read_with(&preempt), 3);
    drop(preempt);
    let opt: RcuOption<Box<u32>> = RcuOption::new_none();
    assert!(opt.read().is_none());
    assert!(opt.read().compare_exchange(Some(Box::new(4))).is_ok());
    let preempt = disable_preempt();
    assert_eq!(opt.read_with(&preempt).map(|b| **b), Some(4));
    drop(preempt);
    assert_eq!(opt.read().get().map(|b| **b), Some(4));
    let delayed = RcuDrop::new(vec![1u8, 2]);
    assert_eq!(delayed.len(), 2);
    drop(delayed);
    let _ = crate::sync::reclaim();
}

#[ktest]
fn readers_and_writers_over_memory() {
    let frame = FrameAllocOptions::new().alloc_frame().unwrap();
    frame.write_slice(0, &[1u16, 2, 3]).unwrap();
    let mut out = [0u16; 3];
    frame.read_slice(0, &mut out).unwrap();
    assert_eq!(out, [1, 2, 3]);
    use crate::mm::VmIoOnce;
    frame.write_once(16, &9u64).unwrap();
    assert_eq!(frame.read_once::<u64>(16).unwrap(), 9);
    use crate::mm::VmIoFill;
    assert_eq!(frame.fill_zeros(0, 8), Ok(()));
    assert_eq!(frame.read_val::<u64>(0).unwrap(), 0);
    assert!(frame.read_val::<u64>(PAGE_SIZE).is_err());
    assert!(frame.write_bytes(PAGE_SIZE - 1, &[0, 0]).is_err());
    assert!(frame.fill_zeros(PAGE_SIZE, 1).is_err());

    let mut reader = frame.reader();
    reader.skip(16);
    assert_eq!(reader.read_val::<u64>().unwrap(), 9);
    assert!(reader.has_remain());
    reader.limit(4);
    assert_eq!(reader.remain(), 4);
    assert!(!reader.cursor().is_null());
    assert_eq!(reader.read_once::<u32>().unwrap(), 0);
    assert!(reader.read_val::<u32>().is_err());
    let reader2 = reader.clone();
    assert_eq!(reader2.remain(), 0);

    let mut buf = [0u8; 32];
    let mut writer = VmWriter::from(&mut buf[..]);
    writer.write_val(&0xffu8).unwrap();
    writer.write_once(&0xeeu8).unwrap();
    writer.skip(2);
    assert_eq!(writer.avail(), 28);
    assert!(writer.has_avail());
    assert!(!writer.cursor().is_null());
    {
        let mut inner = writer.clone_exclusive();
        inner.limit(4);
        assert_eq!(inner.fill_zeros(100), 4);
    }
    let mut src = VmReader::from(&[5u8; 8][..]);
    assert_eq!(writer.write(&mut src), 8);
    let mut fallible = writer.to_fallible();
    fallible.write_val(&7u32).unwrap();
    assert_eq!(fallible.fill_zeros(4), Ok(4));
    assert_eq!(buf[0], 0xff);
    assert_eq!(buf[1], 0xee);
    // The exclusive clone's zeros do not move the writer: its bytes come next.
    assert_eq!(buf[4..12], [5u8; 8]);
    assert_eq!(buf[12..16], 7u32.to_ne_bytes());

    let data = [1u8, 0, 0, 0, 2, 0, 0, 0];
    let mut fr = VmReader::from(&data[..]).to_fallible();
    assert_eq!(fr.read_val::<u32>().unwrap(), 1);
    let mut small = [0u8; 2];
    let mut sw = VmWriter::from(&mut small[..]);
    assert_eq!(VmReader::from(&data[..]).read(&mut sw), 2);
    assert!(fr.read_val::<u64>().is_err());

    let user = vec![0u8; 8];
    backend::fault_range(user.as_ptr() as usize..user.as_ptr() as usize + 8);
    // SAFETY: the buffer is valid; it is marked as unmapped.
    let mut ur = unsafe { VmReader::from_user_space(user.as_ptr(), 8) };
    assert_eq!(ur.read_val::<u64>(), Err(Error::PageFault));
    assert_eq!(ur.remain(), 8);
    let mut user2 = vec![0u8; 8];
    backend::fault_range(user2.as_ptr() as usize..user2.as_ptr() as usize + 8);
    // SAFETY: as above.
    let mut uw = unsafe { VmWriter::from_user_space(user2.as_mut_ptr(), 8) };
    assert_eq!(uw.write_val(&1u64), Err(Error::PageFault));
    assert_eq!(uw.avail(), 8);
}

#[ktest]
fn atomic_cpu_ids_and_either() {
    use crate::{task::AtomicCpuId, util::Either};
    let id = AtomicCpuId::default();
    assert!(id.set_if_is_none(CpuId::bsp()).is_ok());
    assert_eq!(id.set_if_is_none(CpuId::bsp()), Err(CpuId::bsp()));
    id.set_to_none();
    assert_eq!(id.get(), None);
    let l: Either<u8, u16> = Either::Left(1);
    assert!(l.is_left() && !l.is_right());
    assert_eq!(l.left(), Some(1));
    let r: Either<u8, u16> = Either::Right(2);
    assert_eq!(r.right(), Some(2));
    assert_eq!(Either::<u8, u16>::Left(1).right(), None);
    assert_eq!(Either::<u8, u16>::Right(2).left(), None);
    assert_eq!(CpuId::try_from(0usize).ok(), Some(CpuId::bsp()));
    assert!(CpuId::try_from(10_000usize).is_err());
    assert_eq!(u32::from(CpuId::new(0)), 0);
    crate::task::atomic_mode::might_sleep();
}

mod vm {
    use crate::{
        Error,
        mm::{
            CachePolicy, FallibleVmRead, FallibleVmWrite, FrameAllocOptions, PAGE_SIZE, PageFlags,
            PageProperty, UFrame, VmIo, VmReader, VmSpace, VmWriter,
            tlb::{TlbFlushOp, TlbFlusher},
            vm_space::VmQueriedItem,
        },
        prelude::*,
        task::disable_preempt,
    };

    fn rw() -> PageProperty {
        PageProperty::new_user(PageFlags::RW, CachePolicy::Writeback)
    }

    fn frame() -> UFrame {
        FrameAllocOptions::new().alloc_frame().unwrap().into()
    }

    #[ktest]
    fn cursors_map_query_protect_unmap() {
        let space = VmSpace::new();
        let guard = disable_preempt();
        let range = PAGE_SIZE..16 * PAGE_SIZE;
        assert!(space.cursor(&guard, &(1..PAGE_SIZE)).is_err());
        assert!(space.cursor(&guard, &(PAGE_SIZE..PAGE_SIZE)).is_err());
        let first = frame();
        {
            let mut c = space.cursor_mut(&guard, &range).unwrap();
            let (hole, item) = c.query().unwrap();
            assert!(item.is_none());
            assert_eq!(hole, range.clone());
            c.jump(4 * PAGE_SIZE).unwrap();
            c.map(first.clone(), rw());
            assert_eq!(c.virt_addr(), 5 * PAGE_SIZE);
            c.map(
                frame(),
                PageProperty::new_guest(PageFlags::R, CachePolicy::Writeback),
            );
            assert!(c.jump(32 * PAGE_SIZE).is_err());
            c.jump(PAGE_SIZE).unwrap();
            assert_eq!(c.find_next(8 * PAGE_SIZE), Some(4 * PAGE_SIZE));
            match c.query().unwrap() {
                (r, Some(VmQueriedItem::MappedRam { frame, prop })) => {
                    assert_eq!(r, 4 * PAGE_SIZE..5 * PAGE_SIZE);
                    assert_eq!(frame.paddr(), first.paddr());
                    assert_eq!(prop.flags, PageFlags::RW);
                }
                _ => panic!("no frame"),
            }
            assert!(c.find_iomem_by_paddr(0).is_none());
            let r = c.protect_next(PAGE_SIZE, |flags, _| *flags = PageFlags::R);
            assert_eq!(r, Some(4 * PAGE_SIZE..5 * PAGE_SIZE));
            c.jump(10 * PAGE_SIZE).unwrap();
            assert_eq!(c.protect_next(2 * PAGE_SIZE, |_, _| {}), None);
            assert_eq!(c.virt_addr(), 12 * PAGE_SIZE);
            c.flusher()
                .issue_tlb_flush(TlbFlushOp::for_single(4 * PAGE_SIZE));
            c.flusher().dispatch_tlb_flush();
        }
        assert_eq!(space.mapped_pages(), 2);
        {
            let mut c = space.cursor(&guard, &range).unwrap();
            c.jump(4 * PAGE_SIZE).unwrap();
            assert_eq!(c.virt_addr(), 4 * PAGE_SIZE);
            assert_eq!(c.query().unwrap().1.unwrap().prop().flags, PageFlags::R);
            assert_eq!(c.find_next(PAGE_SIZE), Some(4 * PAGE_SIZE));
            c.jump(6 * PAGE_SIZE).unwrap();
            assert_eq!(c.find_next(PAGE_SIZE), None);
            assert!(c.jump(64 * PAGE_SIZE).is_err());
        }
        {
            let mut c = space.cursor_mut(&guard, &range).unwrap();
            assert_eq!(c.unmap(15 * PAGE_SIZE), 2);
            assert_eq!(c.virt_addr(), 16 * PAGE_SIZE);
            assert!(c.query().is_err());
        }
        assert_eq!(space.mapped_pages(), 0);
        assert_eq!(VmSpace::default().mapped_pages(), 0);
        assert!(alloc::format!("{space:?}").contains("VmSpace"));
    }

    #[ktest]
    #[should_panic(expected = "already mapped")]
    fn mapping_twice_panics() {
        let space = VmSpace::new();
        let guard = disable_preempt();
        let mut c = space
            .cursor_mut(&guard, &(PAGE_SIZE..4 * PAGE_SIZE))
            .unwrap();
        c.map(frame(), rw());
        c.jump(PAGE_SIZE).unwrap();
        c.map(frame(), rw());
    }

    // The fault path: a write to a program page nothing maps yet asks the handler, which
    // maps one, and the copy goes on.
    fn map_on_fault(e: &crate::arch::cpu::context::CpuException) -> core::result::Result<(), ()> {
        let crate::arch::cpu::context::CpuException::PageFault(info) = e else {
            return Err(());
        };
        if info.addr >= 64 * PAGE_SIZE {
            return Err(());
        }
        let space = ACTIVE.get().ok_or(())?;
        let guard = disable_preempt();
        let page = info.addr & !(PAGE_SIZE - 1);
        let mut c = space
            .cursor_mut(&guard, &(page..page + PAGE_SIZE))
            .map_err(|_| ())?;
        // A page already there was refused for its property: nothing to map.
        if c.query().map_err(|_| ())?.1.is_some() {
            return Err(());
        }
        c.map(frame(), rw());
        Ok(())
    }

    static ACTIVE: spin::Once<Arc<VmSpace>> = spin::Once::new();

    #[ktest]
    fn program_memory_through_the_active_space() {
        let space = ACTIVE.call_once(|| Arc::new(VmSpace::new())).clone();
        space.activate();
        crate::mm::fault::inject_user_page_fault_handler(map_on_fault);
        let data = [7u8; 3 * PAGE_SIZE];
        // Two pages from 8: mapped on fault, then written.
        let mut w = space.writer(8 * PAGE_SIZE + 100, 2 * PAGE_SIZE).unwrap();
        let mut r = VmReader::from(&data[..2 * PAGE_SIZE]);
        assert_eq!(w.write_fallible(&mut r), Ok(2 * PAGE_SIZE));
        let mut back = [0u8; 2 * PAGE_SIZE];
        let mut r = space.reader(8 * PAGE_SIZE + 100, 2 * PAGE_SIZE).unwrap();
        let mut bw = VmWriter::from(&mut back[..]);
        assert_eq!(r.read_fallible(&mut bw), Ok(2 * PAGE_SIZE));
        assert_eq!(back, [7u8; 2 * PAGE_SIZE]);
        // The handler refuses past page 64: a fault there stays a fault.
        let mut w = space.writer(70 * PAGE_SIZE, 8).unwrap();
        assert_eq!(w.write_val(&1u64), Err(Error::PageFault));
        let mut z = space.writer(8 * PAGE_SIZE, 16).unwrap();
        assert_eq!(z.fill_zeros(16), Ok(16));
        let word = space.reader(8 * PAGE_SIZE, 4).unwrap();
        assert_eq!(word.atomic_load::<u32>(), Ok(0));
        // A read-only page: a write faults, the handler cannot map over it.
        let guard = disable_preempt();
        {
            let mut c = space
                .cursor_mut(&guard, &(20 * PAGE_SIZE..21 * PAGE_SIZE))
                .unwrap();
            let f = frame();
            f.write_val(0, &5u32).unwrap();
            c.map(
                f,
                PageProperty::new_user(PageFlags::R, CachePolicy::Writeback),
            );
        }
        drop(guard);
        let mut w = space.writer(20 * PAGE_SIZE, 4).unwrap();
        assert_eq!(w.write_val(&1u32), Err(Error::PageFault));
        let mut r = space.reader(20 * PAGE_SIZE, 4).unwrap();
        assert_eq!(r.read_val::<u32>(), Ok(5));
        let (mut r2, _w2) = space.reader_writer(20 * PAGE_SIZE, 4).unwrap();
        assert_eq!(r2.read_val::<u32>(), Ok(5));
        assert!(space.reader(usize::MAX - 8, 4).is_err());
        assert!(space.writer(usize::MAX - 8, 4).is_err());
        assert!(space.reader_writer(usize::MAX - 8, 4).is_err());
        let cmp = space.writer(8 * PAGE_SIZE, 4).unwrap();
        let cmp_r = space.reader(8 * PAGE_SIZE, 4).unwrap();
        assert_eq!(
            cmp.atomic_compare_exchange(&cmp_r, 0u32, 9u32),
            Ok((0, true))
        );
    }

    #[ktest]
    fn tlb_flushes_and_ops() {
        assert_eq!(TlbFlushOp::for_range(0..8).range(), &(0..8));
        assert_eq!(
            TlbFlushOp::for_single(PAGE_SIZE).range(),
            &(PAGE_SIZE..2 * PAGE_SIZE)
        );
        assert_eq!(TlbFlushOp::for_all().range().start, 0);
        TlbFlushOp::for_all().perform_on_current();
        let mut f = TlbFlusher::new(disable_preempt());
        f.issue_tlb_flush(TlbFlushOp::for_all());
        let fr: crate::mm::Frame<dyn crate::mm::AnyFrameMeta> =
            FrameAllocOptions::new().alloc_frame().unwrap().into();
        f.issue_tlb_flush_with(TlbFlushOp::for_all(), crate::sync::RcuDrop::new(fr));
        f.sync_tlb_flush();
    }
}

#[ktest]
fn frames_as_rcu_pointers() {
    use crate::{mm::frame::FrameRef, sync::non_null::NonNullPtr};
    let frame = FrameAllocOptions::new()
        .alloc_frame_with(Meta::default())
        .unwrap();
    let paddr = frame.paddr();
    let raw = NonNullPtr::into_raw(frame.clone());
    assert_eq!(frame.reference_count(), 2);
    // SAFETY: `raw` came from `into_raw` and its count is still held.
    let r: FrameRef<'_, Meta> = unsafe { <Frame<Meta> as NonNullPtr>::raw_as_ref(raw) };
    assert_eq!(r.paddr(), paddr);
    assert!(alloc::format!("{r:?}").contains("Frame"));
    let again = <Frame<Meta> as NonNullPtr>::ref_as_raw(r);
    assert_eq!(again, raw);
    // SAFETY: as above, once.
    let back: Frame<Meta> = unsafe { NonNullPtr::from_raw(raw) };
    assert_eq!(back.reference_count(), 2);
    drop(back);
    assert_eq!(frame.reference_count(), 1);
}

#[ktest]
fn panic_power_io_and_scheduler_stand_ins() {
    use crate::{
        io::IoMem,
        task::scheduler::{
            EnqueueFlags, LocalRunQueue, Scheduler, UpdateFlags, enable_preemption_on_cpu,
            inject_scheduler,
        },
    };
    assert_eq!(crate::panic::catch_unwind(|| 5).ok(), Some(5));
    crate::panic::print_stack_trace();
    assert!(IoMem::acquire(0..PAGE_SIZE).is_err());
    fn on_power(_: crate::power::ExitCode) {}
    crate::power::inject_poweroff_handler(on_power);
    crate::power::inject_restart_handler(on_power);
    struct Nothing;
    impl Scheduler for Nothing {
        fn enqueue(&self, _: Arc<Task>, _: EnqueueFlags) -> Option<CpuId> {
            None
        }
        fn local_rq_with(&self, _: &mut dyn FnMut(&dyn LocalRunQueue)) {}
        fn mut_local_rq_with(&self, _: &mut dyn FnMut(&mut dyn LocalRunQueue)) {}
    }
    static NOTHING: Nothing = Nothing;
    inject_scheduler(&NOTHING);
    enable_preemption_on_cpu();
    assert_ne!(EnqueueFlags::Spawn, EnqueueFlags::Wake);
    assert_ne!(UpdateFlags::Tick, UpdateFlags::Exit);
    // A bottom half runs on each tick.
    static HALVES: AtomicUsize = AtomicUsize::new(0);
    fn half(g: crate::irq::DisabledLocalIrqGuard, _: u8) -> crate::irq::DisabledLocalIrqGuard {
        HALVES.fetch_add(1, Ordering::Relaxed);
        g
    }
    crate::irq::register_bottom_half_handler_l2(|_| {});
    crate::irq::register_bottom_half_handler_l1(half);
    crate::timer::tick();
    assert!(HALVES.load(Ordering::Relaxed) >= 1);
    // A tick's callbacks run at an interrupt level, and task context comes back after.
    static LEVELS: AtomicUsize = AtomicUsize::new(0);
    crate::timer::register_callback_on_cpu(|| {
        if crate::irq::InterruptLevel::current()
            == crate::irq::InterruptLevel::L1(crate::cpu::PrivilegeLevel::Kernel)
        {
            LEVELS.fetch_add(1, Ordering::Relaxed);
        }
        assert_eq!(
            crate::irq::as_interrupt(crate::irq::InterruptLevel::current),
            crate::irq::InterruptLevel::L2
        );
    });
    crate::timer::tick();
    assert!(LEVELS.load(Ordering::Relaxed) >= 1);
    assert!(crate::irq::InterruptLevel::current().is_task_context());
    crate::task::halt_cpu();
    let _ = CpuException::new(14, 2, 0x1000);
}

mod shared_memory {
    //! The task's shared memory (`backend/shared.rs`) on a core that records each
    //! process's mappings.

    use alloc::{collections::BTreeMap, vec};
    use core::sync::atomic::{AtomicUsize, Ordering};

    use crate::{
        Error,
        backend::{Bump, Core, Processes, TaskMemory, map_everywhere},
        mm::PAGE_SIZE,
        prelude::*,
    };

    /// A core whose processes are numbers, each with its mappings by address.
    #[derive(Default)]
    struct Recorder {
        mapped: BTreeMap<u32, BTreeMap<usize, (u32, usize)>>,
        next_object: u32,
        objects_alive: Arc<AtomicUsize>,
        refuse_map_in: Option<u32>,
        refuse_create: bool,
    }

    /// An object: its number, counted alive until dropped.
    struct Object(u32, Arc<AtomicUsize>);

    impl Drop for Object {
        fn drop(&mut self) {
            self.1.fetch_sub(1, Ordering::Relaxed);
        }
    }

    impl Core for Recorder {
        type Process = u32;
        type Object = Object;

        fn create(&mut self, _len: usize) -> Result<Object> {
            if self.refuse_create {
                return Err(Error::NoMemory);
            }
            self.next_object += 1;
            self.objects_alive.fetch_add(1, Ordering::Relaxed);
            Ok(Object(self.next_object, self.objects_alive.clone()))
        }

        fn map(&mut self, process: u32, object: &Object, addr: usize, len: usize) -> Result<()> {
            if self.refuse_map_in == Some(process) {
                return Err(Error::NoMemory);
            }
            let mappings = self.mapped.entry(process).or_default();
            assert!(mappings.insert(addr, (object.0, len)).is_none());
            Ok(())
        }

        fn unmap(&mut self, process: u32, addr: usize, len: usize) {
            let (_, mapped) = self
                .mapped
                .get_mut(&process)
                .unwrap()
                .remove(&addr)
                .unwrap();
            assert_eq!(mapped, len);
        }
    }

    const BASE: usize = 0x3000_0000_0000;
    const RANGE: core::ops::Range<usize> = BASE..BASE + 64 * PAGE_SIZE;

    fn task() -> TaskMemory<Recorder> {
        TaskMemory::new(Recorder::default(), RANGE)
    }

    fn mapped(task: &mut TaskMemory<Recorder>, process: u32) -> Vec<usize> {
        task.core()
            .mapped
            .get(&process)
            .map(|m| m.keys().copied().collect())
            .unwrap_or_default()
    }

    #[ktest]
    fn a_chunk_is_mapped_in_every_process_at_one_address() {
        let mut task = task();
        let both = [1, 2];
        let a = task.map(1, &both).unwrap();
        let b = task.map(3 * PAGE_SIZE, &both).unwrap();
        assert_eq!((a, b), (BASE, BASE + PAGE_SIZE));
        assert_eq!(mapped(&mut task, 1), vec![a, b]);
        assert_eq!(mapped(&mut task, 2), vec![a, b]);
        let (start, _, len) = task.chunk(b + 2 * PAGE_SIZE).unwrap();
        assert_eq!((start, len), (b, 3 * PAGE_SIZE));
        assert!(task.chunk(b + 3 * PAGE_SIZE).is_none());
        assert!(task.chunk(BASE - 1).is_none());
    }

    #[ktest]
    fn a_freed_chunk_leaves_every_process_and_its_addresses_return() {
        let mut task = task();
        let both = [1, 2];
        let a = task.map(PAGE_SIZE, &both).unwrap();
        let b = task.map(PAGE_SIZE, &both).unwrap();
        let c = task.map(PAGE_SIZE, &both).unwrap();
        task.unmap(b, &both).unwrap();
        task.unmap(a, &both).unwrap();
        assert_eq!(task.unmap(a, &both), Err(Error::InvalidArgs));
        assert_eq!(mapped(&mut task, 2), vec![c]);
        assert_eq!(task.core().objects_alive.load(Ordering::Relaxed), 1);
        // The two freed pages merged: a two-page chunk fits where they were.
        assert_eq!(task.map(2 * PAGE_SIZE, &both).unwrap(), a);
        task.unmap(c, &both).unwrap();
        // Everything after `a`'s two pages is one free range again.
        assert_eq!(task.map(62 * PAGE_SIZE, &both).unwrap(), a + 2 * PAGE_SIZE);
        assert_eq!(task.map(PAGE_SIZE, &both), Err(Error::NoMemory));
    }

    #[ktest]
    fn a_refused_map_or_object_maps_nothing() {
        let mut task = task();
        let all = [1, 2, 3];
        task.core().refuse_map_in = Some(3);
        assert_eq!(task.map(PAGE_SIZE, &all), Err(Error::NoMemory));
        assert!(mapped(&mut task, 1).is_empty());
        assert!(mapped(&mut task, 2).is_empty());
        assert_eq!(task.core().objects_alive.load(Ordering::Relaxed), 0);
        task.core().refuse_map_in = None;
        task.core().refuse_create = true;
        assert_eq!(task.map(PAGE_SIZE, &all), Err(Error::NoMemory));
        task.core().refuse_create = false;
        // The addresses the refusals reserved are free again.
        assert_eq!(task.map(PAGE_SIZE, &all).unwrap(), BASE);
    }

    #[ktest]
    fn the_process_table_holds_each_once_and_no_more_than_its_room() {
        let mut table: Processes<u32, 3> = Processes::new(0);
        assert!(table.as_slice().is_empty());
        table.add(7).unwrap();
        table.add(8).unwrap();
        table.add(7).unwrap();
        table.add(9).unwrap();
        assert_eq!(table.add(10), Err(Error::NotEnoughResources));
        assert_eq!(table.as_slice(), [7, 8, 9]);
        table.remove(8);
        table.remove(42);
        assert_eq!(table.as_slice(), [7, 9]);
        table.add(10).unwrap();
        assert_eq!(table.as_slice(), [7, 9, 10]);
    }

    #[ktest]
    fn the_heap_s_addresses_go_up_in_pages_until_the_range_ends() {
        let mut bump = Bump::new(BASE..BASE + 4 * PAGE_SIZE);
        assert_eq!(bump.take(1), Some(BASE));
        assert_eq!(bump.take(2 * PAGE_SIZE), Some(BASE + PAGE_SIZE));
        assert_eq!(bump.take(2 * PAGE_SIZE), None);
        assert_eq!(bump.take(PAGE_SIZE), Some(BASE + 3 * PAGE_SIZE));
        assert_eq!(bump.take(0), None);
    }

    #[ktest]
    fn mapping_everywhere_maps_in_all_or_none() {
        let mut core = Recorder::default();
        let object = core.create(PAGE_SIZE).unwrap();
        map_everywhere(&mut core, &[1, 2], &object, BASE, PAGE_SIZE).unwrap();
        assert_eq!(core.mapped[&1].len() + core.mapped[&2].len(), 2);
        core.refuse_map_in = Some(5);
        assert_eq!(
            map_everywhere(&mut core, &[4, 5], &object, BASE, PAGE_SIZE),
            Err(Error::NoMemory)
        );
        assert!(core.mapped[&4].is_empty());
    }
}
