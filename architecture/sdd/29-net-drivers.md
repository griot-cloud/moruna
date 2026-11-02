# Moruna SDD 29: Network card drivers (`kernel/drivers/net-protocol`, `virtio-net`, `gve`)

**Document type:** software design document, component 29
**Status:** DRAFT · 2026-10-29 (becomes HANDOFF-READY when section m is empty; the human flips it)
**Parent:** `BOARD.md` section EK, item KE1 (Brackly, 2026-10-28): "NVMe and the cloud network card as driver processes"; `architecture/optimized-runtime.md` (the kernel as a node); SDD 25 section o (what KE1 needs: the driver shape over `moruna-pci`, several MSI-X vectors, the network server as the card's client). NVMe, KE1's other half, is SDD 25 f.15.
**Decisions:** none new; MK1 (OSTD) stands
**Component location:** `kernel/drivers/net-protocol` (`moruna.driver.net`, its receive hub, the server every driver runs, the suite for one card and the suite for two cards on one link), `kernel/drivers/virtio/src/net.rs` (`moruna_virtio::net`), `kernel/drivers/virtio-net` (the virtio-net driver), `kernel/drivers/gve-core` (`moruna-gve`, gVNIC in the GQI format, host tested against a model), `kernel/drivers/gve` (the gVNIC driver), `kernel/drivers/net-test` (the boot's client); the pair suite in `kernel/svc/src/conformance.rs` (SDD 23 f.5); contiguous DMA memory in `kernel/{abi,core,sys}` (ABI 2.1, SDD 25 f.4); the cards in `kernel/initramfs/boot.image`
**Consumes:** `moruna-svc` (protocols, the dispatcher, the conformance harness and its pair suites), `moruna-pci` (the function, `DmaBuffer`), `moruna-virtio`, `moruna-mmio`, `devices-protocol` (the directory). **Consumed by:** KE2 (the network server, smoltcp and rustls, the protocol's client), KE3 (the node image on Google Cloud, which proves gVNIC).

**Decisions worth your eye:** (1) frames travel in messages, a batch each way, not by DMA into a client's memory as block data does: a card's buffers are its device's (virtio-net's descriptors, gVNIC's registered page lists), and the network server copies each frame into its own buffers anyway; (2) offloads are declared and 1.0 defines none, so nothing a client sends or receives has been changed by the card; (3) every subscribed connection gets every frame, each into a bounded queue that drops and counts when full, so a slow client loses its own frames and nobody else's; (4) the server side of the protocol is one `Server` every driver runs, and a driver supplies only its card's transmit ring; (5) gVNIC is built from the interface Google publishes with its Linux driver and is **not yet proven on hardware**: there is no emulator, and its tests run against a model of the device; (6) gVNIC's rings are longer than a page, which needed the one kernel addition of KE1, contiguous DMA memory (ABI 2.1); (7) gVNIC's DQO queue format is not built; a device that offers only DQO is refused at start by name.

---

## a. Purpose and boundary

A Moruna node needs a network card for the network server (KE2) to speak TCP/IP over. This component owns: the network card protocol and its conformance suites; the receive fan-out and the server every network driver serves the protocol with; a driver for virtio-net (Google Cloud offers it, QEMU emulates it); a driver for gVNIC (Google Cloud's own card, in the GQI queue format); the boot's two cards and their test.

It refuses to know: IP, TCP, ARP, DHCP, TLS (the network server's); which frames a client may send beyond their framing (the egress policy is Griot's and the server's); a card's checksum and segmentation offloads (none is negotiated).

## b. Vocabulary

**Frame.** A whole Ethernet frame without its frame check sequence: destination, source, EtherType, payload.

**Card.** A network device and the driver process that holds it.

**Subscriber.** A connection that has asked to receive the card's frames.

**Hub.** A driver's receive side: its subscribers' queues, their waiting receives, the link and its waiters, the counters.

**Ring.** A card's transmit side, as the server sees it: put a frame on the ring it belongs on, or say that ring is full.

**QPL, RDA.** gVNIC's two ways of naming buffers: a queue page list (pages registered once, descriptors carrying offsets into them) and raw DMA addressing (descriptors carrying device addresses).

## c. Invariants

- **NT-I1.** A frame a card sends is one a client sent, byte for byte, and a frame a client receives is one the card received, byte for byte (a receiver may see a frame shorter than 60 bytes padded to 60 with zeros).
- **NT-I2.** A send is refused whole if any frame in it is shorter than a header, longer than the card's largest frame, from a group address, or the batch is empty or over 64 frames; nothing of a refused send goes out.
- **NT-I3.** A send is answered once every frame of it is on a device ring; sends go to the rings in the order they arrived.
- **NT-I4.** Each subscribed connection's queue holds at most 1024 frames and 4 MiB; a frame that finds it full is dropped and counted for that connection alone.
- **NT-I5.** One reply carries at most `RECEIVE_BYTES` of frames, and always at least one frame of the largest size the protocol carries (`FRAME_MAX`, 16 KiB).
- **NT-I6.** Every value a card's device returns is checked before it is believed: a used ring's ids and lengths and a received buffer's header (virtio-net); a doorbell or counter index, an event counter, a receive descriptor's sequence number, length and flags, and every field of the device description (gVNIC). A value out of bounds is an error or a counted drop, never an index.
- **NT-I7.** A driver resets its card before the card may master the bus and before the driver releases its quarantine (SDD 25 DR-I8), and resets it again when it ends, so the card uses no memory of a driver that has gone.
- **NT-I8.** A card's MTU is what its driver can carry whole: one page of virtio-net buffer with its header; gVNIC's transmit slots and receive fragments; at most `FRAME_MAX` less the header.

## d. Interfaces

### d.1 `moruna.driver.net` 1.0

```rust
1 => fn info() -> Info;                       // mac, mtu, max_frame (mtu + 14), offloads, queues
2 => fn link() -> Link;                       // up, speed_mbps (0 unknown), generation
3 => fn link_changed(generation: u32) -> Link;
4 => fn send(frames: Vec<Frame>) -> u32;
5 => fn subscribe() -> ();
6 => fn receive(max: u32) -> Vec<Frame>;
7 => fn stats() -> Stats;                     // tx and rx frames and bytes, this connection's drops,
                                              // malformed frames from the device, transmit errors
errors { 1 => TooShort; 2 => TooLong; 3 => BadSource; 4 => Empty; 5 => TooMany;
         6 => NotSubscribed; 7 => InUse; 8 => Io; }
```

`Frame` travels as a length and its bytes. `mac` holds the first octet in bits 40 to 47. `link_changed` is answered at once when `generation` is not the link's, and when the link next changes otherwise. `receive` is answered once a frame is queued for the connection, with at most `max` (clamped to 1 to 1024) and within one reply; a connection that has not subscribed is `NotSubscribed`, and a second `receive` while one waits is `InUse`. A send while the card has failed is `Io`. `offload::KNOWN` is zero in 1.0.

### d.2 `net-protocol`

`Frame`, `Info`, `Link`, `Stats`, `net` (the protocol), `check(frames, max_frame)`, `flow_hash(frame)` (FNV-1a over an IPv4 or IPv6 frame's addresses, and its TCP or UDP ports unless it is a fragment; zero for any other frame), `octets`, `mac`, `is_group`; `Hub<P>` (`subscribe`, `receive`, `deliver`, `answers`, `link_changed`, `set_link`, `stats`, `closed`); `Server` (`new`, `handle`, `transmit`, `delivered`, `link`, `closed`, `waiting`, its `info`, `hub`, `ready` and `failed`) and `trait Ring` (`put`, `flush`); `suite()`, `pair_suite()`, `frame(dst, src, seq, len)`, `ETHERTYPE`. Constants: `HEADER` 14, `FRAME_MIN` 14, `FRAME_MAX` 16384, `SEND_FRAMES_MAX` 64, `QUEUE_FRAMES` 1024, `QUEUE_BYTES` 4 MiB, `RECEIVE_BYTES`.

### d.3 `moruna_virtio::net`

`feature` (`MTU`, `MAC`, `STATUS`, `CTRL_VQ`, `MQ`), `config`, `HEADER` (12), `RawConfig`, `read_raw`, `check_config`, `read_config`, `NetConfig`, `NetError`, `link_up`, `write_header`, `received`, `ctrl`, `write_pairs_command`, `rx_queue`, `tx_queue`, `ctrl_queue`.

### d.4 `moruna-gve`

`regs` (BAR 0's offsets and bits, the interrupt doorbell's `ACK`, `MASK`, `EVENT`), `admin` (`Command`, `AdminQueue::execute`, the statuses and their names), `describe` (`Description::parse`, `Format`), `device` (`Gve::start`, `send`, `flush`, `complete`, `receive`, `ack`, `pending`, `status`, `link_up`, `has_room`, `queues`, `max_frame`, `shutdown`; `Settings`), `trait Dma` (`alloc(bytes, contiguous)`), `GveError`, and the big-endian field helpers.

### d.5 What a network driver starts with

As every driver (SDD 25 d.9): `console`, `config`, `bar0` to `bar5`, `msi`, `dma`, `listen`. The CI table names `1af4:1041 virtio-net net` and `1ae0:0042 gve net`, so a machine's first card is `net0` whichever it is (SDD 25 e.3).

## e. Data model, formats and state machines

### e.1 A card's life

`started` → `serving` (the card reset, its queues set up and its receive buffers posted, the driver ready) → `ended` (every client gone: the card reset; or the card failed: the driver ends with a failure line, and the device manager starts it again).

### e.2 gVNIC's layouts

Every admin command, register and GQI descriptor is big endian. An admin command is 64 bytes: opcode, status, then the command's fields at the offsets of `union gve_adminq_command`. The device descriptor is 40 bytes, then options of an 8-byte header (id, length, required features) and a body. A transmit descriptor is 16 bytes (type and flags, checksum and header offsets, descriptor count, packet length, segment length, segment address); a receive descriptor is 64 (its length at 60, its flags and three-bit sequence number at 62); a receive data slot is 8 bytes (a QPL offset or a device address). A queue's resources (64 bytes) carry the doorbell index and the event counter index the device assigned it. The interrupt doorbell index array has one 64-byte entry a notification block.

## f. Algorithms and policies

### f.1 Frames in messages

A send carries up to 64 frames; a receive's reply up to `RECEIVE_BYTES`. A driver copies each frame between a message and its device's buffers. That is one copy each way inside the node, which the network server makes anyway when it moves a frame into its own buffers; a zero-copy form would be a registration like the block protocol's (SDD 25 d.3) over buffers the device can name, which gVNIC's QPL format cannot, since its pages are registered when its queues are created.

### f.2 The server and the hub

`Server::handle` answers the seven requests. A send is checked (NT-I2), queued, and `transmit` puts its frames on the card's `Ring` in order until a frame finds its ring full (NT-I3); it answers each send whose frames are all on a ring and flushes the card once. A driver hands every frame its device received to `Hub::deliver`, which queues a copy for every subscriber (NT-I4), and then calls `delivered`, which answers each waiting receive with what its connection's queue holds (NT-I5). `link` records the card's link and, when it changed, moves its generation on and answers the connections waiting. A driver's loop sends `ready` after each turn.

### f.3 The virtio-net driver

It negotiates `MAC` (required), `STATUS`, `MTU`, `CTRL_VQ` and `MQ`, and no offload (so `VERSION_1`'s 12-byte header is all zeros out and is refused in if it asks for an offload). With `MQ` and `CTRL_VQ` it uses one queue pair per CPU, at most the device's and eight, sets every queue up before it is ready and sets the pair count through the control queue after (a refusal fails the driver). Each pair has 128 one-page receive buffers posted and 128 one-page transmit buffers of the driver's; every queue interrupts through MSI-X entry 0, and the configuration change through it too when the device reports its link. Its `Ring` puts a frame, behind a zeroed header, into a free transmit page of the queue its flow hashes to. A returned receive buffer whose length is out of bounds or whose header asks for an offload is counted and dropped; the buffer is posted again. The MTU is the device's (1500 without `MTU`), at most what one page holds after the header.

### f.4 The gVNIC driver (GQI)

Following the Linux driver's order (`gve_main.c`, `gve_adminq.c`): reset (revision 0: the admin queue's page frame register to zero, waiting for it to read zero; revision 1: the driver status's reset bit, waiting for the device status's is-reset bit), the caller's quarantine release and bus mastering, the version string into its byte register, the admin queue (its page frame, or its address, length and the driver status's run bit), Describe Device. The format is GQI-QPL where the device lists it or lists no queue format (the format every gVNIC began with), GQI-RDA where it lists only that, and a device listing only DQO is refused. The driver asks for one queue pair per CPU, at most what the device has, its default, eight, and `(vectors - 1) / 2`. Configure Device Resources gives the event counters and the notification blocks (transmit queue `i` on block `i`, receive queue `i` on block `queues + i`, the management interrupt the MSI-X entry after them); each interrupt doorbell index the device writes is checked to lie inside BAR 2. In QPL format each transmit queue registers the device's pages-a-list count of pages and each receive queue one page a ring entry, refused if they pass the device's registered-page bound. Each queue is created and the doorbell and counter indices it is given are checked; every receive buffer is posted; every block is unmasked.

A frame sent is copied into a slot of its queue's buffers (slots of the largest frame rounded up to 2 KiB) and described by one packet descriptor; the doorbell carries the count of descriptors put. The event counter is the count the device finished, and is checked never to pass the count put. A frame received is one descriptor, or several with `PKT_CONT`, each naming its ring entry's buffer; the first's data starts two bytes in. A descriptor is new when its sequence number is the next (1 to 7, around); a fragment longer than a 2 KiB buffer, or a packet the device stops in the middle of, is the device's error; a frame flagged in error, shorter than a header or longer than the largest frame is dropped and counted. The receive doorbell carries the count of buffers posted.

Every MSI-X entry the driver uses carries its first MSI slot's message, so one interrupt wakes it for every block and the management interrupt: it then reads the device status (the link; a reset the device asks for ends the driver, and the device manager starts it again from a reset), takes every queue's work, acknowledges and unmasks every block, and goes round again while the device has more, at most sixteen times. Its rings are contiguous DMA memory (ABI 2.1, SDD 25 f.4). When its clients have gone it destroys its queues, unregisters its page lists, deconfigures its resources and resets the device.

Verify Driver Compatibility is not sent: its operating system type names no Moruna, and Linux drivers before it worked without it. Link speed is not asked for (reported as 0).

## g. Concurrency within the component

Each driver is single threaded: one dispatcher waiting on its listener, its connections and its interrupt, its card and server in one `Rc<RefCell<_>>`. A card's frames from every queue go through one hub in that thread.

## h. Behaviour

The CI boot: the device manager starts `net0` and `net1`, two virtio-net cards QEMU joins through its socket backend (each frame out of one leaves QEMU as a UDP datagram on the host to the other's port, and comes in at the other); `net-test` looks both up, runs the one-card suite against each and the pair suite over both, and streams frames each way; its last connection closes, each driver resets its card and ends.

## i. Configuration

None at run time. Constants: the protocol's (d.2); virtio-net's 128 buffers a queue each way and eight queue pairs at most; gVNIC's 2 KiB receive buffers, eight queue pairs, 17 MSI-X entries, ten seconds for an admin command or a reset, sixteen rounds an interrupt.

## j. Observability

Each driver prints its service name and what came up (`KE1 net0: virtio-net up: 52:54:00:4b:45:00, MTU 1500, 1 queue pairs for 4 CPUs, link up, no offloads`; gVNIC's line adds its format, its device options and its ring sizes) and, when it ends, the frames and bytes it moved and the malformed frames its device gave.

## k. Tests

Host tests run with `cargo test` in `kernel/` for `net-protocol`, `moruna-virtio` and `moruna-gve`, beside SDD 25's. The boot tests run on 1, 2 and 4 CPUs, judged by `check-boot.sh`.

| id | test | where | asserts |
|---|---|---|---|
| NT-T1 | the types | `drivers/net-protocol` | frames, batches, `Info`, `Link` and `Stats` round trip; a frame whose length runs past its message is refused; addresses and group bits |
| NT-T2 | the frame checks | `drivers/net-protocol` | the least and largest frames pass; short, long, group-source, empty and oversized batches, and a batch with one bad frame, are refused |
| NT-T3 | the flow hash | `drivers/net-protocol` | a flow's frames hash alike whatever their payload; ports and protocols part them; fragments hash by addresses; other frames hash to zero |
| NT-T4 | the hub | `drivers/net-protocol` | subscribe, a waiting receive, `InUse`, `NotSubscribed`, fan-out to every subscriber, drops counted for a full queue alone, one reply's bound, a closed connection's queue and waits gone, the link's generations and waiters, the counters |
| NT-T5 | the suites against a model | `drivers/net-protocol/tests` | the one-card suite against each of two model cards served by `Server`, and the pair suite over both, pass; a card that answers sends itself fails the refusals and the counters; a card that loses frames fails every pair case |
| NT-T6 | virtio-net's helpers | `drivers/virtio` | the configuration with and without each feature, each refusal; the header written; a received buffer's frame, every length refused, an offload refused; the pairs command; queue indices |
| NT-T7 | the one-card suite on virtio-net | boot: `net-test` | `KE1 conformance moruna.driver.net over net0: ok:` and `over net1: ok:` |
| NT-T8 | the pair suite on virtio-net | boot: `net-test` | `KE1 conformance moruna.driver.net over net0 and net1 on one link: ok:`: frames from 26 bytes to the largest arrive intact, a batch of 64 in order and none dropped, both ways |
| NT-T9 | frames a second | boot: `net-test` | `KE1 net performance:` lines, `net0` to `net1` and back with 64-byte frames and `net0` to `net1` with 1514-byte frames: frames arrived, a second, Mbit/s, lost and dropped; at least one frame arrives. Printed, not judged |
| NT-T10 | gVNIC's description | `drivers/gve-core` | the fields; QPL where listed or no format is listed, RDA where only it is, DQO alone refused, an option requiring a feature refused; every length, ring, MTU, address and count refusal |
| NT-T11 | gVNIC's admin queue | `drivers/gve-core` | a command executed; a failed status named; a timeout; an event counter that trails or jumps is `Malformed`; Create RX Queue's fields at their offsets |
| NT-T12 | gVNIC brought up | `drivers/gve-core` | revision 0 and 1, QPL and RDA: queues, blocks, format, page lists, receive buffers posted, version string, contiguous rings; queue counts bounded by the device, its default and the interrupts; doorbell indices outside BAR 2, a reset that never ends, a failed queue creation and a DQO device refused |
| NT-T13 | gVNIC sends | `drivers/gve-core` | frames from 60 bytes to the largest reach the model intact in both formats only after the doorbell; completions free slots; a full queue says so and frees as the device finishes; a counter past what was sent is `Malformed`; jumbo frames cross queue page list pages |
| NT-T14 | gVNIC receives | `drivers/gve-core` | single and three-fragment frames delivered; a frame flagged in error and a runt dropped and counted; buffers posted again; a thousand frames round the ring with the sequence numbers wrapping; an oversized frame dropped whole; a fragment longer than a buffer and a packet stopped in the middle are `Malformed` |
| NT-T15 | gVNIC shut down | `drivers/gve-core` | queues, page lists and resources released and the device reset, both revisions; the status read (a reset asked for, the link); blocks acknowledged and unmasked |

gVNIC has no boot test: QEMU has no gVNIC. KE3 boots `gve` on Google Cloud.

## l. Implementation notes for the agent

- QEMU's `--without-default-features` build (CI's) has no `slirp`, so the boot's cards use the socket backend, which needs no library.
- `romfile=` keeps QEMU from looking for a virtio-net option ROM the boot does not need.
- QEMU's socket backend has one queue, so QEMU offers a virtio-net card one queue pair however `mq` is set: the boot does not exercise multiqueue (m.2).

## m. Open items

1. **gVNIC on hardware.** Every gVNIC line is tested against a model built from the published interface, not against a device. KE3 boots it on Google Cloud; until then, the reset at start on a revision 1 device, the version string, skipping Verify Driver Compatibility, the queue page list ids and the management interrupt's MSI-X entry are the published driver's behaviour as read, unconfirmed.
2. **Multiqueue unexercised.** virtio-net's several pairs and the control queue command are host tested (NT-T6) but no boot offers more than one pair (l.3). A tap backend with `queues=N` would, and needs privileges the CI runner's QEMU does not take.
3. **DQO not built.** Google Cloud's newest machine families may offer gVNIC only in DQO; the driver refuses such a device by name. DQO's descriptors, completion queues and buffer queues are a second format to build in `moruna-gve`.
4. **No offloads.** Checksum and segmentation offloads would need per-frame metadata in the protocol (a 1.1).
5. **Link speed.** gVNIC's Report Link Speed and virtio-net's `SPEED_DUPLEX` are not read; `speed_mbps` is 0.

## n. Traceability

| requirement (BOARD EK KE1 brief) | here |
|---|---|
| a net protocol with `moruna-svc`: frames in and out, MAC, MTU, link state, offloads declared | d.1, d.2, f.1, f.2; NT-I1 to NT-I5; NT-T1 to NT-T5 |
| virtio-net first, multiqueue where offered | f.3; NT-T6 to NT-T9; m.2 |
| gVNIC against Google's published interface, unit tested against a model, marked unproven | f.4, e.2; NT-T10 to NT-T15; m.1 |
| conformance: loopback frames, MTU limits, malformed frames refused | `suite`, `pair_suite`; NT-T5, NT-T7, NT-T8 |
| boot checks on 1, 2 and 4 CPUs, frames out and in, packets a second | h, k; NT-T7 to NT-T9 |
| the QEMU flags through the boot description | SDD 25 e.4, SDD 23 f.8; DR-T19 |

## o. Deferred

DQO (m.3), offloads (m.4), zero-copy frames (f.1), per-device IOMMU domains (SDD 25 m.3).
