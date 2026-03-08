# yolobox

Use branch-scoped fast-launch micro-VMs for AI-safe development on macOS. 
Each branch gets its own persistent VM with a writable root disk,
 a shared git checkout, and a stable network identity.

```bash
# Launch a new VM
yolobox

# Check out a git branch in a new VM
yolobox --repo git@github.com:org/repo.git --branch main

# Launch a previous checkout by repo-branch
yolobox --name markless-fix-edit-mode-width
```

In branch mode, you're dropped into shell with your repo at `/workspace`, 
your SSH agent forwarded, and network services reachable from the host 
at `<project>-<branch>.local` (e.g. `repo-main.local`).

New VM creation/first-boot takes about 15 seconds, subsequent launches take less than a second.

## Yolo mode

Claude and Codex are pre-installed in the guest environment and are set to automatically
use their respective --dangerously-skip* modes. Just type `claude` or `codex`

## How It Works

On creation, an immutable root Ubuntu image is cloned using APFS copy-on-write,
The new image takes almost no extra space on disk, only changes are stored. All changes are persistent
for the life of the VM, and scoped to only that VM.

krunkit orchestrates the VM, mapping the git repo in using virtio-fs, mounted as /workspace.

vmnet-helper gives the VM an IP on your local network, and avahi-daemon broadcasts a .local domain name using mDNS.

On exit the VM is left running for fast/warm startup on next launch. You can show all instances and their status with 

```bash
yolobox list
```

And you can stop running VMs with 
```bash
yolobox stop --name repo-main
```

## Safety

The VM creates a safer sandbox for running agentic AI in 'yolo' mode. But no sandbox is perfect.

The VM can access only its clone of the root fs, the mapped git repo, and any other local shares you map
with --share. Be careful with the directories you share if you want to limit the blast radius.

The VM has full access to the network, there is no firewall or outbound blocking/filtering.

If you do not turn off AI integrations (--no-ai), your GitHub credentials will be shared with the VM,
 ~/.codex and ~/.claude will be mapped into the VM as filesystem shares, and the respective environmental keys 
will be shared.
This is no different than the access codex or claude would have if you ran them locally, but remember they will
both be running in 'yolo' mode, so they have fewer guardrails and checks on what they do. You won't get prompted
before codex stores your API key in a file and pushes it to the repo.

### SSH Keys

The path to your public key is used to copy that public key into the guest as an authorized key.
The SSH agent is forwarded into the guest to support outbound SSH.

The path to your private key is used only by the host-side launcher when it SSHes into the guest.
That private key is not copied into the VM in any way.

## Install

The recommended installation path is the installer script.

Run it directly from GitHub:

```bash
curl -fsSL https://raw.githubusercontent.com/jvanderberg/yolobox/main/scripts/setup.sh | bash -s -- install
```

Or run it from a local checkout:

```bash
git clone https://github.com/jvanderberg/yolobox.git
cd yolobox
./scripts/setup.sh install
```

The installer:

- installs host dependencies
- builds and installs `yolobox`
- downloads the Ubuntu cloud image
- imports a clean `ubuntu` base
- installs common dev tools and snapshots an `ubuntu-dev` base

Check readiness afterward:

```bash
yolobox doctor
```

For a manual install and base-image setup, see [manual-install.md](/Users/joshv/git/local_sprite/manual-install.md).

## Getting a Base Image

The installer prepares the default Ubuntu-backed bases for you.

If you want to import your own image manually or manage bases by hand, use the instructions in [manual-install.md](/Users/joshv/git/local_sprite/manual-install.md).

## Usage

### Launching Instances

Launch a VM for a repo branch:

```bash
yolobox --repo git@github.com:org/repo.git --branch main
```

Omit `--branch` to pick from recent remote branches interactively. Omit `--base` and yolobox uses the newest imported base image.

Launch a standalone VM (no git checkout):

```bash
yolobox --base ubuntu
yolobox --name tools-box --base ubuntu    # with an explicit name
# name auto-assigned, default base
yolobox
```

The guest hostname defaults to `<project>-<branch>.local` for git-backed instances (e.g. `myrepo-main.local`). 
Unnamed standalone instances get a random petname instead.

Create a new branch:

```bash
yolobox --repo git@github.com:org/repo.git --branch feature/x --new-branch
yolobox --repo git@github.com:org/repo.git --branch feature/x --new-branch --from develop
```

Tune VM resources:

```bash
yolobox --repo git@github.com:org/repo.git --branch main --cpus 6 --memory-mib 12288
```

### Sharing Host Directories

Share extra directories into the guest as `virtio-fs` mounts:

```bash
yolobox --repo git@github.com:org/repo.git --branch main \
  --share ~/Downloads:/mnt/downloads \
  --share ~/src/shared-assets:/mnt/assets
```

