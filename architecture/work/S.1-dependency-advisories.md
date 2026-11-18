# S.1: known vulnerabilities in our dependencies

- **Design:** the OS design's security priority, and the Talos-like posture: a data OS with no
  shell, reachable only through its API on 443. **Rules:** `architecture/work/_rules.md`.
- **Found by:** item 4.6's `cargo deny check advisories` (2026-11-18). The fast pre-commit hook
  checks bans, licenses and sources but not advisories, so this went unseen.

## What `cargo deny check advisories` reports (2026-11-18)

**Both workspaces** (`Cargo.lock` and `kernel/Cargo.lock`) have `rustls` 0.23.45. The node uses it
for `api` on port 443 and for the network server's TLS client to object stores. Upgrading to
0.23.46 or later fixes four vulnerabilities, and `rustls-webpki` 0.103.16 or later fixes a fifth:

| ID | What it fixes |
|---|---|
| RUSTSEC-2026-0346 | `Debug` of client session values exposes TLS secrets |
| RUSTSEC-2026-0347 | Buffered data sent to the outer server after ECH rejection |
| RUSTSEC-2026-0348 | Brotli certificate decompression can allocate up to 1 GiB before authentication |
| RUSTSEC-2026-0349 | An unauthenticated ServerHello can downgrade later connections to classical key exchange |
| RUSTSEC-2026-0382 | Certificate verification may accept distrusted intermediates (`rustls-webpki`) |

**The root workspace also has:**
- `tokio`: RUSTSEC-2026-0386, unsound `AsyncFd::new`. Fixed in 1.51.6+ or 1.53.3+.
- `wasmtime`: RUSTSEC-2026-0315, 0316, 0325, 0326, 0327 and 0353–0358 (fuel accounting, GC heap
  corruption, a stack buffer overflow, host panics). Fixed in 48.0.6+ or 49.0.3+.
- **Unmaintained, with no safe upgrade:** `chrono`, `chrono-tz` and `humantime` (root), and
  `paste` (kernel).

## Do

1. **Upgrade `rustls` and `rustls-webpki` in both workspaces** to the fixed versions. Check
   `kernel/vendor/` for a vendored copy, and update it the vendored way (provenance and NOTICE) if
   one exists.
2. **Upgrade `tokio` and `wasmtime` in the root** to fixed versions on their current major lines.
   If a fix needs a new major version of `wasmtime`, adapt `crates/moruna-wasm` and say what
   changed.
3. **For each unmaintained crate,** say what uses it and propose a replacement or an ignore. An
   ignore gets a written reason in `deny.toml`. Do not ignore a vulnerability.
4. **Add `cargo deny check advisories`** to the quality gate where CI runs it (it needs the
   advisory database), so it cannot go unseen again. Say whether the fast hook can run it
   offline.
5. **Run** both workspaces' tests, the TLS tests (`kernel/net/stack/tests/tls.rs`, the api server's)
   and clippy. One booted check that the node serves HTTPS and reaches Blob over TLS waits for the
   PM's next image.

## Report

The versions before and after, each advisory's state, the unmaintained crates' proposals, the
gate change, the tests and the commits.
