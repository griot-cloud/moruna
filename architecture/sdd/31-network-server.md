# Moruna SDD 31: The network server

**Document type:** software design document, component 31
**Status:** DRAFT, 2026-10-30. Implementation and proof are tracked separately below.
**Parent:** `BOARD.md` EK, KE2; SDD 20 (process boundary), SDD 23 (service SDK), SDD 29 (card protocol).
**Component:** `kernel/net/{protocol,stack,server}`. The server is a user process, not kernel code.
**Consumes:** `moruna.driver.net`, the device directory, smoltcp 0.12.0, rustls 0.23.45 and ring 0.17.14.

## a. Responsibility

The server owns IP, ARP/NDP, TCP, UDP, ICMP echo, DHCPv4, DNS and outbound client TLS. Drivers own device DMA and frames. The microkernel owns channels, memory, scheduling and approved entropy, and understands none of these protocols. Adding an interface does not add kernel networking.

Inbound TLS termination is not implemented by this component yet. A TCP listener remains an unencrypted byte socket. This is an explicit remaining scope item, not an implied capability of `start_tls`.

## b. Protocol and compatibility

`netstack-protocol` defines `moruna.net` 3.0. Majors 1 and 2 are refused by the SDK handshake; there is no compatibility body decoder or copied TCP fallback. Ordinal 5 is `send_from(socket, source: Memory, offset: u64, length: u32) -> u32`; ordinal 6 remains `receive_into(socket, destination: Memory, offset: u64, max: u32) -> u32`. Both are bounded caller-owned loans (at most 1 MiB per request), retained through original completion or cancellation. UDP's separately bounded datagram control bodies remain distinct; they are not a TCP payload fallback.

| Ordinal | Contract |
| --- | --- |
| 1–4, 7–15 | Existing info/open/listen/accept, shutdown/close, UDP, resolution, ping, grant and status encodings |
| 5 | Caller-owned Memory TCP TX, partial positive count under backpressure |
| 6 | Caller-owned Memory TCP RX, zero only at authenticated EOF for TLS |
| 16 | Verified outbound `start_tls(socket, server_name)` |
| 17 | One immutable `bind_account(account)` before any network operation |
| 18 | `install_server_identity(control, control_bytes, key, key_bytes)` |
| 19 | `start_tls_server(socket, identity)` on a pristine accepted socket |
| 20 | `release_server_identity(identity)`, retiring associated sockets |
| 21 | `tls_alpn(socket)` after successful handshake |

The public client constructor takes the actual caller Account, performs the major-3 handshake and account binding within the supplied deadline, and retains that Account for subsequent loans. The server checks immutable charged Account identity before mapping any stream or credential loan, using ABI 2.5 `Account::matches`; same names, ancestry and service-pool accounts confer no substitution authority. Socket numbers and installed identity numbers belong only to their connection. Disconnect retires sockets, credential contexts and outstanding loans. Error numbers remain unchanged, including `TlsUnavailable` (19) and `TlsFailed` (20).

A boot-granted listener has one immutable policy: rights, local/remote address and port ranges, name patterns and a socket count. `grant` can only narrow that policy. Resolution by an authorized name follows the existing name-rule semantics; a resolved address does not widen an unrelated address grant. The original socket remains charged to its connection throughout an upgrade; TLS does not create an uncharged socket or a new connection authority.

## c. TLS placement

TLS executes in the network server process, against the TCP socket already owned by the caller's connection. It does not execute in the kernel or require an application to link a TLS library. A client library would simplify the server but move trust material and buffering into every task, contrary to KE2's server-owned boundary. A separate privileged TLS process would need an additional delegated-socket protocol; it is not introduced here.

Only a pristine established outbound TCP socket can be upgraded: no previous plaintext send/receive, no pending byte operation, and no queued received/transmitted bytes. A hostname must be a valid DNS name authorized by the original policy (a matching connect-name rule, or remote endpoint permission plus name-resolution permission). The hostname is also verified against the peer's certificate by rustls. A second upgrade is refused.

Once an upgrade is accepted, the socket retains a TLS marker even after errors. Send/receive route through it forever; an error aborts the TCP connection rather than restoring a plaintext path. A clean EOF requires TLS `close_notify`; bare TCP EOF is a TLS failure. Shutdown flushes accepted application bytes, sends `close_notify`, then closes TCP. `close` may abort an unfinished TLS exchange as it retires the socket.

