//! Controlled lifecycle port; never opens an io_uring or performs device IO.
use super::*;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct Model {
    published: VecDeque<(u64, usize)>,
    consumed: VecDeque<(u64, usize)>,
    cq: VecDeque<(u64, i32)>,
    submits: usize,
    completed: usize,
    hold: bool,
    fail_first: bool,
    consume_each: usize,
    closed: bool,
}
#[derive(Clone)]
struct Control(Arc<(Mutex<Model>, Condvar)>);
impl Control {
    fn new(fail_first: bool) -> Self {
        Self(Arc::new((
            Mutex::new(Model {
                hold: true,
                fail_first,
                consume_each: 1,
                ..Model::default()
            }),
            Condvar::new(),
        )))
    }
    fn wait_submits(&self, wanted: usize) {
        let (mutex, wake) = &*self.0;
        let model = mutex.lock().unwrap();
        let (model, timeout) = wake
            .wait_timeout_while(model, Duration::from_secs(5), |m| m.submits < wanted)
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "controlled owner did not submit {wanted}: {}",
            model.submits
        );
    }
    fn release(&self) {
        let (mutex, wake) = &*self.0;
        let mut model = mutex.lock().unwrap();
        model.hold = false;
        wake.notify_all();
    }
    fn closed(&self) -> bool {
        self.0.0.lock().unwrap().closed
    }
}
struct TestPort(Control);
impl Port for TestPort {
    fn push(&mut self, id: u64, req: &FileReq, transferred: usize) -> bool {
        let mut m = self.0.0.0.lock().unwrap();
        if m.published.len() == 2 {
            return false;
        }
        m.published
            .push_back((id, (req.len - transferred).min(CHUNK)));
        true
    }
    fn submit(&mut self) -> io::Result<usize> {
        let mut m = self.0.0.0.lock().unwrap();
        m.submits += 1;
        let mut count = 0;
        while count < m.consume_each {
            let Some(entry) = m.published.pop_front() else {
                break;
            };
            m.consumed.push_back(entry);
            count += 1;
        }
        self.0.0.1.notify_all();
        if m.fail_first && m.submits == 1 {
            Err(io::Error::other("controlled uncertain partial submit"))
        } else {
            Ok(count)
        }
    }
    fn unconsumed(&mut self) -> usize {
        self.0.0.0.lock().unwrap().published.len()
    }
    fn complete(&mut self) -> Option<(u64, i32)> {
        let mut m = self.0.0.0.lock().unwrap();
        if !m.hold {
            while let Some((id, len)) = m.consumed.pop_front() {
                m.cq.push_back((id, len as i32));
            }
        }
        let result = m.cq.pop_front();
        if result.is_some() {
            m.completed += 1;
        }
        result
    }
}
impl Drop for TestPort {
    fn drop(&mut self) {
        let mut m = self.0.0.0.lock().unwrap();
        assert!(
            m.consumed.is_empty() && m.cq.is_empty(),
            "ring closed while consumed original CQEs remain"
        );
        m.closed = true;
    }
}
struct Owner {
    bytes: Vec<u8>,
    dropped: Arc<AtomicBool>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}
fn request() -> (FileReq, Owner, Arc<AtomicBool>) {
    let dropped = Arc::new(AtomicBool::new(false));
    let mut owner = Owner {
        bytes: vec![0; 8],
        dropped: Arc::clone(&dropped),
    };
    let request = FileReq {
        fd: Arc::new(OwnedFd::from(File::open("/dev/null").unwrap())),
        verb: Verb::Read,
        ptr: owner.bytes.as_mut_ptr() as usize,
        len: owner.bytes.len(),
        offset: 0,
    };
    (request, owner, dropped)
}

#[test]
fn re_t17_stop_and_dropped_receiver_retain_owner_until_original_cqe() {
    let control = Control::new(false);
    let shared = Arc::new(Shared::default());
    let (request, owner, dropped) = request();
    let (sender, receiver) = moruna_kernel::Completion::<usize>::channel();
    drop(receiver);
    shared.submit(
        request,
        Box::new(move |result| {
            let _until_original = owner;
            sender.resolve(result.map_err(|e| moruna_kernel::MorunaError::Io {
                op: "test",
                target: "controlled".into(),
                msg: e.to_string(),
            }));
        }),
    );
    let worker = {
        let shared = Arc::clone(&shared);
        let control = control.clone();
        std::thread::spawn(move || run(TestPort(control), shared))
    };
    control.wait_submits(1);
    shared.stop();
    shared.wait_draining();
    assert!(!dropped.load(Ordering::SeqCst));
    assert!(!control.closed());
    control.release();
    worker.join().unwrap();
    assert!(dropped.load(Ordering::SeqCst));
    assert!(control.closed());
}

