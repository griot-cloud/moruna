//! A `node-admission` for the engine host's tests (item 3.2): it serves the real protocol on its
//! own thread, keeps every engine it is asked to start (its configuration, its slots, its
//! services, its budget and needs, and the engine's end of its channel, which the test then
//! plays the engine on), and answers as `kernel/node/admission` does: the job bound, a check at
//! a time, a state until the test ends the engine, and a connection's engines ended when the
//! connection closes (its `Session::closed`).

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    thread,
};

use moruna_svc::{Context, Decoder, Dispatcher, Handler, Memory, Outcome, ProtocolId, map};
use node_protocol::{CANCELLED, Needs, RUNNING, TERMINATED, admission};

use super::*;

/// One engine the test's admission was asked to start.
pub struct Started {
    pub id: u64,
    /// The connection that asked.
    pub owner: u64,
    /// `None` for a check.
    pub budget: Option<u64>,
    /// The engine supervisor's configuration, as its data handle holds it.
    pub config: String,
    pub names: Vec<String>,
    pub services: Vec<String>,
    pub needs: Needs,
    /// The engine's end of its channel to `api`, until the test takes it.
    pub channel: Option<Channel>,
}

/// What the test's admission has been asked and keeps.
#[derive(Default)]
pub struct Book {
    pub bound: u64,
    pub started: Vec<Started>,
    pub states: BTreeMap<u64, u32>,
    pub released: Vec<u64>,
    pub waited: Vec<u64>,
    pub cancelled: Vec<u64>,
    /// The connections that have closed, in order.
    pub closed: Vec<u64>,
    /// What the next start answers instead of starting.
    pub refuse: Option<admission::Error>,
    /// The check that holds the check account, until it is released.
    check: Option<u64>,
    next: u64,
}

/// The test's `node-admission`, and the client's end of its listener, which the test connects
/// through as the supervisor does for each start of `api`.
pub struct Admission {
    pub book: Arc<Mutex<Book>>,
    listener: Channel,
}

impl Admission {
    /// It serves on its own thread, with a job bound of `bound` bytes.
    pub fn start(bound: u64) -> Admission {
        let book = Arc::new(Mutex::new(Book {
            bound,
            ..Book::default()
        }));
        let (listener, theirs) = Channel::create().expect("a channel");
        let served = book.clone();
        thread::spawn(move || {
            let mut dispatcher = Dispatcher::new();
            dispatcher
                .listen(
                    theirs,
                    Box::new(move || {
                        Box::new(Session(admission::Serve(Fake {
                            book: served.clone(),
                            owner: None,
                        }))) as Box<dyn Handler>
                    }),
                )
                .expect("listening");
            let _ = dispatcher.run();
        });
        Admission { book, listener }
    }

    /// A fresh connection, as the supervisor makes one for each start of `api`.
    pub fn connect(&self) -> Channel {
        moruna_svc::connect(&self.listener).expect("a connection")
    }

    /// The engine's end of the channel of the engine started `nth`, from 0.
    pub fn engine(&self, nth: usize) -> Channel {
        self.book.lock().unwrap().started[nth]
            .channel
            .take()
            .expect("the engine's channel")
    }

    /// The engine `id`'s process ends.
    pub fn end(&self, id: u64) {
        self.book.lock().unwrap().states.insert(id, TERMINATED);
    }

    /// The configuration the engine started `nth` was given.
    pub fn config(&self, nth: usize) -> String {
        self.book.lock().unwrap().started[nth].config.clone()
    }
}

/// One connection's session: the protocol's server, and its end.
struct Session(admission::Serve<Fake>);

impl Handler for Session {
    fn protocol(&self) -> ProtocolId {
        admission::ID
    }

    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        self.0.0.owner = Some(cx.connection());
        self.0.handle(cx, ordinal, d)
    }

    /// As node-admission's: the connection's engines that still run are killed.
    fn closed(&mut self) {
        let fake = &self.0.0;
        let mut book = fake.book.lock().unwrap();
        if let Some(owner) = fake.owner {
            let ids: Vec<u64> = book
                .started
                .iter()
                .filter(|s| s.owner == owner)
                .map(|s| s.id)
                .collect();
            for id in ids {
                if book.states.get(&id) == Some(&RUNNING) {
                    book.states.insert(id, CANCELLED);
                }
            }
            book.closed.push(owner);
        }
    }
}

