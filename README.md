# vibebox

`vibebox` is a branch-scoped workspace launcher. Each repo branch gets:

- a persistent Git checkout
- a persistent writable disk cloned from an immutable managed base image
- a stable host-port block so parallel branches do not collide on common dev ports

The current implementation is intentionally small and local-first:

- it prepares the branch workspace
- it checks out or creates the branch in a persistent checkout
- it clones a branch disk from a managed base image into `branch.img`
- it can launch a real VM through built-in `krunkit` + `vmnet-helper` networking or hand off to a configured external launcher

By default, branch disks are expanded to a sparse 32 GiB rootfs even if the imported base image is smaller. Override that with `VIBEBOX_ROOTFS_MIB`.

While developing inside this repo, invoke it with `cargo run -- ...`. After building, the binary is `./target/debug/vibebox`.

If you run `vibebox` with no arguments, it behaves like `vibebox launch` and tries to boot a standalone VM using the newest imported base image.
If you pass top-level flags, `launch` is also implied, so `vibebox --base ubuntu` works the same as `vibebox launch --base ubuntu`.

## Prerequisites

The built-in VM path is currently aimed at macOS with APFS.

Host requirements:

- macOS on an APFS volume
- Rust toolchain to build `vibebox`
- `krunkit`
- `vmnet-helper` / `vmnet-client`
- an SSH keypair in a common `~/.ssh` location such as `id_ed25519` or `id_rsa`
- a Linux base image that supports EFI, `virtio-blk`, `virtio-fs`, `cloud-init`, and `sshd`

Install the local prerequisites:

```bash
brew tap slp/krunkit
brew install krunkit
curl -fsSL https://raw.githubusercontent.com/nirs/vmnet-helper/main/install.sh | sudo bash
```

Create an SSH key if you do not already have one:

```bash
ssh-keygen -t ed25519 -f ~/.ssh/id_ed25519
```

Check whether the machine is ready:

```bash
cargo run -- doctor
```

Typical first-run flow:

```bash
cargo run -- base import --name ubuntu --image ./ubuntu-jammy-arm64.raw
cargo run -- launch \
  --repo git@github.com:org/repo.git \
  --branch main \
  --base ubuntu \
  --init-script ./scripts/bootstrap-vm.sh
```

You can also launch a base-only VM with no repo checkout at all:

```bash
cargo run -- launch --base ubuntu
```

If you do that without `--name`, `vibebox` generates a persistent petname-style instance name and uses it as the guest hostname too. Git-backed instances use the simple `<project>-<branch>` form. If you want a stable, explicit name instead:

```bash
cargo run -- launch --name tools-box --base ubuntu
```

## Getting a Base Image

You need a Linux guest image that supports:

- EFI boot
- `cloud-init`
- `sshd`
- `virtio-blk`
- `virtio-fs`

Ubuntu cloud images are a good starting point. A practical workflow is:

1. Download an Ubuntu cloud image.
2. Convert it to a raw disk image if needed.
3. Import that raw image into `vibebox`.

Example using an Ubuntu Jammy ARM64 cloud image:

```bash
curl -LO https://cloud-images.ubuntu.com/jammy/current/jammy-server-cloudimg-arm64.img
qemu-img convert -f qcow2 -O raw jammy-server-cloudimg-arm64.img ubuntu-jammy-arm64.raw
cargo run -- base import --name ubuntu --image ./ubuntu-jammy-arm64.raw
```

If your download is already a raw image, you can skip the `qemu-img convert` step.

You may need `qemu-img` locally for image conversion:

```bash
brew install qemu
```

## Base Images

Import a Linux image as a managed immutable base:

```bash
cargo run -- base import --name ubuntu-24.04 --image /path/to/ubuntu.img
```

Capture a configured instance root disk as a new immutable base:

```bash
cargo run -- base capture \
  --name ubuntu-dev \
  --repo git@github.com:org/repo.git \
  --branch main
```

Or capture a base-only instance:

```bash
cargo run -- base capture --name ubuntu-dev --instance tools-box
```

List imported bases:

```bash
cargo run -- base list
```

Imported bases live under:

```text
$VIBEBOX_HOME/base-images/<base-id>/
```

Each base is stored as `base.img` and marked read-only. On macOS, `vibebox` requires APFS `clonefile(2)` semantics when creating base and instance images and will fail instead of falling back to a full copy. That keeps captures and instance creation copy-on-write, but it means the source and destination need to be on the same APFS volume.

## Branch Model

Each `repo + branch` pair is mapped to a stable instance directory under:

```text
$VIBEBOX_HOME/instances/<instance-id>/
```

By default, `VIBEBOX_HOME` resolves to:

