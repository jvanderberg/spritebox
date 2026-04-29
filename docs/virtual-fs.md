# Virtual Filesystem Sharing

Design plan for sharing local host directories with a remote sprite over the
constrained-bandwidth WebSocket transport.

## Background

Yolobox shared host directories with its local krunkit VM via virtiofs. That
mechanism requires shared host/guest memory and is unavailable when the VM is a
remote Firecracker microVM running on Fly.io. spritebox needs a different
approach.

## Goals

- Make a designated host directory appear inside the sprite at a stable mount
  point (e.g. `/workspace/shared`).
- Reads and writes work through normal POSIX syscalls — tools like editors,
  `grep`, `cargo`, etc. just work.
- Bidirectional: the sprite can read and write; the host can read and write;
  changes propagate.
- Bandwidth-efficient: only transfer bytes that are actually accessed.
- No manual full-tree sync at startup; large repos with build artifacts must
  not eat the link.

## Non-Goals

- Concurrent multi-writer conflict resolution. Last-writer-wins is acceptable;
  the user is responsible for not editing the same file in two places at once.
- Block-level deduplication, snapshots, or versioning.
- Working offline. The mount is meaningful only while the sprite is running and
  reachable.
- A general-purpose distributed filesystem. This is a single-user dev tool.

## Approach

Implement a **lazy, on-demand virtual filesystem** on the sprite using FUSE.
Directory listings are populated from a manifest pushed by the host; file
contents are fetched on first access and cached locally. Writes are buffered
and pushed back to the host on `release`/`fsync`.

The host runs a long-lived daemon that serves protocol requests from the
sprite, watches the local directory for changes, and pushes manifest deltas to
the sprite when files change underneath it.

### Why FUSE (not eager bidirectional sync)

Eager sync (Mutagen-style) requires walking and transferring the full tree
upfront. For repos with `node_modules`, `target/`, etc., this is exactly the
bandwidth cost we want to avoid. Lazy FUSE pays only for what gets opened.

The tradeoff is per-file latency on cold reads. We mitigate with parallel
fetches, prefetch-on-readdir, and aggressive content caching.

### Why not Mutagen / Syncthing

Both are Go binaries. There is no production-grade Rust equivalent. We also
want tight integration with the existing spritebox WebSocket transport rather
than running a separate sync daemon with its own networking.

## Architecture

Hexagonal / ports-and-adapters layering. Each boundary is a Rust trait so
everything below the FUSE adapter can be unit-tested on macOS without a real
kernel mount.

```
┌─────────────────────────── sprite (Linux) ──────────────────────────┐
│  FUSE kernel                                                        │
│      │                                                              │
│  fuser::Filesystem adapter   (thin translation, no logic)           │
│      │                                                              │
│  RemoteFs trait              (open/read/write/readdir/stat/...)     │
│      │                                                              │
│  Remote logic                (manifest cache, content cache,        │
│                               write buffer, prefetch policy)        │
│      │                                                              │
│  CmdClient trait             (send Request -> Response)             │
└──────│──────────────────────────────────────────────────────────────┘
       │ Transport (trait)
       │   real:    exec WebSocket binary frames
       │   tests:   in-memory mpsc
┌──────│──────────────────────── host (any OS) ───────────────────────┐
│  CmdServer trait             (handle Request -> Response)           │
│      │                                                              │
│  Host logic                  (request dispatch, fs watcher,         │
│                               push notifier)                        │
│      │                                                              │
│  HostFs trait                (read_file/write_file/list_dir/watch)  │
│      │                                                              │
│  Real impl: tokio::fs + notify     /     in-memory tree for tests   │
└─────────────────────────────────────────────────────────────────────┘
```

### Workspace Layout

