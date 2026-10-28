# Moruna SDD 25: Drivers as processes (`kernel/drivers`, `kernel/devices`), and the kernel calls drivers need

**Document type:** software design document, component 25
**Status:** DRAFT · 2026-10-29 (becomes HANDOFF-READY when section m is empty; the human flips it)
**Parent:** `BOARD.md` section EK, item KB3 (Brackly, 2026-10-28): "first drivers as processes: virtio console, virtio block, vsock", each an ordinary unprivileged process written only against `moruna-svc` and the generic kernel calls drivers need; SDD 23 section m item 1 and f.9 (MSI-X instead of level-triggered lines)
**Decisions:** none new; MK1 (OSTD) stands
**Component location:** `kernel/drivers/` (`moruna-mmio`, `moruna-pci`, `moruna-virtio`; the protocols `block-protocol`, `console-protocol`, `vsock-protocol`; the drivers `virtio-blk`, `virtio-console`, `virtio-vsock`; the tests `block-test`, `console-test`, `vsock-test`, `driver-isolation`), `kernel/devices/` (`moruna-driver-table`, `devices-protocol`, `devices`, the device manager); the kernel additions in `kernel/core/src/{syscall,interrupt,memory,object,process,boot}.rs`, `kernel/abi` (ABI 1.2), `kernel/sys`; `kernel/tools/{boot,check-boot,check-examples}.sh`, `kernel/tools/{disk-image,vsock-peer}.py`, `kernel/tools/image` (the `driver` directive), `kernel/initramfs/boot.image`
**Consumes:** `moruna-svc` (protocols, the dispatcher, the conformance harness), `moruna-sys` through it, `moruna-abi`, OSTD 0.19's `IrqLine`, `DmaStream`, `IoMem` and ACPI MCFG. **Consumed by:** KC2 (the image filesystem over the block protocol), KD1 and the engine's spill (block), KE1 (NVMe and the cloud network card as drivers of the same shape), every third-party driver.