```text
~/.local/state/vibebox
```

Inside an instance:

```text
checkout/       persistent repo working tree
vm/branch.img   persistent writable branch disk cloned from the base image
instance.env    persisted instance metadata, including base image and port mappings
```

That means yes: the branch has its own local filesystem block. The `branch.img` file persists until you destroy the instance, but it starts life as a clone of the imported base image rather than as an empty file.
`vibebox` will expand the branch disk file beyond the base image size when needed, and cloud-init plus the guest shell entry path will attempt to grow the root partition/filesystem automatically.

## Usage

Import a base image first:

```bash
cargo run -- base import --name ubuntu-24.04 --image /path/to/ubuntu.img
```

Prepare and enter an existing branch:

```bash
cargo run -- launch --repo git@github.com:org/repo.git --branch main --base ubuntu-24.04
```

If you provide `--repo` without `--branch`, `vibebox` will fetch a short list of recent remote branches and prompt you to choose one interactively.

Boot a base-only VM with an empty persistent workspace:

```bash
cargo run -- launch --base ubuntu-24.04
```

Boot a named standalone VM:

```bash
cargo run -- launch --name tools-box --base ubuntu-24.04
```

If you omit `--base` for a new launch, `vibebox` will pick the newest imported base image automatically.

Create a new branch from the remote default branch:

```bash
cargo run -- launch --repo git@github.com:org/repo.git --branch feature/x --base ubuntu-24.04 --new-branch
```

Create a new branch from a specific base branch:

```bash
cargo run -- launch --repo git@github.com:org/repo.git --branch feature/x --base ubuntu-24.04 --new-branch --from develop
```

Print the prepared workspace without entering a shell:

```bash
cargo run -- launch --repo git@github.com:org/repo.git --branch main --base ubuntu-24.04 --no-enter
```

By default, `vibebox` keeps launch output quiet and shows a single in-place `Loading` pulse while the VM becomes reachable. If you want the full instance summary and bootstrap chatter, add `--verbose`.

Tune VM size explicitly:

```bash
cargo run -- launch --repo git@github.com:org/repo.git --branch main --base ubuntu-24.04 --cpus 6 --memory-mib 12288
```

Set the guest login user, hostname, and SSH key explicitly:

```bash
cargo run -- launch \
  --repo git@github.com:org/repo.git \
  --branch main \
  --base ubuntu-24.04 \
  --cloud-user josh \
  --hostname repo-main \
  --ssh-pubkey ~/.ssh/id_ed25519.pub \
  --ssh-private-key ~/.ssh/id_ed25519
```

Run a first-boot bootstrap script inside the guest user account:

```bash
cargo run -- launch \
  --repo git@github.com:org/repo.git \
  --branch main \
  --base ubuntu-24.04 \
  --cloud-user josh \
  --init-script ./scripts/bootstrap-vm.sh
```

The init script is copied into cloud-init seed data, runs once per instance as the guest user, and can use `sudo` for system package installs. Output is written inside the guest to `/var/log/vibebox-init.log`.

Share extra host directories into the guest:

```bash
cargo run -- launch \
  --repo git@github.com:org/repo.git \
  --branch main \
  --base ubuntu-24.04 \
  --share ~/Downloads:/mnt/downloads \
  --share ~/src/shared-assets:/mnt/assets
```

Map your local Codex and Claude config directories into the guest home:

```bash
cargo run -- --base ubuntu-24.04 --with-ai
```

Or enable them individually:

```bash
cargo run -- --base ubuntu-24.04 --with-codex
cargo run -- --base ubuntu-24.04 --with-claude
cargo run -- --base ubuntu-24.04 --with-gh
```

That maps `~/.codex` to `/home/<guest-user>/.codex` and `~/.claude` to `/home/<guest-user>/.claude`. These mounts persist like any other share, so you typically only need to pass the flag once per instance. This shares the full config directories, not just auth tokens.

If `ANTHROPIC_API_KEY` is set in the host environment, `--with-claude` also exports it into the guest shell as `ANTHROPIC_API_KEY` on connect. Claude Pro/web-login state still does not cross from macOS into the Linux guest.

If the host `gh` CLI is authenticated, `--with-gh` exports its `gh auth token` value into the guest as `GH_TOKEN`. If `~/.config/gh` exists on the host, that directory is also shared into the guest.

The checkout is always shared at `/workspace`. Extra `--share` entries are persisted with the instance, so a plain later `launch` reuses them. If you change the share set for a running VM, `vibebox` will restart that VM on relaunch to apply the new mounts. Remove all saved extra shares with:

```bash
cargo run -- launch --repo git@github.com:org/repo.git --branch main --clear-shares
```

