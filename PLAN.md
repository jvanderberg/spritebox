# Spritebox: yolobox with Fly.io Sprites Backend

## Overview

Replace yolobox's local krunkit/vmnet VM backend with Fly.io Sprites --
remote, persistent, hardware-isolated Linux VMs that boot in ~1 second,
auto-sleep when idle, and cost nothing while dormant.

The CLI experience stays similar: branch-scoped environments, deterministic
naming, interactive shell, exec mode. But instead of managing local disk
images, APFS clones, krunkit processes, and vmnet networking, spritebox
delegates all of that to the Sprites API.

---

## Concept Mapping

| yolobox (local)                  | spritebox (Sprites)                       |
|----------------------------------|-------------------------------------------|
| krunkit microVM on macOS         | Fly.io Sprite (Firecracker VM, remote)    |
| APFS clone of base image         | Sprite creation (standard base image)     |
| Base images (`base import`)      | Checkpoints (`sprite checkpoint create`)  |
| cloud-init provisioning          | Exec-based provisioning (init script)     |
| vmnet static IP + mDNS           | Sprite public URL + `sprite proxy`        |
| SSH transport                    | WebSocket exec/console (Sprites API)      |
| virtiofs file sharing            | Filesystem API (read/write/list/delete)   |
| host bridge (file-based IPC)     | Drop or reimplement over exec/filesystem  |
| `~/.local/state/yolobox/`       | Minimal local state (instance→sprite map) |
| krunkit.pid process tracking     | Sprites auto-sleep/wake, no local process |
| Port forwarding (host_port_base) | `sprite proxy` or public HTTPS URL        |
| doctor (check krunkit, vmnet)    | doctor (check sprite CLI, auth, org)      |

---

## What Gets Removed

These modules are entirely local-VM concerns and can be deleted or gutted:

- **`cloud_init.rs`** -- No cloud-init needed; Sprites boot from a standard image.
- **`network.rs`** -- No vmnet, no MAC addresses, no static IPs.
- **Disk image management** in `state.rs` -- No APFS clones, no base images on disk, no rootfs paths.
- **krunkit launch logic** in `runtime.rs` -- No local VM process to spawn/track.
- **vmnet-helper integration** -- Gone entirely.

## What Gets Replaced

- **`runtime.rs`** -- Rewrite to call the Sprites API instead of spawning krunkit.
  The `LaunchMode` enum collapses to: `Sprite` (remote VM) and `Shell` (local, for testing).
- **`state.rs`** -- Simplify dramatically. Local state becomes a thin mapping:
  `instance_id → sprite_name` plus cached metadata. No disk images, no APFS clones.
- **SSH transport** -- Replace with Sprites WebSocket exec/console. The `sprite` CLI
  or the REST API provides interactive TTY sessions and one-shot command execution.

## What Stays (Mostly Unchanged)

- **`app.rs`** -- CLI flag parsing, command dispatch, branch selection, interactive prompts.
  The user-facing commands (`launch`, `exec`, `list`, `status`, `stop`, `destroy`) stay,
  but their implementations call Sprites instead of local VM machinery.
- **`git.rs`** -- Git operations for branch discovery, checkout management. Still needed
  for the branch-scoping identity model. Though file sync changes (see below).
- **`host_bridge.rs`** -- Needs rethinking (see open questions). The file-based IPC
  over virtiofs won't work. Could reimplement a subset over the filesystem API.

---

## Architecture

```
CLI (app.rs)
  |
  +-- Instance identity (repo+branch → sprite name)
  |     Same slug logic as today
  |
  +-- Sprites API client (new module: sprites_api.rs)
  |     - Create / destroy / list sprites
  |     - Exec commands (one-shot and interactive)
  |     - Console (interactive TTY via WebSocket)
  |     - Filesystem operations (upload/download)
  |     - Checkpoint create / restore
  |     - Proxy management
  |
  +-- File sync (new module: sync.rs)
  |     - Push local checkout → sprite filesystem
  |     - Pull sprite filesystem → local checkout
  |     - Or: use `sprite proxy` + rsync/scp over SSH
  |
  +-- Local state (simplified state.rs)
        - instance_id → sprite_name mapping
        - Cached sprite status
        - No disk images, no base images on disk
```