**Decisions worth your eye:** (1) the kernel gains five calls and no knowledge of any bus or device: an interrupt delivered by message, pins of DMA memory, a resource's range, and killing a process; PCI enumeration, BARs, MSI-X tables and virtio are userland code; (2) a closed pin goes to its DMA resource's quarantine instead of back to the allocator, because a device may still be writing it, and only `dma_release` (after the driver has reset its device) frees it, so a driver that crashes mid-request cannot hand a device's write to a page's next owner; (3) without an IOMMU (QEMU's default machine has none) a driver is trusted with every page it pins: that is why only the supervisor grants the DMA right, and grants it per device; (4) the device manager is a process of its own that the supervisor hands its device roots, so the supervisor stays the single holder of every root and the device work is replaceable; (5) the driver table is a line per driver in the boot description, rendered into the image: adding a driver touches no kernel file (DR-T16, DR-T17); (6) the virtio library is written for Moruna rather than on `virtio-drivers`, whose hardware abstraction is an `unsafe` trait handing out raw pointers, which a driver that forbids unsafe code cannot implement; (7) the console driver's output goes to a virtio console (QEMU's `virtconsole`), whose file the boot check reads beside the serial log, so a driver's line never splits a kernel line.

---

## a. Purpose and boundary

The microkernel (SDD 20) holds processes, memory, threads, interrupts, timers, messages and handles; the SDK (SDD 23) makes a process a server. This component makes drivers out of those: a device is reached only by a process that holds its windows, its interrupts and a right to hand it memory, and that serves a published protocol to clients that found it by name.

It owns: the generic kernel calls drivers and supervisors need (d.1); width-exact register access over mappings (`moruna-mmio`); PCI configuration space, BARs, capabilities and MSI-X (`moruna-pci`); modern virtio over PCI (`moruna-virtio`); the block, console and vsock protocols and their conformance suites; the three drivers; the device manager, its driver table, its directory and its control protocol; the boot's devices and their checks.

It refuses to know: filesystems (KC2), the network stack (KE2), NVMe and real network cards (KE1), the engine. The kernel refuses to know PCI, virtio and every device: the test of every kernel addition here is that a driver for a bus or device the kernel has never heard of needs no kernel change.

## b. Vocabulary

**Driver.** A process that holds one device's resources and serves a protocol for it.

**Device manager.** The process `devices`, which enumerates PCI, starts and restarts drivers, and serves the directory. It does the supervisor's device work with the device roots the supervisor hands it.

**Window.** A device memory resource narrowed to a range a driver may claim: its function's 4 KiB of configuration space, or one BAR rounded out to whole pages.

**Pin.** A kernel object holding a DMA memory object's pages mapped for a device (`memory_pin`).

**Quarantine.** A DMA resource's set of pins whose handles closed before the resource's holder said the device was done with them.

**Driver table.** The lines `driver VVVV:DDDD PROGRAM SERVICE` that say which program drives which device and the name its service is published under.

**Directory.** The device manager's protocol that turns a service name (`block0`) into a connection to its driver.

## c. Invariants

- **DR-I1.** No kernel file names a bus, a device or a device protocol: PCI, virtio, MSI-X tables and ECAM layout are userland code. A driver is added by a driver table line and a program (DR-T16, DR-T17).
- **DR-I2.** A driver holds exactly: a window over its function's configuration space, a window over each of its memory BARs, a range of MSI slots, a DMA resource for its function's requester id, a console and its listener. Every claim outside those ranges is refused by the kernel (`AccessDenied`) before OSTD is asked (DR-T14).
- **DR-I3.** Only the first process receives `root.ecam`, `root.msi` and `root.dma` (KK-I11); the device manager receives them from the supervisor; a driver receives only resources narrowed to its own function.
- **DR-I4.** An MSI slot is held by at most one live interrupt object; it is freed when that object's last handle closes (DR-T7).
- **DR-I5.** `memory_pin` pins only a memory object made with `memory_flags::DMA`, only through a DMA resource handle with `WRITE`, and needs `READ` on the memory handle, and `WRITE` as well when the device writes the pages.
- **DR-I6.** A pin's pages are never returned to the allocator while a device may use them: closing a pin moves it to its DMA resource's quarantine, and only `dma_release` on that resource frees them. A driver calls `dma_release` only after its device has finished with every quarantined pin: after a reset (at start), or after the device returned the chains whose pins it closed.
- **DR-I7.** A device memory region holds its range exclusively while it lives; once its last handle closes, the range can be claimed again, so a restarted driver claims the windows its predecessor held.
- **DR-I8.** A driver resets its device before it lets it master the bus and before it releases its quarantine, so the device uses no buffer a previous driver gave it.
- **DR-I9.** Data moves between a block device and a client by DMA into and out of the client's memory object; no byte of a block transfer travels in a message.
- **DR-I10.** A client's request in flight when its driver ends is answered, or its connection closes (`PeerClosed`); it never waits for ever (DR-T11).
- **DR-I11.** A driver that ends is started again by the device manager with the same resources, and a lookup that arrives meanwhile is answered with a connection to the new driver.
- **DR-I12.** Every value a device writes into a ring is checked before it is believed: a used id that names no chain in flight is an error, never an index.
- **DR-I13.** The boot powers off only when every client, driver and the device manager have ended (KK-I12): the device manager closes its drivers' listeners when its last client has gone, and each driver ends when its own clients have.

## d. Interfaces

### d.1 Kernel calls added (ABI 1.2; SDD 20 d.2)

| nr | call | arguments | returns | rights needed |
|---|---|---|---|---|
| 7 | `memory_create` (flags since 1.2) | size, flags (`memory_flags::DMA` = 1; 0 as in ABI 1.0) | memory object | |
| 25 | `interrupt_create_msi` | MSI resource, slot | interrupt (`rax`); `data << 32 \| address` (`rdx`), the address's high half zero | `WRITE` on the resource; the slot inside its range |
| 26 | `memory_pin` | DMA resource, memory object, flags (`pin_flags::DEVICE_WRITES` = 1), addresses, capacity | pin (`rax`), page count (`rdx`); on `BufferTooSmall`, `rdx` = pages needed | `WRITE` on the resource; `READ` on the memory, and `WRITE` with `DEVICE_WRITES` |
| 27 | `dma_release` | DMA resource | pins released | `WRITE` |
| 28 | `resource_info` | resource, 24-byte buffer | 0; the buffer holds `u32 kind`, `u32` zero, `u64 base`, `u64 length` | |
| 29 | `process_kill` | process | 0 | `WRITE` |

Errors, in the order checked (SDD 20 e.5): every handle is looked up first (KK-I2), then its type, then its rights, then the other arguments. `interrupt_create_msi`: a slot at or above 224 is `InvalidArgs`, outside the resource `AccessDenied`, held by a live object `AccessDenied`, no free vector `Limit`; a resource of another kind is `AccessDenied`. `memory_pin`: unknown flags or a memory object not made for DMA are `InvalidArgs`; fewer addresses than pages is `BufferTooSmall`; a quarantine holding 4096 pins is `Limit`. `process_kill`: a process not started or already ended is `BadState`. `memory_create`: an unknown flag is `InvalidArgs`.

`moruna-sys` wraps them: `Memory::create_dma`, `Interrupt::create_msi -> (Interrupt, Msi { address, data })`, `Pin::create`, `Pin::pages` (the pages a pin needs, pinning nothing), `dma_release`, `Handle::resource_info -> ResourceInfo { kind, base, len }`, `Process::kill`; and for drivers, `Mapping::{read8, read16, read32, read64, write8, write16, write32, write64}` (one access of that width at a naturally aligned offset), `Memory::map_detached` and `DeviceMemory::map_detached` (a mapping that does not borrow its handle; a mapping keeps its pages whatever becomes of the handle), `Process::start_with` (a start with any number of handles). `moruna-svc` re-exports them and gains `Dispatcher::channels` (connections and listeners, without watched objects).

### d.2 Resources and manifest sources

| kind | range | root source | narrowed by the device manager to |
|---|---|---|---|
| device memory | physical addresses `0 .. 2^52` | `root.device` | one BAR, rounded out to pages |
| device memory (ECAM) | exactly ACPI MCFG's first segment, from its first bus to the end of its last | `root.ecam` | one function's 4 KiB |
| MSI (`ResourceKind::Msi` = 4) | slots `0 .. 224` | `root.msi` | the function's MSI-X entries, 1 to 4 slots |
| DMA (`ResourceKind::Dma` = 5) | PCI requester ids `0 .. 65536` | `root.dma` | the function's requester id |

`root.ecam`, `root.msi` and `root.dma` are given to the first process only (KK-I11). A boot whose manifest gives `root.ecam` on firmware with no MCFG is refused.

### d.3 `moruna.driver.block` 1.0

```rust
1 => fn info() -> Info;                                     // sector_size, sectors, max_sectors, read_only
2 => fn read(sector: u64, count: u32, buffer: Memory) -> Memory;
3 => fn write(sector: u64, count: u32, data: Memory) -> Memory;
4 => fn flush() -> ();
errors { 1 => OutOfRange; 2 => BadCount; 3 => BadBuffer; 4 => ReadOnly; 5 => Io; }
```

A sector is 512 bytes. `buffer` and `data` are DMA memory objects (`Memory::create_dma`) at least `count * 512` bytes long; the transfer uses their first bytes, and the reply hands the same object back. A read needs `READ` and `WRITE` on the handle (the device writes it), a write needs `READ`. `max_sectors` is what one descriptor chain carries: `min(seg_max, queue size - 2, 256)` pages. `flush` is answered once every write answered before it is on stable storage; a device without a write cache answers it at once.

### d.4 `moruna.driver.console` 1.0

```rust
1 => fn write(text: Vec<u8>) -> u32;   // answered with the length once the device has taken it
2 => fn read_line() -> Vec<u8>;        // the next whole line, without its newline
errors { 1 => Empty; }
```

Lines longer than 4096 bytes arrive in 4096-byte pieces. Requests are answered in order: writes one after another, reads in the order they came.

### d.5 `moruna.driver.vsock` 1.0

```rust
1 => fn local_cid() -> u64;
2 => fn connect_to(cid: u64, port: u32) -> u32;            // a stream, once the peer accepts
3 => fn listen(port: u32) -> ();
4 => fn accept(port: u32) -> (u32, u64, u32);             // stream, peer cid, peer port
5 => fn send(stream: u32, data: Vec<u8>) -> u32;          // once queued within the peer's credit
6 => fn recv(stream: u32, max: u32) -> Vec<u8>;           // empty: the peer sends no more
7 => fn close(stream: u32) -> ();
errors { 1 => Refused; 2 => Reset; 3 => NoStream; 4 => InUse; 5 => NotListening; 6 => Empty; }
```

The streams and listeners a connection opens belong to it: another connection's stream id is `NoStream`, and closing the connection resets its streams and drops its listeners. One `recv` waits on a stream at a time (`InUse` for a second). The request is named `connect_to` because `protocol!` gives every client a `connect` of its own.

### d.6 `moruna.devices.directory` 1.0

```rust
1 => fn lookup(name: String) -> Channel;                  // a new connection to the service
2 => fn list() -> Vec<(String, u32, String, u32)>;        // name, vendor << 16 | device, program, starts
errors { 1 => NotFound; 2 => Unavailable; }
```

`lookup` answers with a raw connection to the driver; the client says `Hello` on it. A lookup for a driver that has ended is answered once the device manager has started it again. Every process the boot description links to a `directory*` slot of the device manager may look up every service.

### d.7 `moruna.devices.control` 1.0

```rust
1 => fn restart(name: String) -> u32;                     // its start count once it runs again
2 => fn sandbox(name: String) -> Vec<Handle>;             // config window, BAR windows, MSI slots
3 => fn windows(name: String) -> (u64, Vec<(u32, u64, u64)>);  // config address; BAR index, base, length
errors { 1 => NotFound; 2 => Failed; }
```

Only the processes the boot description links to a `control*` slot reach it. `sandbox` duplicates the resources the named driver holds, less its DMA right, for tests of confinement (DR-T14).

### d.8 The libraries

- `moruna-mmio`: `trait Region` (`len`, `read8/16/32/64`, `write8/16/32/64`, `write64_split`, `read_bytes`, `write_bytes`); `Sub` (a bounded part of a region); `impl Region for moruna_svc::Mapping` on the kernel; `Fake` (plain memory with the same alignment and bounds rules) on a host.
- `moruna-pci`: `Address` (bus, device, function; `ecam_offset`, `requester_id`), `Config<R>` (`vendor`, `device`, `present`, `class`, `header_type`, `multifunction`, `command`, `enable`, `disable`, `capabilities`, `find`, `bar_base`, `size_bars`, `msix`), `Bar` (`Memory { base, size, prefetchable, wide }`, `Io`; `pages`), `Msix` (`program`, `enable`); on the kernel, `function::Function` (`open` from the start slots `config`, `bar0` to `bar5`, `msi`, `dma`; `bar`, `take_bars`, `interrupt`, `take_dma`, `enable_bus_master`, `config_base`).
- `moruna-virtio`: `Layout::read` (the vendor capabilities), `Transport<R>` (`reset`, `device_features`, `negotiate`, `queues`, `queue_max`, `setup_queue`, `set_config_vector`, `driver_ok`, `fail`, `notify`, `isr`, `device_config`), `Queue<M>` (`new`, `add`, `pop_used`, `free`, `in_flight`), `Buffer`, `Used`; on the kernel, `device::Device::open(kind, wanted, required) -> (Device, Interrupt)`, `Device::queue`, `ready`, `notify`, and `DmaBuffer` (driver-owned DMA memory, mapped and pinned, with each page's device address).

### d.9 What a driver starts with

| slot | what |
|---|---|
| `console` | the debug console, write only |
| `config` | a device memory resource over its function's configuration space |
| `bar0` to `bar5` | a device memory resource over each memory BAR, rounded out to pages |
| `msi` | an MSI resource over its slots |
| `dma` | a DMA resource for its function's requester id |
| `listen` | the listener its clients' connections arrive on |

## e. Data model, formats and state machines

### e.1 Objects

The kernel's tenth object type is the pin (`ObjectType::Pin` = 10): a pinned memory object's pages and their DMA mappings. It has no signals and no rights; it is only ever closed. A DMA resource carries its quarantine (at most 4096 pins). An interrupt object is either a GSI line (SDD 20 f.7) or an MSI line holding its slot.

### e.2 The MSI message

Compatibility format (no interrupt remapping): address `0xfee0_0000 | destination << 12`, with the destination the local APIC id 0 (the boot processor on QEMU and on the machines Moruna targets), physical mode; data the vector, fixed delivery, edge. Remappable format, when OSTD has interrupt remapping: address `0xfee0_0000 | handle[14:0] << 5 | 1 << 4 | 1 << 3 | handle[15] << 2`, data 0. A driver writes the address and data into an MSI-X table entry, unmasked, and turns MSI-X on with the function mask clear.

### e.3 The driver table

```text
moruna-drivers 1
driver 1af4:1042 virtio-blk block
```

At most 32 lines; one driver per vendor and device id; one line per service name; programs 1 to 25 bytes and services 1 to 30 bytes of `[a-z0-9._-]`. The boot image description writes a table as `driver` lines under the device manager's `process` line; `moruna-image` renders it to `etc/NAME.drivers`, gives it as the slot `drivers`, and gives each program's image as the slot `driver.PROGRAM`. The device manager names the n-th device a line matches, in PCI order, `SERVICEn`; its driver process has that name.

The CI boot's table: `1af4:1042 virtio-blk block`, `1af4:1043 virtio-console console`, `1af4:1053 virtio-vsock vsock`, and, from DR-T17's commit, `1234:11e8 edu edu`.

### e.4 The CI boot's devices

`kernel/tools/boot.sh` gives QEMU's q35 machine: virtio consoles at 00:03 (output to `OUT/console0-CPUS.log`) and 00:04 (input from `OUT/console1-input.txt`), a 4 MiB raw disk as virtio-blk at 00:05 (`OUT/disk-CPUS.img`, pattern A written by `disk-image.py` before each boot), `vhost-vsock-pci` with guest cid 3 at 00:06 when `/dev/vhost-vsock` is usable (with `vsock-peer.py` as the host's side), and QEMU's `edu` test device at 00:07. Every virtio device is modern only (`disable-legacy=on`), so its device id is `0x1040 +` its virtio type. `OUT/vsock-CPUS` says whether the vsock was attached and, if not, why.

### e.5 A driver's life

`started` (by the device manager, with duplicates of its resources and a fresh listener) → `serving` (after it has reset its device, released its quarantine, set up its queues and told the device it is ready) → `ended` (when its listener and every client connection have closed, or it fails, or it is killed). An `ended` driver is started again unless the device manager is shutting down.

## f. Algorithms and policies

### f.1 Narrowing for a device

`resource_narrow` (SDD 20 f.10) works on every kind with a range: device memory (page aligned), interrupts and MSI slots and DMA requester ids (any range). A process resource is still `InvalidArgs`. A DMA resource is narrowed to one requester id, so each device has its own resource object and its own quarantine; the device manager keeps that object and gives each of the device's drivers a duplicate, so the quarantine outlives a driver.

### f.2 MSI

`interrupt_create_msi` checks the slot (`< 224`, inside the resource), takes it in a global table (one live object per slot), allocates an OSTD IRQ line with no pin, registers the callback that raises `SIGNALED`, and returns the message for the line's vector (e.2). OSTD acknowledges the local APIC after the callback; `interrupt_ack` clears `SIGNALED`, as for a GSI line. A driver acknowledges before it reads its rings, so a completion that lands while it reads raises the line again and is not lost. Dropping the object frees the vector and the slot.

### f.3 Device memory claimed again

OSTD hands an MMIO range out once and never takes it back (its `IoMem` is never recycled). The kernel keeps two lists under one lock: every range it acquired from OSTD (with its `IoMem`), and the ranges live device memory regions hold. A claim is refused if a live region overlaps it; inside a range acquired before it is a slice of that `IoMem`; partly overlapping one acquired before it is refused (`AccessDenied`); otherwise it is acquired from OSTD and remembered. Dropping a region removes its range from the live list. So the device manager can map the whole ECAM window, let it go, and each driver claims its own 4 KiB slice of it; and a driver that replaces one that ended claims the same windows.

### f.4 Pins, the quarantine, and DMA without an IOMMU

`memory_pin` maps each page of the memory object for DMA with OSTD's `DmaStream::map` (one stream a page) and returns each stream's device address: the IOMMU's address when OSTD has DMA remapping, the physical address otherwise. The addresses are copied out before the pin exists; a pin whose addresses could not be reported ends at once, since no device has them. Closing a pin's last handle does not free its pages: the pin goes to its DMA resource's quarantine, because the device may still be reading or writing them (a driver that crashed mid-request closed its pins without telling the device anything). `dma_release` drops the quarantine. A driver calls it after it has reset its device at start (releasing whatever a predecessor left), and after it has taken completed chains off its used ring and closed their pins (every pin in the quarantine is then one the device has finished with). A quarantine is bounded at 4096 pins; `memory_pin` is `Limit` beyond.

**Trust without an IOMMU.** QEMU's default q35 machine, and the CI boot, has no IOMMU. Without one, a device writes wherever it is told, so a driver that holds a DMA right can have its device read or write any physical page, the kernel's included, by giving it an address it never pinned. The kernel cannot stop that; the pin only guarantees that the pages a well-behaved driver hands out stay allocated while the device may use them. A driver is therefore trusted with the memory it is given, and more: with all memory. That is why `root.dma` goes to the first process only, why the supervisor alone (through the device manager) grants DMA rights, and why it grants one per device, never to a process that is not a driver. With an IOMMU, OSTD's `DmaStream` maps each pinned page into the IOMMU's page table and the device can reach those pages and nothing else; the per-device DMA resource is then the unit a per-device IOMMU domain attaches to. OSTD 0.19 keeps one IOMMU domain for every device, so per-device domains are KE's (m.3).

### f.5 Register access

Every access to a register or ring goes through `Region` at its own width and natural alignment: one volatile load or store, never a Rust reference to device or shared memory (`moruna-sys` holds the only unsafe code, in the same place as KB1's and KB2's). A 64-bit register a device exposes as two halves is written low half first. Ring publication is ordered with `fence(SeqCst)`: descriptors and the ring entry before the index that publishes them, the used index before the entries it covers.

### f.6 PCI

Function `bus:device.function` is at `bus << 20 | device << 15 | function << 12` in the ECAM window; the device manager numbers buses from the window's first bus (m.2). A function is absent when its vendor reads `0xffff`; a device's functions 1 to 7 are read only when function 0 says it is multi-function. A BAR is sized with decoding off (command register's I/O and memory bits cleared, every BAR register written all ones, read back, restored, decoding restored); a 64-bit BAR spans two registers; an absent BAR reads back zero. The capability list is walked from offset `0x34` and refused if it loops, points into the header or runs past 48 entries. MSI-X: the capability gives the table's BAR and offset and the entry count; an entry is written masked, then its address and data, then unmasked; the capability's enable bit is set with the function mask clear.

### f.7 Virtio

`Layout::read` takes the first vendor capability of each configuration type: common (1), notify (2, with its offset multiplier), ISR (3), device (4); the PCI configuration access type (5) and unknown types are skipped; a common structure shorter than its fields or a structure outside its BAR's window is refused. `Device::open` (the driver's side, d.8): open the function (f.8); refuse anything but a modern virtio device of the requested type; enable memory decoding with INTx off; require MSI-X, allocate the function's first MSI slot, program entry 0 and enable MSI-X; reset the device and wait until its status reads zero; `dma_release`; enable bus mastering; acknowledge, negotiate (`VERSION_1` required, `ACCESS_PLATFORM` taken whenever offered, the driver's wanted and required bits, `FEATURES_OK` checked after it is set); no configuration-change vector. Each queue gets its own DMA memory of three pages (descriptor table, driver ring, device ring), is sized to the largest power of two the device and the driver allow (at most 256, so the descriptor table fills one page), and interrupts through MSI-X entry 0, which every queue of a Moruna driver shares. Device configuration is read again until the configuration generation is the same before and after.

A split queue keeps its free descriptors and each chain's length in the driver; `add` writes a chain's descriptors (`NEXT` between parts, `WRITE` on parts the device writes), the ring entry, and then the index; `pop_used` reads the device's index, refuses an id that names no chain in flight (DR-I12), and frees the chain.

### f.8 A driver's function

`Function::open` takes the slots of d.9, checks each is a device memory resource, claims and maps each window, and records where each BAR's first byte is in its window (a BAR need not start on a page). The claims live as long as the driver, so no other holder of an overlapping resource can claim the same pages while it runs.

### f.9 The block driver

A request is validated (count `1 ..= max_sectors`, the sectors inside the device, not a write to a read-only device), the client's memory object is pinned (`Pin::pages` then `Pin::create`; any refusal is `BadBuffer`, as is a buffer smaller than the transfer), and the request queued. It goes to the device as one chain: a 16-byte header in a slot of the driver's header page (type, zero, sector), one part per page of the transfer, and a status byte in the same slot. Requests that find no free slot or too few free descriptors wait in order. When the device returns a chain, its status byte decides the answer (`Io` for anything but zero), the pin is closed (to the quarantine), the memory object goes back in the reply, the quarantine is released, and waiting requests go to the device.

### f.10 The console driver

Port 0 of a virtio console without multiport: receive queue 0, transmit queue 1. Eight 512-byte receive buffers stay posted; what arrives is cut into lines (a trailing carriage return dropped, a line of 4096 bytes delivered as it is), and waiting `read_line`s are answered in order. A write is copied into a pinned 64 KiB buffer and sent as a chain of pages; it is answered when the device returns it, and the next write starts.

### f.11 The vsock driver

Every packet is a 44-byte header (source and destination cid and port, payload length, type `STREAM`, operation, flags, the sender's buffer size and how much of the stream it has consumed) and its payload, in one page: sixteen receive pages stay posted, sixteen pages carry what is sent, and packets that find no free page wait in order. A stream offers its peer 64 KiB and tells it how much its client has read with every packet it sends, and in a credit update once its client has read half the buffer since the last one; it sends no more than the peer's buffer size less what it has sent and the peer has not consumed. A request to a listened port is answered with a response and the new stream queued for `accept`; a request to no listener, or a packet for no stream, is answered with a reset. A shutdown of the peer's sending side ends `recv` with nothing after the data; a reset ends the stream and answers its waiting requests with `Reset` (`Refused` while connecting). A transport reset event resets every stream and reads the cid again.

### f.12 The device manager

1. It reads the driver table and takes each program's image, then waits (up to 120 s) for the supervisor's message: one message carrying the process, device memory, ECAM, MSI and DMA roots, in that order.
2. It claims and maps the whole ECAM window (`resource_info` gives it), walks every bus, device and function, records each function's ids and configuration address, and sizes the BARs and reads the MSI-X entry count only of endpoints the table names. Then it drops the window.
3. For each named function, in PCI order: a function whose BAR pages overlap an earlier named function's gets no driver and is reported; otherwise it narrows the ECAM to the function's 4 KiB, the device root to each memory BAR's pages (a BAR the firmware left at zero is an error), the MSI root to the next 1 to 4 slots (its MSI-X entries, at least one), and the DMA root to its requester id, and names it `SERVICEn`.
4. It starts each driver (`process_create` under the service's name, the program's image, `start_with` the slots of d.9, each resource a duplicate) and watches a duplicate of the process handle for `TERMINATED`. A driver that ends is started again at once with the same resources and a new listener; lookups that arrived meanwhile and pending restarts are answered when it runs. `restart` kills the driver (`process_kill`) and is answered by the same path.
5. It serves the directory on every `directory*` slot and control on every `control*` slot of its start page, until `Dispatcher::channels` is zero: every client has gone. Then it stops restarting, drops the drivers' listeners, and waits up to 120 s for every driver to end; one that does not is a failure naming it.

### f.13 Extensibility

A third party's driver is a program written against `moruna-svc` (and `moruna-pci` and `moruna-virtio` if it likes) and one `driver` line in the boot description; `check-examples.sh` refuses any commit that changes `kernel/drivers/` or `kernel/examples/` together with a file of the kernel or its ABI (DR-T16). DR-T17 is that rule exercised: the `edu` driver was added in a commit touching only `kernel/drivers/`, `kernel/initramfs/boot.image` and `kernel/Cargo.lock`.

## g. Concurrency within the component

Every driver and the device manager are single threaded: one dispatcher, waiting on its listener, its connections and its interrupt or its drivers' processes in one `wait`, and state shared between handlers and watches through `Rc<RefCell<_>>` within that thread. In the kernel, the MSI slot table and the device memory claims are each one spin lock, taken alone; a DMA resource's quarantine is a spin lock taken alone, and the pins it releases are dropped outside it.

## h. Behaviour

The CI boot: the supervisor runs its tests, hands the device manager its roots and exits; the device manager starts `console0`, `console1`, `block0` and (with a vsock) `vsock0`; the tests look them up, drive them, and end; the device manager closes the listeners, the drivers end, it ends, and the machine powers off with the supervisor's success.

## i. Configuration

None at run time. Constants: 224 MSI slots, 4 slots a function at most, 4096 pins a quarantine, 256 entries a queue at most, 120 s for the supervisor's message and for drivers to end, the block driver's 128 header slots and 256 pages a transfer, the console's eight 512-byte receive buffers and 64 KiB transmit buffer, the vsock driver's sixteen pages each way and 64 KiB a stream.

## j. Observability

Each driver prints its service name and what came up (`KB3 block0: virtio-blk up: 8192 sectors of 512 bytes, ...`) and what it served when it ends. The device manager prints the drivers it started and the functions no line names, each restart, and a summary when it ends. The kernel prints a killed process's end (`kk: process NAME was killed through a handle to it; ...`).

## k. Tests

Host tests run with `cargo test` in `kernel/` for `moruna-mmio`, `moruna-pci`, `moruna-virtio`, `moruna-driver-table` and `moruna-image`, in CI's `kernel` job beside KB1's and KB2's; boot tests run in the same job on 1, 2 and 4 CPUs, judged by `check-boot.sh`, which reads `console0`'s file with the serial log, checks the disk image on the host, and judges the vsock lines by whether the boot had a vsock.

| id | test | where | asserts |
|---|---|---|---|
| DR-T1 | `dr_t1_*`, `kk_t1_*` | `abi` | the MSI message packs and unpacks; the new flags, ranges, object type, resource kinds and syscall numbers |
| DR-T2 | `dr_t2_*` | `abi::manifest` | `root.ecam`, `root.msi`, `root.dma` parse, render and round trip, carry a root's rights, and are refused outside the first process |
| DR-T3 | `dr_t3_*` | `drivers/mmio` | width, alignment and bounds of a window and of a part of one; read-only refusal |
| DR-T4 | `dr_t4_*` | `devices/table` | the driver table parses, renders and round trips; every refusal by line |
| DR-T5 | `dr_t5_*` | `drivers/pci` | addresses and requester ids; header fields; BARs sized with decoding off and restored (32-bit, 64-bit, I/O, absent); a wide BAR in the last register refused; the capability walk and its refusals; MSI-X read, programmed and enabled |
| DR-T6 | `dr_t6_*` | `drivers/virtio` | the layout from the capabilities; a short or misplaced structure refused; reset, negotiation (legacy, missing and refused features), queue setup, notify, ISR and the configuration generation against a model device; chains out and back, out of order and around the rings; a used id that names no chain refused |
| DR-T7 | the kernel calls | boot: `supervisor` | `KB3 kernel calls: ok:`: `resource_info` of every root; an MSI slot's message, once at a time, only inside its resource, freed on close; `memory_create`'s unknown flag; pins (too few addresses, `Pin::pages`, the addresses, not DMA memory, rights, no DMA resource), the quarantine and `dma_release`; `process_kill` refused before a start and after an end, and a kill (`kk: process killee was killed`); a device page claimable again once released |
| KK-T8 | isolation (extended) | boot: `isolation` | the five new handle-taking calls, with the sixteen before, answer `BadHandle` for every value that names nothing in the caller |
| DR-T8 | `dr_t8_*` | `tools/image` | `driver` lines render the table, give it and each image once, and are refused by line |
| DR-T9 | the disk | boot: `block-test`; host: `disk-image.py check` | `KB3 block: ok:`: every sector of pattern A read through `block0` by DMA into a memory object and checked; pattern B written, flushed and read back; after the boot the image file holds pattern B |
| DR-T10 | the block suite | boot: `block-test` | `KB3 conformance moruna.driver.block: ok:`: SV-T7's shared checks and the protocol's own (info, reads at the start and at the end at the largest size, out of range, bad counts, bad buffers of each kind, a write read back and restored, flush, a handle not a memory object) |
| DR-T11 | a crashed driver | boot: `block-test`, `devices` | `KB3 driver crash: ok:`: 32 reads in flight, `block0` killed through control; each read answered or ended `PeerClosed`, at least one closed, none waits past its deadline; `devices` starts it again (`start 2`); a read through a new connection returns the disk's data |
| DR-T12 | the consoles | boot: `console-test`; `console0`'s file | `KB3 console: ok:`, the three lines and the line read from `console1`'s input in `console0`'s file, `KB3 conformance moruna.driver.console: ok:` |
| DR-T13 | vsock | boot: `vsock-test`; host: `vsock-peer.py` | with a vsock: `KB3 vsock: ok:`, `KB3 conformance moruna.driver.vsock: ok:`, and both of the peer's `ok` lines (the guest connected to port 5000; the host connected to the guest's port 5001 and was answered); without one: `KB3 vsock: no device:` and the boot's reason |
| DR-T14 | a driver's confinement | boot: `driver-isolation` | `KB3 driver isolation: ok:`: holding `block0`'s resources less DMA, refused `console0`'s configuration window and each of its BARs, the windows either side of its own, a wider window, an MSI slot past its own, and a pin through each resource it holds |
| DR-T15 | the device manager | boot: `devices` | `KB3 devices: started`, each driver's `up` line, the restart, `KB3 devices: ok:` (every driver ended), each driver's and the device manager's exit 0 |
| DR-T16 | drivers touch no kernel file | CI: `tools/check-examples.sh` | DR-I1 over every commit of the pull request |
| DR-T17 | a fourth driver | boot: `edu-test`; its commit | `KB3 edu: ok:`; its commit touches only `kernel/drivers/`, the boot description and `Cargo.lock` |

Coverage on the host (`cargo llvm-cov`, lines): `moruna-mmio` 100%, `moruna-pci` 100%, `moruna-virtio` 99.1% (transport) and 100% (queue), `moruna-driver-table` 98.8%, `moruna-image` 98.1%; the kernel additions and the drivers are covered by the boot tests above.

## l. Implementation notes for the agent

- A protocol whose client needs a request named `connect` cannot have one: `protocol!` generates `Client::connect`.
- QEMU's `file` chardev takes `input-path`, which is how `console1` gets a line to read without a terminal.
- QEMU is built with `--enable-vhost-kernel`, without which `vhost-vsock-pci` does not exist; the runner needs the `vhost_vsock` module and a writable `/dev/vhost-vsock`.
- The disk image is written afresh before each of the three boots, since each boot rewrites it.

## m. Open items

1. **Vsock in CI.** The data path runs only on a runner with `/dev/vhost-vsock`. Without it QEMU cannot create the device at all (`vhost-vsock-pci` opens `/dev/vhost-vsock` when it is realised), so not even negotiation and queue setup can run; the boot then records why and requires the test's no-device line. The CI result on GitHub's `ubuntu-24.04` runner is recorded on the board.
2. **Bus numbers.** Buses are numbered from the ECAM window's first bus, which is bus 0 on q35 and every machine Moruna targets; on a segment whose MCFG starts at another bus the names and requester ids would be off by that bus. `resource_info` could carry the first bus (KE1).
3. **Per-device IOMMU domains.** OSTD 0.19 has one IOMMU domain for every device, so with an IOMMU a driver's device reaches every pinned page, not only its own driver's. KE1 needs a domain per DMA resource.
4. **MSI destination.** Every MSI goes to local APIC 0. Spreading a device's interrupts over CPUs needs the CPU's APIC id from OSTD, which it does not export.
5. **A partial overlap.** A claim that partly overlaps a range acquired before is refused even when nothing live holds it (f.3); it cannot be acquired from OSTD and is not inside what the kernel has.
6. **The debug console.** The console driver serves every process started after the device manager; the supervisor, the KB1 and KB2 tests and the drivers themselves still print through the kernel's debug console, which stays for early boot and for whatever must print when no driver can.

## n. Traceability

| requirement (BOARD EK KB3 brief) | here |
|---|---|
| PCI configuration access: the ECAM window from MCFG as a device memory resource, narrowed per function | d.2, f.3, f.6, f.12; `root.ecam`, `resource_info`; DR-T7, DR-T14 |
| MSI-X: a call that allocates a vector and returns the message, as an interrupt handle | d.1, e.2, f.2; `interrupt_create_msi`; DR-T7 |
| DMA: pin a DMA memory object and return its pages' addresses, only with a DMA right; the IOMMU; the trust statement | d.1, f.4; `memory_pin`, `dma_release`; DR-T7, DR-T14 |
| a virtio-pci library, `no_std`, safe; what it was built on | d.8, f.5 to f.7; decision 6 |
| console, block and vsock drivers, each a server with a protocol crate and a conformance suite | d.3 to d.5, f.9 to f.11; DR-T9 to DR-T13 |
| data moves by memory object | DR-I9, f.9 |
| the supervisor's enumeration, its table, only a device's own resources, a directory | e.3, f.12, d.6, d.7; DR-T15 |
| tests: block, console, vsock, isolation, a crashed driver, extensibility | k |

## o. Deferred

The image filesystem over the block protocol (KC2) needs: the block protocol as it is (d.3); a way to share one pinned buffer across requests without a pin each time (a long-lived pin a client registers, for the page cache); the layer disks QEMU attaches as more virtio-blk devices, which the table already serves as `block1`, `block2`; and read-only devices (`info().read_only`). NVMe and the cloud network card (KE1) need: the same driver shape over `moruna-pci` without virtio (NVMe's admin and I/O queues are rings like a virtqueue's, in pinned memory); several MSI-X vectors a device, one per queue, which the MSI resource already allows (raise `MSI_PER_FUNCTION`); per-device IOMMU domains (m.3); the first bus of a segment (m.2); and, for the network card, the network server (KE2) as its client.
