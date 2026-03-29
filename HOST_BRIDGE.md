# Host Bridge

The host bridge lets code running inside a sprite trigger actions on the host
machine (the Mac running spritebox). It works over the existing WebSocket
console connection — no reverse tunnel or extra ports required.

## How It Works

```
Guest (sprite)                         Host (spritebox CLI)
    |                                      |
    |  Script emits OSC 9999 escape        |
    |  sequence to the TTY                 |
    |  ──────────────────────────────────> |
    |                                      |
    |  Host's console_read_loop detects    |
    |  the escape, strips it from output,  |
    |  and dispatches the bridge action    |
    |                                      |
    |  For paste-image: host grabs         |
    |  clipboard, pushes file to sprite    |
    |  via filesystem API                  |
    |  <────────────────────────────────── |
    |                                      |
    |  Guest script polls for the file     |
```

### Escape Sequence Format

```
\x1b]9999;<verb>;<payload>\x1b\\
```

- `\x1b]9999;` — OSC (Operating System Command) with private parameter 9999
- `<verb>` — the bridge action to perform
- `<payload>` — action-specific data (usually a path or URL)
- `\x1b\\` — String Terminator (ST); `\x07` (BEL) is also accepted

The sequence is intercepted by spritebox and never reaches the host terminal.

### Supported Verbs

| Verb | Payload | Host Action |
|------|---------|-------------|
| `browser-open` | URL | Opens the URL in the host's default browser (`open` on macOS) |
| `open` | Absolute path on sprite | Prompts the user, downloads the file from the sprite, opens it with the host's default application. Allowed extensions: `html`, `htm`, `svg`, `png`, `jpg`, `jpeg`, `pdf`, `md`, `txt`, `rtf`, `csv` |
| `paste-image` | Absolute path on sprite | Prompts the user, grabs the clipboard image, pushes it to the sprite via the filesystem API |

## Guest-Side Scripts

Three helper scripts are installed in the sprite at `/usr/local/bin/` during
launch (by `install_bridge_scripts`):

### `sprite-browser <url>`

Opens a URL in the host's browser. Used instead of `open` or `xdg-open`
which don't work inside the VM.

Emits: `\x1b]9999;browser-open;<url>\x1b\\`

### `sprite-open <path>`

Opens a file from the sprite on the host machine. The file is downloaded
to a temp directory on the host and opened with the default application.

1. Resolves relative paths to absolute
2. Verifies the file exists on the sprite
3. Emits `\x1b]9999;open;<path>\x1b\\` to the TTY

The host side shows a macOS confirmation dialog before downloading and opening.

### `spritebox-paste-image <absolute-path>`

Imports a screenshot/image from the host's clipboard into the sprite.

1. Emits `\x1b]9999;paste-image;<path>\x1b\\` to the TTY
2. Polls for the file to appear (up to 30 seconds)
3. Exits 0 once the file exists, or 1 on timeout

The host side shows a macOS confirmation dialog before transferring.

## How the Sprite Knows About the Bridge

The bridge is documented to LLMs running inside the sprite via three paths:

1. **`~/.claude/CLAUDE.md`** — A spritebox environment section is written
   (idempotently, between `<!-- spritebox environment -->` markers) on each
   launch. It tells Claude Code to use `sprite-browser` and
   `spritebox-paste-image` instead of native tools, and references
   `/.sprite/llm.txt` and `/.sprite/docs/agent-context.md` for further
   context.

2. **`~/.codex/instructions.md`** — The same document is written for Codex.

3. **`/spritebox/skills.md`** — A standalone copy for other tools.

The skills doc explicitly states:
- Do not use `open`, `xdg-open`, `pbpaste`, `xclip` — they don't work
- Use `sprite-browser` for URLs
- Use `sprite-open` for opening files on the host
- Use `spritebox-paste-image` for clipboard images
- This is a Linux VM, not macOS

## Host-Side Implementation

The detection and dispatch lives in `sprites_api.rs`:

- **`filter_bridge_escapes()`** — Scans binary WebSocket frames for
  `\x1b]9999;` sequences. Handles sequences split across frames by
  buffering partial prefixes. Returns the data with bridge sequences
  stripped.

- **`handle_bridge_command()`** — Dispatches by verb:
  - `browser-open` → `std::process::Command::new("open").arg(url).spawn()`
  - `open` → `tokio::spawn` an async task that:
    1. Shows a macOS confirmation dialog (AppleScript)
    2. Downloads the file from the sprite via `GET /v1/sprites/{name}/fs/read`
    3. Writes to a temp file (preserving extension) and opens with `open`
  - `paste-image` → `tokio::spawn` an async task that:
    1. Shows a macOS confirmation dialog (AppleScript)
    2. Exports clipboard image to a temp PNG (AppleScript)
    3. Pushes the file to the sprite via `PUT /v1/sprites/{name}/fs/write`

## Frame Splitting

OSC sequences can be split across WebSocket binary frames. The parser
handles this by:

1. If a frame ends with a partial `\x1b]9999;` prefix, it's buffered
2. The next frame checks if the buffer + new data completes the prefix
3. If so, it continues scanning for the ST terminator
4. If the buffer doesn't match, it's flushed to output as regular data