---

## Command Mapping

### `spritebox` (launch)
1. Resolve instance identity from repo+branch (same as today)
2. Check if sprite `{instance-id}` exists via API
3. If not: `POST /v1/sprites` with name = instance-id
4. If init script provided: exec it on the sprite
5. Sync workspace files to sprite (see file sync strategy)
6. Open interactive console via WebSocket

### `spritebox exec -- cmd`
1. Resolve instance identity
2. Ensure sprite exists and is awake
3. `POST /v1/sprites/{name}/exec` with command
4. Stream stdout/stderr back, return exit code

### `spritebox list`
1. `GET /v1/sprites` -- list all sprites in org
2. Match against local instance mappings
3. Display table with status (running/sleeping/stopped)

### `spritebox status`
1. `GET /v1/sprites/{name}` for specific sprite details
2. Show: name, status, CPU/RAM, storage used, last active, public URL

### `spritebox stop`
- Sprites auto-sleep. `stop` could either be a no-op with a message
  ("sprites auto-sleep after 30s of inactivity") or explicitly destroy
  the sprite if the user wants to free resources.

### `spritebox destroy`
1. `DELETE /v1/sprites/{name}`
2. Remove local state mapping

### `spritebox base import` → `spritebox snapshot`
- Reframe as checkpoint management:
  - `spritebox snapshot create` → `POST /v1/sprites/{name}/checkpoints`
  - `spritebox snapshot restore` → `POST /v1/sprites/{name}/checkpoints/{id}/restore`
  - `spritebox snapshot list` → `GET /v1/sprites/{name}/checkpoints`

### `spritebox doctor`
- Check: `sprite` CLI installed and in PATH
- Check: authenticated (`sprite login` done, valid token)
- Check: org access
- Check: `gh auth token` succeeds (needed for git credentials in sprite)

---

## File Sync Strategy

yolobox uses virtiofs for zero-copy host↔guest file sharing. Sprites are
remote, so we use a hybrid approach:

- **Repo code**: Git-based. Clone the repo inside the sprite on first launch,
  `git pull` on subsequent launches. The sprite has git credentials (see
  "Git credentials inside the sprite" below) so it talks directly to the
  remote. No need to push the full working tree from the host.
- **Config directories**: Filesystem API push. Small trees like `~/.claude`,
  `~/.config/gh`, cargo config are pushed from host→sprite at launch time
  via `POST /v1/sprites/{name}/filesystem/{path}`.
- **Ad-hoc file transfer**: Filesystem API for individual files (host bridge
  `open` pulls a file from sprite, `paste-image` pushes to sprite).

---

## Open Questions

1. **Host bridge** (resolved): Run a lightweight HTTP server on the host side,
   reverse-tunneled into the sprite so the guest can reach it at
   `localhost:<bridge-port>`. Same request/response semantics as the current
   file-based IPC, just over HTTP:
   ```
   Guest (sprite)                     Host (spritebox CLI)
       |                                     |
       |  POST localhost:9111/open           |
       |  {"path": "/workspace/out.html"}    |
       | ----------------------------------> |
       |                                     |  opens in browser
       |  200 OK                             |
       | <---------------------------------- |
   ```
   Supported verbs: `open` (open file on host -- pull from sprite first),
   `open-url` (browser), `code` (VS Code), `paste-image` (screenshot from
   host clipboard → push to sprite). `finder` is dropped.
   Same validation rules (allowlisted paths/extensions) as today.
   The tunnel can be established via `sprite proxy` in reverse mode or by
   running sshd in the sprite and using SSH remote forwarding.

2. **AI credential sharing** (resolved): A file-push API in the sprites client
   that walks a local directory and uploads it to a target path on the sprite
   via the filesystem API (`POST /v1/sprites/{name}/filesystem/{path}`).
   Used at launch time to sync small config directories:
   - `~/.claude` → `/home/{user}/.claude`
   - `~/.codex` → `/home/{user}/.codex`
   - `~/.config/gh` → `/home/{user}/.config/gh`
   - `~/.cargo/config.toml`, `~/.cargo/credentials.toml` → snapshotted
   Env vars like `ANTHROPIC_API_KEY` and `GH_TOKEN` are passed via the exec
   API's environment support. This replaces the virtiofs mounts entirely --
   it's a one-way push (host→guest) on each launch, not a live shared mount.

