# yolobox

Branch-scoped Linux VMs for local development on macOS. Each repo branch gets its own persistent VM with a writable root disk, a shared git checkout, and a stable network identity.

```bash
yolobox base import --name ubuntu --image ./ubuntu-jammy-arm64.raw
yolobox --repo git@github.com:org/repo.git --branch main --with-ai
```

You're dropped into a Linux shell with your repo at `/workspace`, your SSH agent forwarded, and guest services reachable from the host at `<project>-<branch>.local` (e.g. `repo-main.local`).

## How It Works

- Import a Linux cloud image once as an immutable **base image**
- Each launch clones that base (APFS copy-on-write) into a per-instance root disk
- `cloud-init` configures the guest: user account, SSH key, hostname, mounts
- `krunkit` boots the VM with `virtio-blk` (root disk) and `virtio-fs` (host directories)
- `vmnet-helper` gives the guest a real IP on your local network with mDNS (`<hostname>.local`)

Instances are persistent. Relaunching the same repo+branch reuses the existing root disk and checkout.

## Prerequisites

macOS on an APFS volume, plus:

```bash
# VM runtime
brew tap slp/krunkit
brew install krunkit

# Networking
curl -fsSL https://raw.githubusercontent.com/nirs/vmnet-helper/main/install.sh | sudo bash

# Image conversion (only if your base image is qcow2)
brew install qemu
```

You need an SSH key at `~/.ssh/id_ed25519`, `id_ecdsa`, or `id_rsa`. Create one if you don't have it:

```bash
ssh-keygen -t ed25519
```

Check readiness:

```bash
yolobox doctor
```

## Getting a Base Image

Any Linux image that supports EFI boot, `cloud-init`, `sshd`, `virtio-blk`, and `virtio-fs` will work. Ubuntu cloud images are a good default:

```bash
curl -LO https://cloud-images.ubuntu.com/jammy/current/jammy-server-cloudimg-arm64.img
qemu-img convert -f qcow2 -O raw jammy-server-cloudimg-arm64.img ubuntu-jammy-arm64.raw
yolobox base import --name ubuntu --image ./ubuntu-jammy-arm64.raw
```

Skip the `qemu-img` step if your download is already a raw image.

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
```

The guest hostname defaults to `<project>-<branch>` for git-backed instances (e.g. `myrepo-main`). Unnamed standalone instances get a random petname instead.

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

Forward host credentials into the guest:

| Flag | What it shares |
|------|---------------|
| `--with-ai` | All three below |
| `--with-claude` | `~/.claude` directory + `ANTHROPIC_API_KEY` env var |
| `--with-codex` | `~/.codex` directory |
| `--with-gh` | `~/.config/gh` directory + `GH_TOKEN` from `gh auth token` |

These are `virtio-fs` mounts, so they persist like any other share.

### Init Scripts

Run a first-boot script inside the guest:

```bash
yolobox --base ubuntu --init-script ./scripts/bootstrap-vm.sh
```

The script runs once as the guest user (with `sudo` available), and its output goes to `/var/log/yolobox-init.log` inside the guest. A sample bootstrap script is included at `scripts/bootstrap-vm.sh` that installs Rust, Node.js, Python tooling, and common dev packages.

### Cloud-Init Overrides

```bash
yolobox --repo git@github.com:org/repo.git --branch main \
  --cloud-user josh \
  --hostname my-vm \
  --ssh-pubkey ~/.ssh/id_ed25519.pub \
  --ssh-private-key ~/.ssh/id_ed25519
```

Disable cloud-init entirely with `--no-cloud-init` if your base image is already configured.

### Managing Instances

```bash
yolobox list                                            # all instances and their state
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