TLS 1.3 is the only enabled version. The standard rustls verifier checks chain signatures, trust anchors, hostname and certificate validity. There is no custom verifier, permissive root, or fallback from TLS failure to an unencrypted exchange. Session resumption and 0-RTT are not relied on for correctness.

## d. Trust and time injection

The boot may give the server a read-only `tls-roots` memory capability. Its representation is four little-endian bytes containing the following payload length, then the SDK Wire encoding of `Vec<Vec<u8>>`; every inner value is a DER CA certificate. Maximum payload: 1 MiB; maximum roots: 64. Trailing bytes inside the payload or malformed DER fail server startup. Roots are copied into an immutable rustls configuration before any client is served, and callers cannot replace them.

The same trusted boot configuration supplies `setting tls-unix-seconds N` and `setting tls-time-valid-ms N`. The UTC value is an externally established anchor at server initialization, not the monotonic kernel counter interpreted as Unix time. Its provenance is the boot supervisor's responsibility: deployment must obtain and authenticate current time before supplying it. There is no production RTC, unauthenticated NTP, fixed epoch or guessed timestamp added here. If that responsibility cannot be fulfilled, TLS is unavailable.

The time provider adds monotonic elapsed seconds to the trusted anchor. The validity window is positive and at most one day; after expiry or a monotonic rollback it returns no time. A new TLS handshake then fails closed. An expired anchor does not silently renew itself. Existing authenticated records do not re-verify certificate validity on each receive; connections still obey TCP and protocol timeouts. Updating time or roots requires a fresh server process, with fresh trusted boot inputs.

Either both the roots capability and a nonzero anchor are supplied, or neither. A partial configuration fails startup. Without either, raw networking still works and `start_tls` explicitly refuses `TlsUnavailable`.

## e. Crypto and target support

ring 0.17.14's cached primary source `src/rand.rs` explicitly implements `SystemRandom` for `target_os = "none"` when `less-safe-getrandom-custom-or-rdrand` is enabled, forwarding to getrandom. getrandom 0.2.17 provides `register_custom_getrandom!` for unsupported targets. The sole registered implementation calls `moruna_svc::random`, which calls the approved ABI `random_read`; failure is returned to ring. No deterministic generator, raw local RDRAND implementation or secondary entropy path is used for TLS. smoltcp's separately seeded pseudo-random sequence is not TLS entropy.

The rustls `ClientConfig::builder_with_details` no-std API accepts explicit crypto/time providers. The unbuffered API requires the caller to transmit encoded records before acknowledging `TransmitTlsData`, and discard exactly the bytes reported by each state. The implementation follows that contract and bounds each turn's work.

