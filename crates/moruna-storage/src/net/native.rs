//! The job's network on a node (system design 3D, item 3.4): `moruna-net-session`'s sockets as
//! the connector's [`Network`], over the channel node-admission minted for the job. Each socket
//! has its own session and reader, so a receive waits while a send goes on the same socket, and
//! a close answers whatever waits.

use std::{fmt, io, sync::Arc};

use moruna_net_session as session;
use moruna_svc::{Account, Channel};

use super::{NetSocket, Network};

/// The start slot node-admission gives the engine its job's network on.
pub const SLOT: &str = "network";

/// The job's network: its channel, and the account its sockets' memory is charged to.
pub struct MorunaNet(session::Network);

impl fmt::Debug for MorunaNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MorunaNet")
    }
}

impl MorunaNet {
    /// The job's network on `listener`, its sockets charged to `account`.
    pub fn new(listener: Channel, account: Account) -> MorunaNet {
        MorunaNet(session::Network::new(listener, account))
    }
}

impl Network for MorunaNet {
    fn open(&self, host: &str, port: u16) -> io::Result<Box<dyn NetSocket>> {
        Ok(Box::new(Socket(self.0.open(host, port)?)))
    }
}

/// One of the job's sockets.
struct Socket(session::Socket);

impl NetSocket for Socket {
    fn start_tls(&self, server_name: &str) -> io::Result<()> {
        self.0.start_tls(server_name)
    }

    fn send(&self, bytes: &[u8]) -> io::Result<usize> {
        self.0.send(bytes)
    }

    fn receive(&self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.receive(buffer)
    }

    fn close(&self) {
        self.0.close()
    }
}

/// Installs the job's network from the engine's start handles: the channel node-admission gave
/// it on [`SLOT`], its sockets charged to `account`, the engine's own. `false` when the engine
/// was given no network (its job's check named no host), or one is installed already.
pub fn install_from_start(account: Account) -> bool {
    let Some(listener) = moruna_svc::start::take(SLOT) else {
        return false;
    };
    super::install(Arc::new(MorunaNet::new(Channel(listener), account)))
}
