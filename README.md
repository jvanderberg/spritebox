# spritebox

Branch-scoped development environments powered by [Fly.io Sprites](https://sprites.dev).
Each branch gets its own persistent remote Linux VM that boots in seconds,
auto-sleeps when idle, and costs nothing while dormant.

spritebox is the successor to yolobox. Instead of managing local krunkit VMs,
APFS clones, and vmnet networking, spritebox delegates everything to the
Sprites API — remote Firecracker VMs with persistent storage.

```bash
# Launch a sprite for a repo branch
spritebox --repo git@github.com:org/repo.git --branch main

# Launch a standalone sprite by name
spritebox --name tools-box

# Run a command inside a sprite
spritebox exec --name pico-gamer-main -- uname -a
```

In repo mode, you're dropped into a shell with your repo cloned at `/workspace`,
git credentials configured, and Claude/Codex ready to go.

## How It Works

Sprites are remote Firecracker microVMs managed by Fly.io. They boot in ~1 second,
auto-sleep after 30 seconds of inactivity, and wake automatically when you reconnect.
Storage persists across sleep/wake cycles.

spritebox handles:
- Sprite lifecycle (create, connect, stop, destroy)
- User provisioning (matching your local username)
- Git clone and credential setup (HTTPS via GH_TOKEN)
- Config sync (Claude, Codex, GitHub configs pushed from host)
- Host bridge (open files, URLs, clipboard images across the VM boundary)
- LLM environment docs (skills docs so Claude/Codex know how to use the bridge)

Interactive sessions use WebSocket connections to the Sprites API — no SSH needed.

## Yolo Mode

Claude and Codex are pre-installed in the sprite. Just type `claude` or `codex`.

## Setup

### Prerequisites

- A [Fly.io](https://fly.io) account with the `fly` CLI installed and authenticated
- `gh` CLI installed and authenticated (for git credentials in sprites)

### Install

```bash
cargo install --path .
```

### Authenticate

```bash
# Exchange your Fly.io token for a Sprites API token
spritebox auth login

# Store Claude Code OAuth token for use in sprites
spritebox auth setup-claude

# Store OpenAI API key for Codex
spritebox auth setup-codex

# Check everything is configured
spritebox doctor
```

## Usage

### Launching Sprites

Launch a sprite for a repo branch:

```bash
spritebox --repo git@github.com:org/repo.git --branch main
```

Omit `--branch` to pick from recent remote branches interactively.

Launch a standalone sprite (no git checkout):

```bash
spritebox --name my-sandbox
```

### Running Commands

```bash
spritebox exec --name repo-main -- cargo build
spritebox exec --repo git@github.com:org/repo.git --branch main -- ls /workspace
```

### Managing Sprites

```bash
spritebox list                                              # all sprites and their status
spritebox stop --name repo-main                             # stop a running sprite
spritebox destroy --name repo-main                          # delete a sprite
spritebox destroy --repo git@github.com:org/repo.git --branch main --yes
```

### AI Integration

AI credentials are synced to the sprite on each launch:

- **Claude**: `~/.claude` config and `CLAUDE_CODE_OAUTH_TOKEN` or `ANTHROPIC_API_KEY`
- **Codex**: `~/.codex` config and `OPENAI_API_KEY`
- **GitHub**: `GH_TOKEN` via `gh auth token`, configured with `gh auth setup-git`
- **Git identity**: `user.name` and `user.email` from host git config

Disable with `--no-claude` or `--no-codex`.

### Host Bridge

The host bridge lets code inside the sprite trigger actions on the host machine.
It works over OSC 9999 escape sequences on the WebSocket console connection — no
reverse tunnel or extra ports needed.

**Open a file on the host:**
```bash
sprite-open /workspace/output/report.html
```
Downloads the file from the sprite and opens it with the host's default application.
Requires user confirmation. Allowed file types: html, htm, svg, png, jpg, jpeg, pdf, md, txt, rtf, csv.

**Open a URL in the host browser:**
```bash
sprite-browser https://example.com
```

**Import clipboard image from host:**
```bash
spritebox-paste-image /workspace/screenshot.png
```
Grabs the image from the host's clipboard and pushes it to the sprite.
Requires user confirmation. Waits up to 30 seconds for the transfer.

These helpers are installed automatically at `/usr/local/bin/` on each launch.
They do not expose arbitrary host command execution.

For full details, see [HOST_BRIDGE.md](HOST_BRIDGE.md).

### Accessing Services

Each sprite gets a public HTTPS URL that routes to port 8080 inside the VM.
The URL is available as `$SPRITE_URL` in the environment and shown by
`sprite-env info` inside the sprite.

To open it on the host:
```bash
sprite-browser $SPRITE_URL
```

### Diagnostics

```bash
spritebox doctor
```

Checks: Sprites API authentication, GitHub token, git identity.

## Safety

The sprite creates a hardware-isolated sandbox for agentic AI. Sprites are
Firecracker microVMs with their own kernel, filesystem, and process space.

Credentials shared with the sprite:
- GitHub token (GH_TOKEN) — lives only in shell session memory
- Claude/Codex OAuth tokens — synced to config files in the sprite
- Git author identity

The host bridge restricts what the sprite can do on the host:
- File downloads require user confirmation and are limited to safe file types
- Clipboard imports require user confirmation
- URL opening is unrestricted (browser handles safety)
- No arbitrary command execution on the host

## Building from Source

```bash
cargo build
cargo run -- doctor
```