#[test]
fn re_t18_partial_failed_submit_drains_consumed_then_closes_before_unconsumed_release() {
    let control = Control::new(true);
    let shared = Arc::new(Shared::default());
    let done = Arc::new(AtomicUsize::new(0));
    let mut owners = Vec::new();
    for index in 0..2 {
        let (request, owner, dropped) = request();
        owners.push(dropped);
        let (done, control) = (Arc::clone(&done), control.clone());
        shared.submit(
            request,
            Box::new(move |result| {
                assert!(result.is_err());
                if index == 1 {
                    assert!(
                        control.closed(),
                        "unconsumed published owner outlived ring closure"
                    );
                }
                drop(owner);
                done.fetch_add(1, Ordering::SeqCst);
            }),
        );
    }
    let worker = {
        let shared = Arc::clone(&shared);
        let control = control.clone();
        std::thread::spawn(move || run(TestPort(control), shared))
    };
    control.wait_submits(1);
    shared.stop();
    shared.wait_draining();
    assert!(owners.iter().all(|d| !d.load(Ordering::SeqCst)));
    assert_eq!(done.load(Ordering::SeqCst), 0);
    assert!(!control.closed());
    control.release();
    worker.join().unwrap();
    assert!(owners.iter().all(|d| d.load(Ordering::SeqCst)));
    assert_eq!(done.load(Ordering::SeqCst), 2);
}

#[test]
fn re_t19_partial_successful_submissions_drain_all_originals() {
    let control = Control::new(false);
    let shared = Arc::new(Shared::default());
    let done = Arc::new(AtomicUsize::new(0));
    for _ in 0..3 {
        let (request, owner, _) = request();
        let done = Arc::clone(&done);
        shared.submit(
            request,
            Box::new(move |result| {
                assert_eq!(result.unwrap(), 8);
                drop(owner);
                done.fetch_add(1, Ordering::SeqCst);
            }),
        );
    }
    let worker = {
        let shared = Arc::clone(&shared);
        let control = control.clone();
        std::thread::spawn(move || run(TestPort(control), shared))
    };
    control.wait_submits(3);
    shared.stop();
    shared.wait_draining();
    assert_eq!(done.load(Ordering::SeqCst), 0);
    control.release();
    worker.join().unwrap();
    assert_eq!(done.load(Ordering::SeqCst), 3);
}

#[test]
fn re_t20_partial_original_cqe_retains_owner_until_retry_original_cqe() {
    let control = Control::new(false);
    let shared = Arc::new(Shared::default());
    let (request, owner, dropped) = request();
    let done = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&done);
    shared.submit(
        request,
        Box::new(move |result| {
            assert_eq!(result.unwrap(), 8);
            drop(owner);
            count.fetch_add(1, Ordering::SeqCst);
        }),
    );
    let worker = {
        let shared = Arc::clone(&shared);
        let control = control.clone();
        std::thread::spawn(move || run(TestPort(control), shared))
    };
    control.wait_submits(1);
    {
        let mut model = control.0.0.lock().unwrap();
        let (id, _) = model.consumed.pop_front().unwrap();
        model.cq.push_back((id, 4));
    }
    control.wait_submits(2);
    shared.stop();
    shared.wait_draining();
    assert!(!dropped.load(Ordering::SeqCst));
    assert_eq!(done.load(Ordering::SeqCst), 0);
    control.release();
    worker.join().unwrap();
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(done.load(Ordering::SeqCst), 1);
}

#[test]
fn re_t21_callback_panic_closes_consumer_with_precise_error_and_retains_other_original() {
    struct PanicForwarder(Arc<Shared>);
    impl crate::file_blocking::FileEngine for PanicForwarder {
        fn submit(&self, req: FileReq, done: Done) {
            self.0.submit(
                req,
                Box::new(move |_| {
                    let _would_forward_owned_result = done;
                    panic!("controlled completion-forwarder failure");
                }),
            );
        }
        fn name(&self) -> &'static str {
            "controlled"
        }
        fn terminal_failure(&self) -> Option<String> {
            self.0.terminal_failure()
        }
    }
    let control = Control::new(false);
    let shared = Arc::new(Shared::default());
    let engine: Arc<dyn crate::file_blocking::FileEngine> =
        Arc::new(PanicForwarder(Arc::clone(&shared)));
    let (first, first_owner, first_dropped) = request();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let consumer = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (result, owner) = runtime.block_on(crate::runtime::once(&engine, first, first_owner));
        assert!(owner.is_none());
        result_tx.send(result.unwrap_err().to_string()).unwrap();
    });
    let worker = {
        let shared = Arc::clone(&shared);
        let control = control.clone();
        std::thread::spawn(move || run(TestPort(control), shared))
    };
    control.wait_submits(1);
    let (second, second_owner, second_dropped) = request();
    shared.submit(
        second,
        Box::new(move |result| {
            assert!(result.is_err());
            drop(second_owner);
        }),
    );
    control.wait_submits(2);
    {
        let mut model = control.0.0.lock().unwrap();
        let (id, len) = model.consumed.pop_front().unwrap();
        model.cq.push_back((id, len as i32));
    }
    let cause = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(cause.contains("completion callback panicked"), "{cause}");
    assert!(first_dropped.load(Ordering::SeqCst));
    assert!(!second_dropped.load(Ordering::SeqCst));
    assert!(!control.closed());
    control.release();
    consumer.join().unwrap();
    worker.join().unwrap();
    assert!(second_dropped.load(Ordering::SeqCst));
}
