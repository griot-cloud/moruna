//! One client connection (SDD 31, f.4): `moruna.net` decoded and answered by the
//! stack, under the policy the connection's listener was granted.

use alloc::{boxed::Box, rc::Rc, string::String, vec::Vec};
use core::cell::RefCell;

use moruna_svc::{
    Channel, Context, Decoder, Dispatcher, Factory, Handler, Outcome, ProtocolError, ProtocolId,
    Status, Wire,
};
use netstack_protocol::{Addr, Endpoint, Policy, Target, net};

use crate::Stack;

/// The stack, shared by every connection's handler and the server's loop, and the
/// connections that closed while the stack was in use.
pub struct Shared {
    pub stack: RefCell<Stack>,
    pub gone: RefCell<Vec<u64>>,
}

impl Shared {
    pub fn new(stack: Stack) -> Rc<Shared> {
        Rc::new(Shared {
            stack: RefCell::new(stack),
            gone: RefCell::new(Vec::new()),
        })
    }

    /// Tells the stack of the connections that closed while it was in use.
    pub fn settle(&self) {
        let gone = core::mem::take(&mut *self.gone.borrow_mut());
        let mut stack = self.stack.borrow_mut();
        for id in gone {
            stack.connection_closed(id);
        }
    }
}

/// What the server's loop does after every turn of its dispatcher: tells the stack
/// of connections that closed, polls it at `now`, makes the listeners it granted, and
/// sends every answer that is ready.
pub fn after_turn(shared: &Rc<Shared>, d: &mut Dispatcher, now: u64) {
    shared.settle();
    let (ready, grants) = {
        let mut st = shared.stack.borrow_mut();
        st.poll(now);
        (
            core::mem::take(&mut st.ready),
            core::mem::take(&mut st.grants),
        )
    };
    for (pending, policy) in grants {
        let outcome = match Channel::create() {
            Ok((ours, theirs)) => match d.listen(ours, factory(shared.clone(), policy)) {
                Ok(()) => Outcome::reply(theirs),
                // As many handles as one wait takes: no room for another listener.
                Err(_) => Outcome::Failure(net::Error::TooMany.code()),
            },
            Err(_) => Outcome::Failure(net::Error::TooMany.code()),
        };
        // An answer whose connection has gone is dropped with it.
        let _ = d.complete(pending, outcome);
    }
    for (pending, outcome) in ready {
        let _ = d.complete(pending, outcome);
    }
}

/// A connection's handler.
pub struct Session {
    shared: Rc<Shared>,
    policy: Policy,
    id: Option<u64>,
}

/// A listener's factory: every connection through it holds `policy`.
pub fn factory(shared: Rc<Shared>, policy: Policy) -> Factory {
    Box::new(move || {
        Box::new(Session {
            shared: shared.clone(),
            policy: policy.clone(),
            id: None,
        }) as Box<dyn Handler>
    })
}

/// The arguments of a request: exactly one value of `T`, nothing left over.
fn args<T: Wire>(d: &mut Decoder<'_>) -> Option<T> {
    let v = T::decode(d).ok()?;
    d.finish().ok()?;
    Some(v)
}

impl Handler for Session {
    fn protocol(&self) -> ProtocolId {
        net::ID
    }

    fn handle(&mut self, cx: &mut Context<'_>, ordinal: u16, d: &mut Decoder<'_>) -> Outcome {
        let id = cx.connection();
        self.id = Some(id);
        let mut st = self.shared.stack.borrow_mut();
        st.connection(id, &self.policy);
        let bad = Outcome::Status(Status::BadMessage);
        let bound = st.account_bound(id);
        macro_rules! authorized {
            ($operation:expr) => {
                if bound {
                    $operation
                } else {
                    Outcome::Failure(net::Error::NotPermitted.code())
                }
            };
        }
        match ordinal {
            1 => match args::<()>(d) {
                Some(()) => authorized!(st.info(id)),
                None => bad,
            },
            2 => match args::<(Target, Option<Addr>)>(d) {
                Some((to, from)) => authorized!(st.open(cx, id, to, from)),
                None => bad,
            },
            3 => match args::<(Endpoint, u32)>(d) {
                Some((at, backlog)) => authorized!(st.listen(id, at, backlog)),
                None => bad,
            },
            4 => match args::<u32>(d) {
                Some(l) => authorized!(st.accept(cx, id, l)),
                None => bad,
            },
            5 => match args::<(u32, moruna_svc::Memory, u64, u32)>(d) {
                Some((s, source, offset, length)) => {
                    authorized!(st.send_from(cx, id, s, source, offset, length))
                }
                None => bad,
            },
            6 => match args::<(u32, moruna_svc::Memory, u64, u32)>(d) {
                Some((s, destination, offset, max)) => {
                    authorized!(st.receive_into(cx, id, s, destination, offset, max))
                }
                None => bad,
            },
            7 => match args::<u32>(d) {
                Some(s) => authorized!(st.shutdown(cx, id, s)),
                None => bad,
            },
            8 => match args::<u32>(d) {
                Some(s) => authorized!(st.close(id, s)),
                None => bad,
            },
            9 => match args::<Endpoint>(d) {
                Some(at) => authorized!(st.bind(id, at)),
                None => bad,
            },
            10 => match args::<(u32, Endpoint, Vec<u8>)>(d) {
                Some((s, to, bytes)) => authorized!(st.send_to(cx, id, s, to, bytes)),
                None => bad,
            },
            11 => match args::<u32>(d) {
                Some(s) => authorized!(st.receive_from(cx, id, s)),
                None => bad,
            },
            12 => match args::<String>(d) {
                Some(name) => authorized!(st.resolve(cx, id, name)),
                None => bad,
            },
            13 => match args::<(Addr, u32)>(d) {
                Some((to, size)) => authorized!(st.ping(cx, id, to, size)),
                None => bad,
            },
            14 => match args::<Policy>(d) {
                Some(p) => authorized!(st.grant(cx, id, p)),
                None => bad,
            },
            15 => match args::<u32>(d) {
                Some(s) => authorized!(st.status(id, s)),
                None => bad,
            },
            16 => match args::<(u32, String)>(d) {
                Some((socket, name)) => authorized!(st.start_tls(cx, id, socket, name)),
                None => bad,
            },
            17 => match args::<moruna_svc::Handle>(d) {
                Some(account) => st.bind_account(id, account),
                None => bad,
            },
            18 => match args::<(moruna_svc::Memory, u32, moruna_svc::Memory, u32)>(d) {
                Some((control, n, key, k)) => {
                    authorized!(st.install_server_identity(id, control, n, key, k))
                }
                None => bad,
            },
            19 => match args::<(u32, u32)>(d) {
                Some((socket, identity)) => {
                    authorized!(st.start_tls_server(cx, id, socket, identity))
                }
                None => bad,
            },
            20 => match args::<u32>(d) {
                Some(identity) => authorized!(st.release_server_identity(id, identity)),
                None => bad,
            },
            21 => match args::<u32>(d) {
                Some(socket) => authorized!(st.tls_alpn(id, socket)),
                None => bad,
            },
            _ => Outcome::Status(Status::UnknownMethod),
        }
    }

    fn closed(&mut self) {
        let Some(id) = self.id else { return };
        match self.shared.stack.try_borrow_mut() {
            Ok(mut st) => st.connection_closed(id),
            Err(_) => self.shared.gone.borrow_mut().push(id),
        }
    }
}