struct Fake {
    book: Arc<Mutex<Book>>,
    owner: Option<u64>,
}

/// The text of a configuration's data handle: its bytes up to the first zero.
fn text(config: Handle) -> String {
    let memory = Memory(config);
    let mapping = memory
        .map(0, 4096, map::READ)
        .expect("the configuration maps");
    let mut bytes = vec![0u8; 4096];
    mapping.read(0, &mut bytes).expect("it reads");
    let end = bytes.iter().position(|b| *b == 0).expect("a page at most");
    String::from_utf8(bytes[..end].to_vec()).expect("text")
}

impl Fake {
    fn started(
        &mut self,
        budget: Option<u64>,
        config: Handle,
        names: Vec<String>,
        mut grants: Vec<Handle>,
        services: Vec<String>,
        needs: Needs,
    ) -> Result<u64, admission::Error> {
        let mut book = self.book.lock().unwrap();
        if let Some(refusal) = book.refuse.take() {
            return Err(refusal);
        }
        if budget.is_none() && book.check.is_some_and(|id| !book.released.contains(&id)) {
            return Err(admission::Error::Limit);
        }
        if budget.is_some_and(|budget| budget > book.bound) {
            return Err(admission::Error::Quota);
        }
        assert_eq!(grants.len(), 1, "one grant: the engine's channel");
        book.next += 1;
        let id = book.next;
        if budget.is_none() {
            book.check = Some(id);
        }
        book.states.insert(id, RUNNING);
        book.started.push(Started {
            id,
            owner: self.owner.expect("a connection"),
            budget,
            config: text(config),
            names,
            services,
            needs,
            channel: Some(Channel(grants.remove(0))),
        });
        Ok(id)
    }

    fn state(&self, id: u64) -> Result<u32, admission::Error> {
        let book = self.book.lock().unwrap();
        if book.released.contains(&id) {
            return Err(admission::Error::NotFound);
        }
        book.states
            .get(&id)
            .copied()
            .ok_or(admission::Error::NotFound)
    }
}

impl admission::Server for Fake {
    fn launch(
        &mut self,
        _: &mut Context<'_>,
        budget: u64,
        config: Handle,
        names: Vec<String>,
        grants: Vec<Handle>,
        services: Vec<String>,
        needs: Needs,
    ) -> Result<u64, admission::Error> {
        // As node-admission's (item 3.4): a job that names hosts gets its network as `network`.
        let mut names = names;
        if !needs.0.is_empty() {
            names.push("network".into());
        }
        self.started(Some(budget), config, names, grants, services, needs)
    }

    fn status(&mut self, _: &mut Context<'_>, job: u64) -> Result<u32, admission::Error> {
        self.state(job)
    }

    fn cancel(&mut self, _: &mut Context<'_>, job: u64) -> Result<(), admission::Error> {
        self.state(job)?;
        let mut book = self.book.lock().unwrap();
        book.cancelled.push(job);
        book.states.insert(job, CANCELLED);
        Ok(())
    }

    fn wait(&mut self, _: &mut Context<'_>, job: u64) -> Result<u32, admission::Error> {
        let state = self.state(job)?;
        self.book.lock().unwrap().waited.push(job);
        Ok(state)
    }

    fn release(&mut self, _: &mut Context<'_>, job: u64) -> Result<(), admission::Error> {
        if self.state(job)? == RUNNING {
            return Err(admission::Error::Invalid);
        }
        self.book.lock().unwrap().released.push(job);
        Ok(())
    }

    fn check(
        &mut self,
        _: &mut Context<'_>,
        config: Handle,
        names: Vec<String>,
        grants: Vec<Handle>,
        services: Vec<String>,
    ) -> Result<u64, admission::Error> {
        // As node-admission's (item 3.3): the node's volumes, as data, beside the channel.
        let mut names = names;
        names.push(node_protocol::VOLUMES_SLOT.into());
        self.started(None, config, names, grants, services, Default::default())
    }

    fn bound(&mut self, _: &mut Context<'_>) -> Result<u64, admission::Error> {
        Ok(self.book.lock().unwrap().bound)
    }
}