Inspect or remove a branch instance:

```bash
cargo run -- status --repo git@github.com:org/repo.git --branch main
cargo run -- stop --repo git@github.com:org/repo.git --branch main
cargo run -- destroy --repo git@github.com:org/repo.git --branch main
```

`stop` shuts down the VM but keeps the instance checkout and root filesystem. `destroy` removes the instance entirely.
Pass `--yes` to `destroy` if you need to skip the confirmation prompt.

Inspect or remove a base-only instance:

```bash
cargo run -- status --name tools-box
cargo run -- destroy --name tools-box
```

List all instances and whether they are currently running:

```bash
cargo run -- list
```

## Service Access

Services in the guest should be reached through the guest IP or the guest `.local` hostname, for example:

- `http://police-station-main.local:3000`
- `http://police-station-main.local:5173`
- `ssh joshv@police-station-main.local`

`vibebox` does not create `localhost` port forwards by default.

## Built-In VM Runtime

If `krunkit` and `vmnet-helper` are installed, `vibebox launch` will boot the guest, wait for SSH, and connect automatically.

The built-in runtime launches:

- `krunkit` with the branch disk attached as `virtio-blk`
- `vmnet-client` to provide macOS shared networking
- a per-instance cloud-init `CIDATA` seed ISO as a second `virtio-blk` device
- the host checkout as a `virtio-fs` share with mount tag `workspace`
- a guest console log at `runtime/console.log`
- SSH as the shell/control plane
- SSH `-L` tunnels for deterministic branch-local host ports

Cloud-init is enabled by default for VM launches. `vibebox` will:

- auto-detect `~/.ssh/id_ed25519.pub`, `id_ecdsa.pub`, `id_rsa.pub`, or `authorized_keys`
- create a sudo-capable guest user, defaulting to your current host username
- set the guest hostname from the instance id unless you override it
- write a `network-config` file that assigns a deterministic static guest IP
- auto-mount the `virtio-fs` share at `/workspace`
- auto-mount any extra `--share` `virtio-fs` directories you configured
- create `~/workspace` as a symlink to `/workspace`
- install and start `avahi-daemon` so the guest hostname is advertised over mDNS as `<hostname>.local`
- write `cloud-init/files/user-data` and `cloud-init/files/meta-data`
- build `cloud-init/seed.iso` with volume label `CIDATA`

Disable seed generation with `--no-cloud-init` if your base image is already preconfigured. Use `--shell` if you explicitly want a local shell instead of a VM; VM launch is the default.

Inside the guest, the repo should already be available at `/workspace`, and the cloud-init user should also have `~/workspace` pointing at it. `vibebox` should SSH there automatically once the guest is up.

On macOS, the built-in VM path plus `avahi-daemon` means you can typically open guest-hosted dev servers in a browser via the guest hostname, for example `http://repo-main.local:3000`, as long as the service is listening on the guest network interface.

Your base image therefore needs to be:

- Linux
- EFI bootable
- compatible with virtio block and virtio-fs
- running `sshd`

For built-in networking on macOS, install `vmnet-helper`:

- repo: https://github.com/nirs/vmnet-helper
- install script from the README: `curl -fsSL https://raw.githubusercontent.com/nirs/vmnet-helper/main/install.sh | sudo bash`

## External Handoff

Set `VIBEBOX_VM_LAUNCHER` to an executable that knows how to translate the instance metadata into your local libkrun invocation.

The launcher receives:

- `VIBEBOX_INSTANCE`
- `VIBEBOX_REPO`
- `VIBEBOX_BRANCH`
- `VIBEBOX_CHECKOUT`
- `VIBEBOX_BASE_IMAGE`
- `VIBEBOX_BASE_IMAGE_ID`
- `VIBEBOX_ROOTFS`
- `VIBEBOX_ROOTFS_MB`
- `VIBEBOX_CPUS`
- `VIBEBOX_MEMORY_MIB`
- `VIBEBOX_CLOUD_INIT_IMAGE`
- `VIBEBOX_CLOUD_INIT_USER`
- `VIBEBOX_HOSTNAME`
- `VIBEBOX_GUEST_IP`
- `VIBEBOX_GUEST_GATEWAY`
- `VIBEBOX_GUEST_MAC`
- `VIBEBOX_INTERFACE_ID`
- `VIBEBOX_SSH_PRIVATE_KEY`
- `VIBEBOX_PORTS` as `host:guest,host:guest,...`

If neither `VIBEBOX_VM_LAUNCHER` nor the built-in `krunkit` + `vmnet-helper` path is available, `vibebox launch` will fail unless you explicitly pass `--shell`.
