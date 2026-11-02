//! Exercise the EXACT shipping engine query through the existing SDK host model.
//! No copied handshake/version/seal logic and no native syscall feature spoofing.
#[path = "../../../../crates/moruna-micro/src/image_layers.rs"]
mod layer_query;
use linux_protocol::{Bytes, linux};
use moruna_svc::{
    Channel, Context, Decoder, Dispatcher, Handler, Outcome, ProtocolError, ProtocolId, Status,
    deadline_in,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone)]
enum Reply {
    Seals(Vec<Bytes>),
    NoImage,
    CorruptWire,
}
struct Layers {
    protocol: ProtocolId,
    reply: Reply,
    requests: Arc<AtomicUsize>,
}
impl Handler for Layers {
    fn protocol(&self) -> ProtocolId {
        self.protocol
    }
    fn handle(&mut self, _: &mut Context<'_>, ordinal: u16, body: &mut Decoder<'_>) -> Outcome {
        self.requests.fetch_add(1, Ordering::SeqCst);
        if ordinal != 4 {
            return Outcome::Status(Status::UnknownMethod);
        }
        if body.finish().is_err() {
            return Outcome::Status(Status::BadMessage);
        }
        match &self.reply {
            Reply::Seals(seals) => Outcome::reply(seals.clone()),
            // A failed/corrupt imagefs layers query is exposed by Linux as
            // canonical NoImage. The engine must not turn it into empty success.
            Reply::NoImage => Outcome::Failure(linux::Error::NoImage.code()),
            // Deliberately malformed service response, decoded by the actual
            // generated client; no second test decoder is introduced.
            Reply::CorruptWire => Outcome::reply(7u8),
        }
    }
}
fn query(protocol: ProtocolId, reply: Reply) -> (Result<Vec<[u8; 32]>, String>, usize) {
    let (listener, server) = Channel::create().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let join = std::thread::spawn(move || {
        let mut dispatcher = Dispatcher::new();
        dispatcher.listen(
            server,
            Box::new(move || {
                Box::new(Layers {
                    protocol,
                    reply: reply.clone(),
                    requests: count.clone(),
                }) as Box<dyn Handler>
            }),
        )?;
        dispatcher.run()
    });
    let result = layer_query::query(&listener, deadline_in(5_000_000_000));
    drop(listener);
    join.join().unwrap().unwrap();
    (result, requests.load(Ordering::SeqCst))
}
#[test]
fn shipping_query_accepts_three_zero_minor_zero_and_ordered_seals() {
    let (result, requests) = query(
        ProtocolId::new("moruna.linux", 3, 0),
        Reply::Seals(vec![Bytes(vec![7; 32]), Bytes(vec![9; 32])]),
    );
    assert_eq!(result.unwrap(), vec![[7; 32], [9; 32]]);
    assert_eq!(requests, 1);
}
#[test]
fn shipping_query_refuses_no_image_instead_of_returning_empty_seals() {
    let (result, requests) = query(ProtocolId::new("moruna.linux", 3, 0), Reply::NoImage);
    assert!(result.unwrap_err().contains("NoImage"));
    assert_eq!(requests, 1);
}
#[test]
fn shipping_query_refuses_a_corrupt_layer_response() {
    let (result, requests) = query(ProtocolId::new("moruna.linux", 3, 0), Reply::CorruptWire);
    let error = result.unwrap_err();
    assert!(error.starts_with("the personality's layers:"), "{error}");
    assert_eq!(requests, 1);
}
#[test]
fn shipping_query_refuses_both_short_and_oversized_seals() {
    for length in [31, 33] {
        let (result, requests) = query(
            ProtocolId::new("moruna.linux", 3, 0),
            Reply::Seals(vec![Bytes(vec![7; 32]), Bytes(vec![9; length])]),
        );
        let error = result.unwrap_err();
        assert!(
            error.contains(&format!("layer 1's seal is {length} bytes, not 32")),
            "{error}"
        );
        assert_eq!(requests, 1);
    }
}
#[test]
fn shipping_query_refuses_legacy_major_one_before_request() {
    let (result, requests) = query(
        ProtocolId::new("moruna.linux", 1, 3),
        Reply::Seals(vec![Bytes(vec![7; 32])]),
    );
    assert!(result.unwrap_err().contains("MajorMismatch"));
    assert_eq!(requests, 0);
}