| Crate                       | Purpose                                                    | Targets       |
| --------------------------- | ---------------------------------------------------------- | ------------- |
| `spritebox-fs-protocol`     | `Request`/`Response` enums, attrs, paths. No deps.         | all platforms |
| `spritebox-fs-host`         | Host logic + `HostFs` trait + `tokio::fs` and mock impls   | all platforms |
| `spritebox-fs-remote`       | Remote logic + `RemoteFs` trait + caching                  | all platforms |
| `spritebox-fs-transport`    | `Transport` trait + WebSocket impl + in-memory impl        | all platforms |
| `spritebox-fs-fuse`         | `fuser::Filesystem` adapter over `RemoteFs`                | Linux only    |
| `spritebox-fs-tests`        | End-to-end harness without FUSE                            | all platforms |

The `spritebox-fs-fuse` crate is gated behind `#[cfg(target_os = "linux")]` so
the rest of the workspace builds cleanly on macOS, where unit and integration
tests run.

### Trait Sketch

```rust
// spritebox-fs-protocol
pub enum Request {
    Lookup    { parent: u64, name: String },
    GetAttr   { ino: u64 },
    ReadDir   { ino: u64, offset: u64 },
    Read      { ino: u64, offset: u64, size: u32 },
    Write     { ino: u64, offset: u64, data: Bytes },
    Create    { parent: u64, name: String, mode: u32 },
    Unlink    { parent: u64, name: String },
    Rename    { old_parent: u64, old_name: String,
                new_parent: u64, new_name: String },
    Truncate  { ino: u64, size: u64 },
    Fsync     { ino: u64 },
    // ...
}

pub enum Response {
    Attr     { ino: u64, attr: FileAttr },
    DirEntries { entries: Vec<DirEntry>, eof: bool },
    Bytes    { data: Bytes },
    Ok,
    Error    { errno: i32 },
}

pub enum Push {
    InvalidateAttr  { ino: u64 },
    InvalidateEntry { parent: u64, name: String },
    InvalidateData  { ino: u64, offset: u64, len: u64 },
}
```

The `RemoteFs` trait mirrors FUSE semantics rather than POSIX, so the FUSE
adapter stays a near-mechanical translation (~200 lines).

## Transport

### Channel: dedicated exec WebSocket

Use the existing Sprites `exec` API to spawn `spritebox-fsd --serve-stdio` and
treat the resulting WebSocket as a binary bidirectional pipe. This avoids:

- The OSC 9999 channel's text-only base64 tax (~33% overhead).
- Contention with the user's interactive console.
- The need for any server-side Sprites changes.

The OSC bridge stays in place for what it does well: small synchronous
user-driven actions (`sprite-open`, `paste-image`, etc.).

### Verification needed

The exec WebSocket protocol prefixes stdin frames with a `0x00` byte and uses
`0x04` for EOF (sprites_api.rs:444). We need to confirm raw binary payloads
round-trip cleanly through stdout, or escape inband control bytes. This is the
single transport-layer risk; a quick test seals it.

### Framing

Length-prefixed: `[u32 length][u8 kind][payload]`. The protocol layer is
`bincode`- or `postcard`-encoded for compactness; not human-readable on the
wire, but the in-memory transport impl makes debugging trivial.

### Multiplexing

Single channel, request/response with monotonic request IDs and out-of-order
responses. Pushes from host to sprite (manifest deltas) ride the same channel
as `Push` frames distinguishable by kind byte.

## Caching

### Metadata cache (sprite)

- Populated lazily as the kernel issues `lookup` / `readdir`.
- Invalidated by `Push::InvalidateEntry` from the host when its watcher fires.
- Negative cache for `ENOENT` with short TTL (~1s) to absorb burst lookups
  from build tools.

### Content cache (sprite)

- Disk-backed, content-addressed by `(ino, hash)`.
- LRU eviction with a configurable cap (default ~1 GiB).
- Cache entry invalidated on `Push::InvalidateData` or when host-reported
  `mtime` changes between manifest snapshots.

### Coherence model

Eventual consistency. The host watcher pushes invalidations promptly, but
there is a window where the sprite serves stale data after a host-side edit.
Acceptable for the dev-loop use case; the contract is "don't edit the same
file in two places at once."