3. **Port forwarding UX** (resolved): Sprites already provide a public HTTPS
   URL per sprite (maps to port 8080 inside). For web dev, that's the primary
   access method -- `spritebox url` exposes it. For other ports, `sprite proxy`
   can forward on demand. No need to replicate yolobox's deterministic
   host-port mapping; the sprite name *is* the stable identifier.

4. **Multi-platform** (resolved): Cross-platform by default since the
   macOS-only components (krunkit, APFS, vmnet) are gone. The only
   platform-specific code is in the host bridge verbs:
   - **Open file**: Guest requests a file be opened on the host. The host
     bridge pulls the file from the sprite via filesystem API, then opens
     it with the platform default handler (`open` / `xdg-open` / `start`).
     Works cross-platform.
   - **Open URL in browser**: Same cross-platform pattern.
   - **Screenshot / clipboard paste**: `pbpaste` (macOS) / `xclip` (Linux) /
     PowerShell (Windows). Guest requests screenshot, host grabs clipboard
     and pushes the image to the sprite via filesystem API.
   - **VS Code**: already cross-platform (`code` CLI)
   - **Finder**: Dropped. No replacement.
   Abstract these behind a small `platform.rs` module with `cfg(target_os)` gates.

5. **Authentication model** (resolved): `spritebox auth` handles the full
   Fly.io token lifecycle -- no dependency on the `sprite` CLI. Flow:
   - `spritebox auth login` -- opens browser for Fly.io OAuth, stores token
   - `spritebox auth status` -- shows current org, token validity
   - `spritebox auth logout` -- revokes/removes stored token
   - Token stored in `~/.config/spritebox/auth.json` (or platform equivalent)
   - Supports `SPRITEBOX_TOKEN` env var override for CI/automation
   - `spritebox doctor` checks auth status as part of preflight

6. **Offline mode** (resolved): No offline mode. Sprites require network;
   that's a known trade-off. Remove the `Shell` launch mode.

7. **SDK choice** (resolved): Build a native Rust API client using reqwest
   (HTTP) and tokio-tungstenite (WebSocket). No dependency on the `sprite`
   CLI at all -- spritebox is fully self-contained. Module: `sprites_api.rs`.
   - reqwest for REST calls (CRUD, filesystem, checkpoints)
   - tokio-tungstenite for WebSocket console/exec streaming
   - Auth token injected from `spritebox auth` storage or env var
   - Typed request/response structs, proper error handling

---

## Additional Design Decisions

### Git credentials inside the sprite

yolobox forwards `SSH_AUTH_SOCK` so the guest can git push/pull with the
host's SSH keys. No agent forwarding exists for remote sprites, and we
don't want private keys on the sprite.

Solution: pass `GH_TOKEN` as an env var through the exec/console API.
No secrets touch the sprite's filesystem.
- `GH_TOKEN` is set in the session environment at launch time
- `gh` CLI uses it directly for GitHub API operations
- `git` uses it for HTTPS auth (via `gh auth setup-git` in the init script)
- Token lives only in the shell session's memory; gone when session ends
- Requires HTTPS remotes (not SSH), which `gh` handles naturally
- `spritebox doctor` warns if `gh auth token` fails on the host

### Host bridge architecture

The sprites proxy tunnels host→sprite (forward direction). The guest needs
to reach the host (reverse direction) for bridge requests. Rather than
requiring a reverse tunnel, use a **persistent WebSocket exec session** as
the bridge channel:

```
Host (spritebox CLI)                     Guest (sprite)
    |                                        |
    |  WebSocket exec: bridge-daemon         |
    | -------------------------------------> |
    |                                        |
    |  bridge-daemon watches /spritebox/requests/
    |  via inotifywait (or poll loop)        |
    |                                        |
    |  Guest writes request file:            |
    |  /spritebox/requests/open.xxxxx        |
    |                                        |
    |  bridge-daemon detects it, prints      |
    |  JSON to stdout over WebSocket         |
    | <------------------------------------- |
    |                                        |
    |  Host processes request (opens file,   |
    |  grabs clipboard, etc.)                |
    |                                        |
    |  Host writes response via filesystem   |
    |  API → /spritebox/responses/open.xxxxx |
    | -------------------------------------> |
    |                                        |
    |  Guest polls for response, picks it up |
```