Shares are persisted with the instance -- later launches reuse them automatically. If you change the share set on a running VM, yolobox restarts it to apply the new mounts. Clear saved shares with `--clear-shares`.

### AI and Dev Tool Integration

AI integrations are enabled by default. On launch, yolobox will try to share the host config directories for Codex, Claude, and GitHub into the guest when they exist.


Claude: shares `~/.claude` and exports `ANTHROPIC_API_KEY`
Codex: shares `~/.codex`
GitHub: shares `~/.config/gh` and exports `GH_TOKEN` 

`--no-ai` disables this behavior. You can independently disable integrations with `--no-claude`, `--no-codex`, and `--no-gh`.

These are `virtio-fs` mounts, so they persist like any other share. 

yolobox also installs a profile script in the guest that makes `claude` run with `--dangerously-skip-permissions` and `codex` run with `--dangerously-bypass-approvals-and-sandbox` by default. Use `command claude ...` or `command codex ...` if you want the raw CLI behavior in a shell.

### Init Scripts

Run a first-boot script inside the guest:

```bash
yolobox --base ubuntu --init-script ./scripts/bootstrap-vm.sh
```

The script runs once as the guest user (with `sudo` available), and its output goes to `/var/log/yolobox-init.log` inside the guest. A sample bootstrap script is included at `scripts/bootstrap-vm.sh` that installs Rust, Node.js, Python tooling, and common dev packages.

### Cloud-Init Overrides

Disable cloud-init entirely with `--no-cloud-init` if your base image is already configured.

### Managing Instances

```bash
# all instances and their state
yolobox list                                           
yolobox status --repo git@github.com:org/repo.git --branch main
yolobox stop   --repo git@github.com:org/repo.git --branch main   # shut down VM, keep data
yolobox destroy --repo git@github.com:org/repo.git --branch main  # remove everything
yolobox destroy --name tools-box --yes                  # skip confirmation
```

### Managing Base Images

```bash
yolobox base list
yolobox base import --name ubuntu-24.04 --image /path/to/ubuntu.img
yolobox base capture --name ubuntu-dev --repo git@github.com:org/repo.git --branch main
yolobox base capture --name ubuntu-dev --instance tools-box
```

`base capture` snapshots a running instance's root disk as a new immutable base. Base names can't be overwritten in place -- remove the old one first if you need to reuse the name.

## Accessing Guest Services

Services in the guest are reachable via mDNS:

```
http://myrepo-main.local:3000
http://myrepo-main.local:5173
ssh josh@myrepo-main.local
```

yolobox does not create localhost port forwards. The guest gets a deterministic static IP on the `192.168.105.0/24` subnet (derived from the instance ID), and `avahi-daemon` advertises its hostname over mDNS.

## Instance Layout

All state lives under `~/.local/state/yolobox` (override with `YOLOBOX_HOME`):

```
~/.local/state/yolobox/
  base-images/<id>/
    base.img              # read-only base image (APFS clone of import)
    base.env              # metadata
  instances/<id>/
    instance.env          # metadata: base image, ports, shares, env vars
    checkout/             # persistent git working tree
    vm/branch.img         # writable root disk (APFS clone of base)
    cloud-init/seed.iso   # cloud-init seed
    runtime/
      console.log         # VM console output
      krunkit.pid         # process tracking
```

Branch disks default to a sparse 32 GiB rootfs (override with `YOLOBOX_ROOTFS_MIB`). The guest partition and filesystem are grown automatically on first boot.

## External Launchers

Set `YOLOBOX_VM_LAUNCHER` to use your own VM launcher instead of the built-in `krunkit` path. The launcher receives instance metadata as environment variables:

`YOLOBOX_INSTANCE`, `YOLOBOX_REPO`, `YOLOBOX_BRANCH`, `YOLOBOX_CHECKOUT`, `YOLOBOX_BASE_IMAGE`, `YOLOBOX_BASE_IMAGE_ID`, `YOLOBOX_ROOTFS`, `YOLOBOX_ROOTFS_MB`, `YOLOBOX_CPUS`, `YOLOBOX_MEMORY_MIB`, `YOLOBOX_CLOUD_INIT_IMAGE`, `YOLOBOX_CLOUD_INIT_USER`, `YOLOBOX_HOSTNAME`, `YOLOBOX_GUEST_IP`, `YOLOBOX_GUEST_GATEWAY`, `YOLOBOX_GUEST_MAC`, `YOLOBOX_INTERFACE_ID`, `YOLOBOX_SSH_PRIVATE_KEY`, `YOLOBOX_PORTS`

Use `--shell` to skip the VM entirely and get a host shell in the checkout directory with the same env vars set.

## Building from Source

```bash
cargo build
./target/debug/yolobox doctor
```

While developing, `cargo run -- <args>` works in place of `yolobox <args>`.