## Writes

Three semantics worth considering:

- **Write-through**: every `write` blocks on the round trip. Simple, slow.
- **Write-back-on-release**: buffer in the cache, flush on `release` (file
  close). Standard sshfs default.
- **Async write-back**: dirty queue, fire-and-forget with periodic flush.
  Fastest, but "where is my data really?" risk.

**Default: write-back-on-release**, with `fsync` forcing immediate flush. This
matches user expectations for editor save semantics — the flush happens when
they close/save, and `fsync` (which editors use after a write) gives a
synchronous barrier.

Renames are sent atomically as a single `Rename` request so the host can
preserve atomicity end-to-end (important for editor "write tmpfile then rename
over original" patterns used by vim, VSCode, etc.).

## Provisioning

On sprite launch, spritebox already installs bridge scripts via
`install_bridge_scripts` (app.rs:1178). We extend that with:

1. `chmod 666 /dev/fuse` (the device exists in Sprites' Firecracker kernel but
   ships with restrictive perms — verified on `pico-gamer-main`, kernel
   `6.12.47-fly`).
2. Install the `spritebox-fsd` binary at `/usr/local/bin/spritebox-fsd`. We
   cross-compile from the host (`cross` targeting
   `x86_64-unknown-linux-gnu`) and ship via `write_file`. Eventually this
   should be baked into the sprite image.
3. (Optional) systemd unit or launch-on-first-mount; for v1 the host triggers
   it explicitly via `exec`.

Sprites kernel features confirmed in place: `/dev/fuse` char device 10:229,
`fuse` and `fuseblk` registered in `/proc/filesystems`, `cap_sys_admin` in the
ambient set.

## CLI Surface

```bash
# Mount a host dir into the active sprite session
spritebox --name foo --share ~/code/myproj:/workspace/shared

# Manage shares on a running sprite
spritebox share add    --name foo ~/code/myproj /workspace/shared
spritebox share list   --name foo
spritebox share remove --name foo /workspace/shared
```

By default a share is bidirectional and read-write. `--readonly` flips it to
host-as-source-of-truth (writes from the sprite return EROFS).

## Testing Strategy

The hexagonal design's main payoff is that nearly every interesting failure
mode can be reproduced in `cargo test` on macOS, with no FUSE and no real
sprite. To realize that payoff, the simulation harness needs an explicit
**control surface** for fault injection, and the test plan needs to enumerate
**scenarios crossed with failure modes** rather than gesture at categories.

### Contracts the tests must prove

State these explicitly so tests have something concrete to assert against:

- **Byte integrity**: bytes returned from any read equal the bytes the host
  has at that path/offset/length, modulo eventual-consistency windows.
  Verified with SHA-256, not just `assert_eq!`.
- **Hash-on-receive**: every cached chunk is validated against an expected
  hash before being served. Corruption in transit must surface as `EIO`,
  never as silently bad bytes.
- **fsync barrier**: `fsync` is synchronous and durable. After `fsync`
  returns, killing the daemon and restarting against the same backing store
  must show the data on the host.
- **Release durability**: data written to a file handle must be on the host
  by the time `release` (close) returns success. If transport drops mid-flush
  before durability is achieved, `release` must return `EIO` rather than
  succeeding silently.
- **Atomic rename**: `rename(tmp, real)` is observably atomic on the host —
  no observer ever sees `real` empty or partial.
- **Cache transparency**: byte-content returned for any read is independent
  of cache state, eviction history, or chunking.
- **Eventual consistency window**: after host quiesces and the `Push` queue
  drains, sprite-side `getattr`/`readdir` matches host ground truth exactly.
- **Idempotent invalidation**: applying the same `Push` N times equals
  applying it once.
- **Path scoping**: no request — adversarial or otherwise — escapes the
  configured share root.

### Simulation harness control surface

The in-memory `Transport` and `HostFs` impls are not enough on their own. The
`spritebox-fs-tests` harness needs:

- **Injectable latency** per direction: `transport.set_latency(client→host, 50ms)`.
- **Bandwidth caps**: `transport.set_bandwidth(1 MiB/s)`.
- **Lossy mode**: drop N% of frames, drop a specific frame by predicate, drop
  the next K frames.
- **Disconnect injection**: `disconnect_after_bytes(N)`,
  `disconnect_before_response(request_id)`, `disconnect_now()`.
- **Reorder buffer**: hold a response until a later request's response has
  been delivered, to exercise out-of-order arrival in the demuxer.
- **Frame corruption**: flip a byte at a chosen offset of a chosen frame.
- **Deterministic clock**: every TTL, debounce, and retry consults a `Clock`
  trait. Tests use `tokio::time::pause` plus a manually-advanced fake clock.
- **Bifurcated views**: two `RemoteFs` instances against the same `HostFs`,
  to simulate sleep/wake re-mount and concurrent sprite-side handles.
- **Daemon restart**: tear down the host side and reinstantiate against the
  same on-disk backing; in-flight requests must complete with success or
  `EIO`, never silent loss.
- **Cache-dir restart**: tear down the sprite side and recreate it pointing
  at the same content-cache directory; assert no corruption, dirty data
  either flushed or surfaced.
- **Chaos mode**: deterministic random combination of the above driven by a
  proptest seed.

Without these knobs the in-memory transport tests only happy paths and the
design's claimed advantage (testability) is mostly forfeited. **These belong
in Phase 1**, before any real-world workload runs over the FS.

### Property tests (`proptest`)

The in-memory `HostFs` is a perfect oracle: identical operations on a real
`tempfile`-backed FS and the simulated stack must produce identical observable
state.

- **FS equivalence**: random sequence of
  `(open, write, read, seek, truncate, rename, unlink, fsync, close)` on a
  small path namespace; after every step assert byte-content equivalence and
  stat equivalence (size, mtime ordering, link count) between real and
  simulated.
- **Cache transparency**: under a chaos-driven transport (random drops,
  latency, evictions), every read returns content bit-identical to the host's
  ground truth.
- **Manifest delta convergence**: arbitrary host-side mutation sequence,
  drain Push queue, sprite snapshot equals host snapshot.
- **Idempotent invalidation**: applying any `Push` set twice equals applying
  it once.
- **Framing fuzz**: arbitrary byte input fed to the transport reader must
  not panic, must not OOM, must surface a clean error or be ignored. Run as
  a `cargo fuzz` target as well as a `proptest`.
- **Wire format round-trip**: `bincode`/`postcard` encode→decode of any
  `Request`/`Response`/`Push` round-trips identically.

### Concrete scenarios

The test suite must exercise at minimum the following scenarios. All are
runnable on macOS via the in-memory harness unless flagged.

| #  | Scenario                                          | Class            |
|----|---------------------------------------------------|------------------|
| 1  | Editor atomic save (write tmp → fsync → rename)   | correctness      |
| 2  | Disconnect mid-flush during `release`             | failure          |
| 3  | Host watcher race vs. in-flight sprite write      | concurrency      |
| 4  | mtime round-trip (write→read on both sides)       | correctness      |
| 5  | readdir paging with concurrent host mutation      | concurrency      |
| 6  | Cache eviction during outstanding read            | concurrency      |
| 7  | FS-equivalence proptest (real FS oracle)          | property         |
| 8  | Cache-transparency proptest under chaos transport | property         |
| 9  | Daemon restart mid-flush                          | failure          |
| 10 | Negative cache TTL expiry                         | correctness      |
| 11 | Path-scoping adversarial (`..`, NUL, oversize)    | security         |
| 12 | Frame fuzzer / malformed wire input               | fuzz             |
| 13 | Transit-byte corruption → must surface `EIO`      | integrity        |
| 14 | Open-then-unlink-on-host (POSIX semantics)        | correctness      |
| 15 | fsync barrier under crash + restart               | durability       |
| 16 | Concurrent multi-handle reads vs. writes          | concurrency      |
| 17 | Watcher coalescing under burst (50k file create)  | scalability      |
| 18 | uid/gid mapping host↔sprite (define & assert)     | correctness      |
| 19 | ENOSPC on flush propagates to caller              | failure          |
| 20 | FUSE adapter contract (replay golden trace)       | adapter (Linux)  |

Pseudo-code sketches for a few of the load-bearing ones:

```rust
// 1. Editor atomic save — at no intermediate step does host_fs see "/proj/file" empty
let f = remote.create("/proj/file.tmp")?;
remote.write(f, 0, b"hello")?;
remote.fsync(f)?;
remote.rename("/proj/file.tmp", "/proj/file")?;
remote.release(f)?;
assert_eq!(host_fs.read("/proj/file")?, b"hello");
assert!(!host_fs.exists("/proj/file.tmp"));
```

```rust
// 2. Disconnect mid-flush — release returns Err(EIO) OR the data is fully durable;
// never silent partial loss
let f = remote.create("/big")?;
remote.write(f, 0, &vec![0xAB; 10_000_000])?;
transport.disconnect_after_bytes(2_000_000);
match remote.release(f) {
    Err(EIO) => {}
    Ok(()) => assert_eq!(host_fs.read("/big")?.len(), 10_000_000),
    other => panic!("unexpected: {other:?}"),
}
```

```rust
// 7. FS equivalence — the killer property test
proptest!(|(ops in fs_op_seq(50))| {
    let real = TempLocalFs::new();
    let sim  = SimulatedRemoteFs::new();
    for op in &ops { real.apply(op); sim.apply(op); }
    prop_assert_eq!(real.snapshot(), sim.snapshot());
});
```

```rust
// 13. Hash validation — corruption must surface, not silently propagate
transport.corrupt_byte(request_id = 1, offset = 42);
let r = remote.read("/x", 0, 4096);
assert!(matches!(r, Err(EIO)));
```

```rust
// 20. FUSE adapter contract — Linux-only, but the trace is portable
let golden = include_bytes!("traces/fuser_callback_trace.bin");
let recorder = RecordingRemoteFs::new();
fuser_replay(golden, &recorder);
assert_eq!(recorder.calls(), expected_remotefs_calls);
```

The full list of 20+ scenario sketches is the working backlog for the
`spritebox-fs-tests` crate.

### FUSE adapter equivalence

The plan's "thin translation, ~200 lines" claim is only meaningful if proven.
Tests that run *below* the adapter cannot demonstrate the adapter calls the
methods FUSE actually invokes.

Approach: a **golden FUSE trace**. On a real Linux mount, run a recorded
workload (`tar -xf`, `git clone`, `cargo build`, `vim` save, `rm -rf`) with
the FUSE adapter wrapping a `RecordingRemoteFs` that logs every method call
with arguments. Serialize the recording as `traces/fuser_callback_trace.bin`.
A `cfg(target_os = "linux")` test replays those callbacks against the
adapter and asserts the resulting `RemoteFs` call sequence matches a stored
expected sequence.

This catches drift in either direction: kernel behavior changes (new FUSE
opcode, changed flags) or `RemoteFs` semantic regression. Update the golden
when behavior intentionally changes.

### End-to-end against a real sprite

A `cargo xtask fs-smoke` task launches a sprite, mounts a temp dir,
performs a representative workload (the same one used to record the FUSE
golden trace), and verifies content/stat equivalence with the host. Manual
or pre-release; not a substitute for the simulation harness.

## Instrumentation and Diagnostics

The simulation harness catches what we can predict. The instrumentation layer
catches what we can't — production bugs that only surface when a real kernel,
real FUSE, real WebSocket, and real workload meet for the first time. Build
this in from Phase 1 so the first time something behaves weirdly on a sprite
we have data, not guesses.

### Structured tracing

Every FUSE callback and every internal layer transition emits a
`tracing::span!` with structured fields. Spans nest naturally: a `lookup`
span contains the `CmdClient` request span, which contains the `Transport`
frame send/recv spans.

Every span carries:

- `op` — the FUSE opcode or layer operation name
- `req_id` — protocol request ID, for cross-layer correlation
- `path` or `ino` — what's being touched
- `bytes_in` / `bytes_out` — payload sizes
- `cache` — `hit` / `miss` / `evict` / `n/a`
- `errno` — on error completion
- `elapsed_ms` — span duration

Default level is `info` for span open/close, `debug` for per-frame detail.
Filter via the standard `RUST_LOG` / `SPRITEBOX_FS_TRACE` env vars
(`SPRITEBOX_FS_TRACE=lookup,read,write` to enable specific opcodes only).

### Trace recording mode

When `SPRITEBOX_FS_RECORD=<path>` is set, the daemon records every FUSE
callback (opcode + arguments + result + timing) to a portable binary file.
The same format is replayable against the in-memory harness — meaning a bug
seen on a sprite can be reduced to a deterministic `cargo test` repro by
copying the trace file off the sprite. This file is the same shape used by
the golden-trace test described above; production traces double as test
fixtures.

Recording overhead must be cheap enough to leave on by default in dev mode.
Target: <5% throughput cost at default verbosity, dropping events under
backpressure rather than blocking.

### Live diagnostic dump

The daemon listens on a Unix socket (`/run/spritebox-fsd.sock` inside the
sprite) and on `SIGUSR1`. On either trigger it dumps:

- Cache state: entries, sizes, hit/miss counters, eviction count, current bytes.
- In-flight request table: request ID, opcode, path, age, transport state.
- Watcher state: subscribed paths, queued pushes, last delivery time, debounce backlog.
- Manifest cache: total inodes, last refresh, invalidation rate.
- Transport state: connection health, bytes in/out, last frame timestamp,
  reconnect attempts.
- Recent errors: ring buffer of the last 64 errors with timestamps and
  contexts.

A `spritebox fs diag --name <sprite>` host command pulls the same dump over
the OSC bridge, formats it, and writes a snapshot. Useful for "the mount
seems stuck" reports.

### Counters and metrics

Counters exposed via the diagnostic dump and (if running under a real
observability stack) Prometheus-style scrape on a local port:

- `fs_ops_total{op}` — calls per FUSE opcode.
- `fs_op_duration_seconds{op}` — histogram.
- `fs_cache_hit_ratio{kind}` — for metadata, content, negative.
- `fs_cache_bytes` / `fs_cache_evictions_total`.
- `fs_inflight_requests`.
- `fs_transport_bytes_total{direction}`.
- `fs_transport_reconnects_total`.
- `fs_push_queue_depth`.
- `fs_errors_total{kind}`.

### Differential mode (opt-in)

When `SPRITEBOX_FS_DIFFERENTIAL=1`, the daemon runs every FUSE-driven
operation against both the real `RemoteFs` and a shadow in-memory copy
populated from the same manifest. After each operation it asserts the two
agree on (a) the result returned to the kernel and (b) the cache/state
fingerprint. Divergence emits a `tracing::error!` with both views and (in
test mode) panics.

This is the closest thing to a continuous correctness check that runs in
production. Cost is ~2x memory and ~2x CPU on hot ops, so it's off by
default — but it catches drift between the simulated and real implementations
that golden traces would miss.

### FUSE-side instrumentation hooks

Inside the `fuser::Filesystem` adapter, every callback follows the same
pattern:

```rust
fn read(&mut self, req: &Request, ino: u64, fh: u64,
        offset: i64, size: u32, /* ... */ reply: ReplyData) {
    let span = tracing::info_span!("fuse.read",
        ino, offset, size, req_id = self.next_req_id()).entered();
    let t0 = Instant::now();
    match self.runtime.block_on(self.remote.read(ino, offset as u64, size)) {
        Ok(bytes) => {
            tracing::info!(elapsed_ms = t0.elapsed().as_millis() as u64,
                bytes = bytes.len(), "ok");
            reply.data(&bytes);
        }
        Err(errno) => {
            tracing::warn!(errno, elapsed_ms = t0.elapsed().as_millis() as u64, "err");
            reply.error(errno);
        }
    }
}
```

The pattern is uniform enough that it should be a macro
(`fuse_call!(self, read, ino, offset, size => self.remote.read(...))`) so
adding a new callback automatically gets tracing, recording, timing, and
errno mapping for free.

### What this buys us

When a real sprite shows a bug — wrong bytes, hung mount, mtime drift —
the recovery path is:

1. `spritebox fs diag --name <sprite>` to get state snapshot.
2. Check the recorded trace file for the operation that triggered it.
3. Replay the trace against the in-memory harness.
4. Reproduce, fix, add as a regression test.

Without this loop, debugging a remote FUSE mount over a flaky link is
guesswork. With it, the same bug surfaces twice and ships fixed.

## Risks and Open Questions

- **Exec WebSocket binary cleanliness**: the stdin framing uses `0x00` / `0x04`
  inband control bytes. If these aren't transparent in the stdout direction
  too, we need an escape layer or to use a different API path. **Action**:
  verify with a small round-trip test before committing to the design.
- **Cold-read latency on large operations**: `rg`, `cargo build`, IDE
  indexers will burst hundreds of file opens. We need parallel fetches and
  possibly speculative prefetch on `readdir`. Real-world testing required to
  set sensible defaults.
- **mtime fidelity**: `make` and similar care about mtime ordering. The
  protocol carries real mtimes from the host, but we need to ensure FUSE
  returns them faithfully and that writes preserve sensible mtimes back.
- **Symlinks, xattrs, special files**: each is its own small swamp. v1
  supports regular files and directories; symlinks come in v1.1; xattrs and
  special files are explicit non-goals unless something needs them.
- **`/dev/fuse` perms regression**: we depend on a `chmod 666` that could
  break if the Sprites image changes. Worth a `doctor` check that surfaces
  this clearly.
- **Daemon lifecycle**: who restarts `spritebox-fsd` if it crashes? For v1
  the host detects a closed transport and re-execs. systemd or a supervisor
  is overkill for the user-attached use case.
- **Sprite sleep / wake**: sprites auto-sleep after 30s idle. The mount
  should survive a sleep/wake cycle — verify the daemon and the mount don't
  leave the kernel confused after the sprite freezes.

## Phased Rollout

**Phase 1 — read-only, single share, full test harness**
- Protocol crate, host logic with `tokio::fs`, remote logic with caching, FUSE
  adapter.
- **Simulation harness control surface complete**: latency/bandwidth/loss/
  disconnect/reorder/corruption/clock injection. This is non-negotiable for
  Phase 1 — the rest of the project rides on it.
- **Instrumentation complete**: structured tracing, recording mode, live
  diagnostic dump, counters, FUSE-callback macro.
- All 20 baseline test scenarios green; FS-equivalence and
  cache-transparency proptests passing under chaos.
- `spritebox --share <host>:<sprite>` flag, mount on launch, unmount on exit.
- No writes (return EROFS).
- Validates the architecture, the transport, and the test infrastructure.

**Phase 2 — read-write**
- Write-back-on-release, `fsync`, atomic rename, create, unlink.
- Host watcher pushes invalidations on local edits.
- Tests for editor atomic save, ENOSPC propagation, fsync barrier under
  crash, host-watcher-vs-in-flight-write race.

**Phase 3 — quality of life**
- `spritebox share` subcommands, `.spriteboxignore` for exclusions, prefetch
  tuning, share survives daemon restart, multiple concurrent shares.

**Phase 4 — image bake**
- Ship `spritebox-fsd` and the `chmod` in the sprite base image so launch is
  faster and provisioning is one less moving part.