Primary references: [ring target randomness](https://docs.rs/ring/0.17.14/ring/), [rustls unbuffered API](https://docs.rs/rustls/0.23.45/rustls/unbuffered/), and the corresponding cached crate sources. These establish API support, not successful compilation or execution on Moruna. The ring C/assembly cross-build for `x86_64-unknown-none` remains a required proof. Upstream [ring issue 1832](https://github.com/briansmith/ring/issues/1832) documents an important limitation: its build script does not select ELF assembly for an unknown OS, while x86_64 code assumes those routines exist. The reviewed build-only patch is `kernel/net/ring-none.patch`; exact registry archive/build-script/patch hashes are in `kernel/net/ring-provenance.json`. It selects ELF assembly only for x86_64 `none`, and uses the upstream freestanding-header path with clang and no red zone or stack protector. It changes no crypto source. The actual native-target userland check subsequently failed in ring C compilation through host `/usr/include/assert.h` → `features-time64.h` → missing `bits/wordsize.h`. This is the recorded reason to enable the freestanding adaptation, not a successful link claim. Exact upstream ring 0.17.14 is now vendored at `kernel/vendor/ring-0.17.14`; only `build.rs` differs among all 391 upstream files. `ring-upstream-files.sha256` and `ring-provenance.json` bind the archive, every original file and the patched build script. The kernel workspace excludes that path as a member and selects it through its sole `[patch.crates-io]`, outside `net/*`; the root std workspace and the proof registry vendor are not mutated. Actual baremetal compile/link and boot remain required. If the toolchain cannot build it, report the concrete error before choosing another provider; never weaken verification to obtain a binary.

## f. Stack and dispatcher

Each configured card is a `moruna.driver.net` connection discovered through the device directory. The server asks for address, MTU and link state, refuses unknown offloads, subscribes to frames and maintains one outstanding receive and link watch. Card frames and sends have bounded queues. The loopback interface stays in the process. Its frames are looped through the same smoltcp machinery, not answered by a fake socket implementation.

The dispatcher waits on client channels, card channels and a timer at the next stack deadline. Each turn polls ingress, protocol requests, egress and completion replies. Deferred operations preserve backpressure. TLS uses the same TCP buffers and wakeups, adds a handshake deadline and schedules another turn if its bounded state-machine work is not finished.

DHCPv4 uses smoltcp's lease events to replace IPv4 address, default route and DHCP DNS servers; loss removes those values. Explicit DNS servers precede DHCP-provided servers. Each interface's DNS socket receives only servers reachable from that interface. Resolution starts A and AAAA queries, cancels both on failure/disconnect, and is bounded by a configured deadline. Two resolver slots are the minimum useful capacity because each name consumes two queries.

## i. Configuration and resource bounds

Configuration starts `moruna-netstack 1`; existing `setting`, `interface`, `dns` and `grant` lines remain unchanged. Unknown names, malformed values, duplicate interfaces/grants and invalid policies fail with the line that supplied them.

TLS additions:

| Setting | Default | Bound |
|---|---:|---:|
| `tls-sockets-max` | 16 | 1..1024, also charged to original socket grant |
| `tls-buffer-bytes` | 65536 | 16640..1048576 per socket buffer and cumulative incoming handshake |
| `tls-handshake-timeout-ms` | 30000 | 1..3600000 |
| `tls-unix-seconds` | absent/disabled | explicit 1..253402300799 |
| `tls-time-valid-ms` | 3600000 | 1..86400000 |
| `dns-queries` | 16 | 2..256 per interface |

A TLS application send accepts at most one record (16384 bytes), returning a partial count when needed. TLS processing yields after 64 states per poll. Input, output and decrypted application buffers are bounded; cumulative handshake input is bounded even when fragmented over many polls. Full buffers apply backpressure. The configured simultaneous TLS socket cap includes failed TLS states until the caller closes them. A failed handshake cannot continually allocate replacements on one socket.

Caller-owned TX/RX/credential loans require exact caller Account origin. Private smoltcp, rustls and provider allocations are charged to the real network service process Account, not silently attributed to the caller. Admission applies both the original finite per-connection policy socket count and aggregate `sockets-max` (default 1024, configured maximum 65536); exhaustion refuses `TooMany` before creating another socket. Both TLS directions share `tls-sockets-max`, including failed allocated TLS states until close. Credential installation shares a global eight-context bound. Each owned TLS state reserves three fixed `tls-buffer-bytes` staging buffers plus one 16 KiB application-send buffer; smoltcp's separately bounded TCP rings and opaque provider state are additional service-account allocations. These limits do not promise an exact provider allocation size or replace the real service Account ceiling. No fictitious caller heap ticket or second accounting authority is introduced.

The per-process kernel ceiling is KB5's responsibility. These protocol/settings bounds are not a substitute for KB5, and this branch does not invent an independent kernel allocator or bypass a process grant.

## k. Checks and proof status

Host checks cover the existing wire/policy/config/single-server/pair-server suites, lossy-link TCP, DHCP lease installation and removal, DNS failure/slot accounting, TLS CA/name/time failures, partial records, abrupt EOF, handshake bounds, post-plaintext upgrade refusal and persistence of the failure marker. The protocol suite must account for every major-3 dispatch row, preserve unchanged ordinal encodings, and refuse old majors.

A deterministic QEMU user-network proof must use a separate virtio NIC from KE1's two socket-backend NICs, a configured DHCP lease and local controlled DNS/TCP/TLS peers. It must not depend on public DNS, the internet or a cloud certificate. Test CA/key/time fixtures belong only to the proof; they are never production trust defaults. Host listeners bind loopback, and the proof records which forwards and addresses were used.

Required commands, once the shared execution slot is granted: bounded host tests of `netstack-protocol` and `moruna-netstack`, kernel-target cross-check/build of `netstack` with the repository's linker flags, separate userland clippy, normal pre-commit hook, then QEMU TCG boots on 1, 2 and 4 CPUs with serial expectation checks. Exit status and exact expected lines determine a pass. No boot or TLS handshake pass is claimed until those commands actually execute.

## m. Remaining work

Cross-build and execute all checks above; record genuine failures and fix them. Complete the user-network proof fixture and TLS conformance rows. Inbound server TLS is now source implementation pending compilation, interoperability execution and native boot proof; it is not accepted merely from source review. KE3 owns production time/root provisioning and cloud NIC hardware evidence. The independent component-30 engine/builder document numbering collision discovered during handover is left for authorized integration, not renumbered by this branch.


## Caller-owned ingress (major 2)

The caller retains its original charged Memory and transfers a duplicate carrying
READ|WRITE|MAP|TRANSFER. READ is required because current moruna-sys mapping always
adds map::READ, even for a requested writable mapping. The stack validates and maps
only pages covering the bounded offset/range before consuming any bytes; overflow,
missing rights, wrong object, out-of-range mapping and max beyond RECEIVE_MAX are
refused. A deferred receive owns that loan until completion, close, disconnect or
cancel; it does not allocate a second destination or retain a lifetime-free raw
mapping. Completion is a u32 count and zero retains authenticated EOF semantics.
Plain TCP copies from smoltcp's bounded receive ring directly into the mapped caller
destination. TLS uses rustls' existing bounded server-charged plaintext staging;
this is not a zero-copy or DMA-to-application claim and never a bulk wire reply.

The SDK host Memory model tests shared backing, subset rights, transferable handles,
page bounds and map/write permissions. It does not prove kernel account charging or
DMA. Native boots must prove those with the actual KB5 memory/account authority.
Tests retain local byte-collection adapters only to compare expected fixtures;
production receive requests have one Memory wire contract and no Vec reply path.


## k. Accepted-socket TLS and secret ownership (major 3)

The shared rustls unbuffered engine handles outbound client and accepted-socket server TLS 1.3. Server identity installation requires the connection's LISTEN authority and two charged Memory loans: the sole SDK `ServerIdentitySpec` control codec contains a DER chain, ALPN list and finite lifetime, while a separate bounded PKCS8 loan contains the private key. At most eight contexts belong to a connection. Chains contain 1–8 certificates and at most 48 KiB; keys are at most 16 KiB; ALPN contains at most 16 identifiers of 1–255 bytes. Lifetimes are positive and at most 24 hours, measured against real monotonic time, bounded additionally by the trusted boot clock. Expiry or explicit release closes every associated socket and drops the context. There is no credential readback, private-key RPC or Debug representation.

A fresh accepted socket may already hold ClientHello bytes; this is not prior application plaintext. Any previous application TX/RX, pending application operation or queued server plaintext refuses the upgrade. Once an upgrade attempt enters TLS mode, failure cannot restore plaintext. A certificate/key mismatch refuses installation. Server configuration uses genuine ring signing and rustls TLS 1.3, disables tickets, resumption storage and early data, and advertises only the explicitly supplied ALPN. One identity serves a socket; SNI multi-identity selection and client-certificate authentication are not implemented. TLS does not establish application peer authorization.

Owned plaintext/ciphertext staging allocations are fixed-capacity, initialized through their full reported capacity, never grown, and wiped on consumption, truncation and drop. PKCS8 staging is zeroizing; shared caller Memory is not misrepresented as an exclusive Rust slice. Provider-owned ring signing/AEAD allocations have opaque cleanup APIs: full provider-heap zeroization is explicitly unsupported, rather than asserted. Keys remain private to the isolated network process. This limitation does not alter the separate storage WorkspaceKey/AES schedule wipe contract.

Conformance source must cover every major-3 dispatch row, old-major refusal, missing/repeated/foreign Account binding, bad loan rights/ranges without consuming bytes, genuine verified client/server authenticated bytes and EOF, fragmented handshake, ALPN, wrong key/certificate, plaintext-upgrade refusal, failed-handshake no-downgrade, identity release/expiry and all configured handshake/socket limits. Host fixtures model Memory identity; they do not prove native DMA, kernel charging or secret lifetime. All newly added major-3 tests remain unexecuted until the integration owner runs the pinned compiler and runtime gates.