This keeps the same request/response file semantics the guest-side scripts
already use. The bridge-daemon is a small shell script pushed to the sprite
on first boot. The WebSocket exec session stays open for the lifetime of
the spritebox CLI session, with automatic reconnect on drop.

Alternative (simpler, higher latency): skip the exec session entirely and
have the host poll `/spritebox/requests/` via the filesystem API on a timer.
Start with whichever is simpler to implement; optimize later.

### Init script idempotency

yolobox used cloud-init's built-in run-once semantics. With exec-based
provisioning, spritebox manages this explicitly:

1. On first launch, after init script runs successfully, create a marker:
   `POST /v1/sprites/{name}/filesystem/spritebox/.init-done`
   containing the hash of the init script content.
2. On subsequent launches, check for the marker via filesystem API.
   - If present and hash matches: skip init script.
   - If present but hash differs: re-run (script changed).
   - If absent: run init script.

### User identity

Create a user inside the sprite matching the local username (from `$USER`
or `whoami`). On first boot, the init sequence runs:
```
useradd -m -s /bin/bash -G sudo {username}
echo '{username} ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/{username}
```
Subsequent exec/console sessions run as this user. If `--user` is specified,
use that instead.

### Resource sizing

Sprites support CPU and memory configuration. Expose via CLI flags:
- `--cpus N` (default: 4, matching yolobox)
- `--memory N` (MiB, default: 8192, matching yolobox)
- Passed to `POST /v1/sprites` at creation time
- Stored in local instance state so status can display them

### X11 forwarding

Backlogged. Not included in initial implementation. Sprites have outbound
networking, so X11-over-SSH could work if sshd is running in the sprite
with `-X` forwarding. Revisit later.

### WebSocket resilience

All WebSocket connections (console, exec, bridge) use aggressive reconnect:
- On disconnect: immediate retry, then exponential backoff (100ms → 1s → 5s)
- Console sessions: reconnect transparently, re-attach to the same shell
  (sprite exec sessions persist and can be re-attached by ID)
- Bridge daemon: restart the exec session on reconnect
- User sees a brief "reconnecting..." indicator, not an error

---

## Phased Implementation

### Phase 1: Core lifecycle + auth
- `spritebox auth login/status/logout` with token storage
- New `sprites_api.rs` module (reqwest + tokio-tungstenite)
- Sprite CRUD: create (with --cpus, --memory), destroy, list, status
- Simplified `state.rs` with instance→sprite name mapping
- `spritebox doctor` checking auth, network connectivity

### Phase 2: Interactive console and exec
- WebSocket console integration for `spritebox` (launch)
- HTTP exec for `spritebox exec`
- Terminal handling (PTY, resize, signals) over WebSocket
- Aggressive reconnect on WebSocket drops
- User provisioning (create matching local user on first boot)

### Phase 3: File sync + provisioning
- Git credential injection (GH_TOKEN env var via exec API, HTTPS remotes)
- Git-based sync (clone repo inside sprite on first launch, pull on reconnect)
- Filesystem API push for config dirs (~/.claude, ~/.config/gh, etc.)
- Env var injection (ANTHROPIC_API_KEY, GH_TOKEN, etc.)
- Init script execution via exec with idempotency marker

### Phase 4: Host bridge
- Bridge daemon script pushed to sprite
- Persistent WebSocket exec session as bridge channel
- Host-side request processing (open file, open URL, paste-image)
- `platform.rs` with cross-platform handlers
- Guest helper scripts (spritebox-open, spritebox-paste-image, code)

### Phase 5: Networking + URLs
- Public HTTPS URL management (`spritebox url`)
- `sprite proxy` integration for non-8080 ports
- Checkpoint management (snapshot create/restore/list)

### Phase 6: Polish
- Cross-platform testing (macOS, Linux, Windows)
- WebSocket resilience hardening
- Error handling, retries, edge cases
- X11 forwarding (stretch goal)
