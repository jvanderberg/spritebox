#!/usr/bin/env bash
set -euo pipefail

readonly DEFAULT_REPO_SLUG="jvanderberg/yolobox"
readonly DEFAULT_REPO_REF="main"
readonly DEFAULT_UBUNTU_IMAGE_URL="https://cloud-images.ubuntu.com/jammy/current/jammy-server-cloudimg-arm64.img"
readonly DEFAULT_BASE_NAME="ubuntu"
readonly DEFAULT_DEV_BASE_NAME="ubuntu-dev"
readonly DEFAULT_TEMP_INSTANCE_NAME="setup-ubuntu-dev"
readonly DEFAULT_TEMP_HOSTNAME="setup-ubuntu-dev"
readonly DEFAULT_INSTALL_BIN_DIR="$HOME/.local/bin"
readonly DEFAULT_CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/yolobox/images"
readonly DEFAULT_STATE_HOME="${YOLOBOX_HOME:-$HOME/.local/state/yolobox}"

COMMAND=""
ASSUME_YES=0
PURGE=0
REPLACE_BASE=0
SKIP_VMNET=0
SKIP_IMAGE=0
SKIP_DEV_BASE=0
NO_SSH_KEYGEN=0
BASE_NAME="$DEFAULT_BASE_NAME"
DEV_BASE_NAME="$DEFAULT_DEV_BASE_NAME"
TEMP_INSTANCE_NAME="$DEFAULT_TEMP_INSTANCE_NAME"
TEMP_HOSTNAME="$DEFAULT_TEMP_HOSTNAME"
IMAGE_URL="${YOLOBOX_IMAGE_URL:-$DEFAULT_UBUNTU_IMAGE_URL}"
REPO_SLUG="${YOLOBOX_SETUP_REPO_SLUG:-$DEFAULT_REPO_SLUG}"
REPO_REF="${YOLOBOX_SETUP_REF:-$DEFAULT_REPO_REF}"
INSTALL_BIN_DIR="${YOLOBOX_BIN_DIR:-$DEFAULT_INSTALL_BIN_DIR}"
CACHE_DIR="${YOLOBOX_CACHE_DIR:-$DEFAULT_CACHE_DIR}"
STATE_HOME="$DEFAULT_STATE_HOME"
CLOUD_USER="${YOLOBOX_CLOUD_USER:-${USER:-vibe}}"
SSH_PRIVATE_KEY=""
SSH_PUBLIC_KEY=""
SCRIPT_DIR=""
TEMP_ROOT=""
SOURCE_REPO_DIR=""
DOCTOR_FAILURES=0
ACTIVE_CHILD_PID=""

log() {
  printf '%s\n' "$*"
}

step() {
  printf '\n==> %s\n' "$1" >&2
}

explain() {
  printf '    %s\n' "$1" >&2
}

warn() {
  printf 'warning: %s\n' "$*" >&2
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<EOF
Usage:
  scripts/setup.sh install [options]
  scripts/setup.sh doctor [options]
  scripts/setup.sh uninstall [options]

Commands:
  install      Install host dependencies, build yolobox, import ubuntu, and prepare ubuntu-dev
  doctor       Check host prerequisites, cached images, and imported bases
  uninstall    Remove the installed yolobox binary and optionally purge local state

Options:
  --yes                 Skip confirmation prompts where possible
  --purge               Remove yolobox state and cached images during uninstall
  --replace-base        Re-import the clean base image and rebuild the dev base
  --skip-vmnet          Skip vmnet-helper installation and related checks
  --skip-image          Skip Ubuntu image download/import
  --skip-dev-base       Skip provisioning and capturing the dev base image
  --no-ssh-keygen       Fail instead of generating a new SSH key when none is present
  --base-name NAME      Name for the clean imported base image (default: ${DEFAULT_BASE_NAME})
  --dev-base-name NAME  Name for the prepared dev base image (default: ${DEFAULT_DEV_BASE_NAME})
  --image-url URL       Override the Ubuntu cloud image URL

Environment overrides:
  YOLOBOX_HOME
  YOLOBOX_BIN_DIR
  YOLOBOX_CACHE_DIR
  YOLOBOX_CLOUD_USER
  YOLOBOX_SETUP_REPO_SLUG
  YOLOBOX_SETUP_REF
  YOLOBOX_IMAGE_URL
EOF
}

cleanup() {
  if [[ -n "$TEMP_ROOT" && -d "$TEMP_ROOT" ]]; then
    rm -rf "$TEMP_ROOT"
  fi
}

trap cleanup EXIT

stop_active_child() {
  if [[ -z "$ACTIVE_CHILD_PID" ]]; then
    return 0
  fi

  pkill -TERM -P "$ACTIVE_CHILD_PID" >/dev/null 2>&1 || true
  kill -TERM "$ACTIVE_CHILD_PID" >/dev/null 2>&1 || true
  sleep 1
  pkill -KILL -P "$ACTIVE_CHILD_PID" >/dev/null 2>&1 || true
  kill -KILL "$ACTIVE_CHILD_PID" >/dev/null 2>&1 || true
  ACTIVE_CHILD_PID=""
}

handle_interrupt() {
  warn "interrupt received; stopping active setup work"
  stop_active_child
  destroy_temp_instance_if_present
  exit 130
}

trap handle_interrupt INT TERM

confirm() {
  local prompt=${1:-"Continue?"}
  if [[ "$ASSUME_YES" -eq 1 ]]; then
    return 0
  fi

  local answer
  read -r -p "$prompt [Y/n] " answer
  [[ -z "$answer" || "$answer" =~ ^([yY]|[yY][eE][sS])$ ]]
}

confirm_step() {
  local title=$1
  local reason=$2
  step "$title"
  explain "$reason"
  confirm "Continue?" || die "aborted"
}

parse_args() {
  if [[ $# -lt 1 ]]; then
    usage
    exit 1
  fi

  COMMAND=$1
  shift

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --yes)
        ASSUME_YES=1
        ;;
      --purge)
        PURGE=1
        ;;
      --replace-base)
        REPLACE_BASE=1
        ;;
      --skip-vmnet)
        SKIP_VMNET=1
        ;;
      --skip-image)
        SKIP_IMAGE=1
        ;;
      --skip-dev-base)
        SKIP_DEV_BASE=1
        ;;
      --no-ssh-keygen)
        NO_SSH_KEYGEN=1
        ;;
      --base-name)
        shift
        [[ $# -gt 0 ]] || die "--base-name requires a value"
        BASE_NAME=$1
        ;;
      --dev-base-name)
        shift
        [[ $# -gt 0 ]] || die "--dev-base-name requires a value"
        DEV_BASE_NAME=$1
        ;;
      --image-url)
        shift
        [[ $# -gt 0 ]] || die "--image-url requires a value"
        IMAGE_URL=$1
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        die "unknown argument: $1"
        ;;
    esac
    shift
  done

  case "$COMMAND" in
    install|doctor|uninstall) ;;
    *)
      die "unknown command: $COMMAND"
      ;;
  esac
}

find_script_dir() {
  if [[ -n "${BASH_SOURCE[0]:-}" && -f "${BASH_SOURCE[0]}" ]]; then
    SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
  else
    SCRIPT_DIR=""
  fi
}

have_cmd() {
  command -v "$1" >/dev/null 2>&1
}

slugify() {
  printf '%s' "$1" \
    | tr '[:upper:]' '[:lower:]' \
    | sed -E 's/[^a-z0-9]+/-/g; s/^-+//; s/-+$//'
}

base_dir_for_name() {
  local slug
  slug=$(slugify "$1")
  printf '%s/base-images/%s' "$STATE_HOME" "$slug"
}

check_apfs() {
  local target=$1
  local label=$2
  mkdir -p "$target"
  local fstype
  fstype=$(filesystem_type_for_path "$target")
  [[ "$fstype" == "apfs" ]] || die "$label must be on APFS (found $fstype at $target)"
}

filesystem_type_for_path() {
  local target=$1
  local mount_point
  mount_point=$(df "$target" | tail -n 1 | awk '{print $NF}')
  [[ -n "$mount_point" ]] || die "failed to determine mount point for $target"

  local line
  line=$(mount | grep " on ${mount_point} (" | head -n 1 || true)
  [[ -n "$line" ]] || die "failed to determine filesystem type for $target"

  printf '%s\n' "$line" | sed -E 's/.*\(([^,]+).*/\1/'
}

ensure_macos() {
  [[ "$(uname -s)" == "Darwin" ]] || die "yolobox setup currently supports macOS only"
}

ensure_xcode_clt() {
  if xcode-select -p >/dev/null 2>&1; then
    log "Xcode Command Line Tools already installed"
    return 0
  fi

  confirm_step \
    "Install Xcode Command Line Tools" \
    "Rust builds and several Homebrew formulas need the Apple developer toolchain."
  xcode-select --install >/dev/null 2>&1 || true
  die "finish installing Xcode Command Line Tools, then rerun setup"
}

ensure_homebrew_in_path() {
  if have_cmd brew; then
    return 0
  fi

  if [[ -x /opt/homebrew/bin/brew ]]; then
    eval "$(/opt/homebrew/bin/brew shellenv)"
    return 0
  fi

  if [[ -x /usr/local/bin/brew ]]; then
    eval "$(/usr/local/bin/brew shellenv)"
    return 0
  fi
}

ensure_homebrew() {
  ensure_homebrew_in_path
  if have_cmd brew; then
    log "Homebrew already installed"
    return 0
  fi

  confirm_step \
    "Install Homebrew" \
    "yolobox uses Homebrew to install krunkit, qemu, and other host dependencies."
  NONINTERACTIVE=1 /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
  ensure_homebrew_in_path
  have_cmd brew || die "Homebrew installation completed but brew is still not available"
}

brew_install_if_missing() {
  local formula=$1
  if brew list --versions "$formula" >/dev/null 2>&1; then
    log "Homebrew formula already installed: $formula"
    return 0
  fi
  confirm_step \
    "Install Homebrew formula: $formula" \
    "This dependency is required by the built-in VM runtime or the base image preparation flow."
  HOMEBREW_NO_INSTALL_CLEANUP=1 HOMEBREW_NO_ENV_HINTS=1 brew install "$formula"
}

ensure_rustup() {
  if have_cmd cargo; then
    log "Rust toolchain already installed"
    return 0
  fi

  confirm_step \
    "Install Rust toolchain" \
    "The setup script builds yolobox from source before installing the binary."
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
  # shellcheck disable=SC1090
  source "$HOME/.cargo/env"
}

ensure_cargo_in_path() {
  if have_cmd cargo; then
    return 0
  fi
  if [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck disable=SC1090
    source "$HOME/.cargo/env"
  fi
  have_cmd cargo || die "cargo is not available after Rust installation"
}

ensure_vmnet_helper() {
  if [[ "$SKIP_VMNET" -eq 1 ]]; then
    log "Skipping vmnet-helper installation"
    return 0
  fi

  if [[ -x /opt/vmnet-helper/bin/vmnet-client ]]; then
    log "vmnet-helper already installed"
    return 0
  fi

  confirm_step \
    "Install vmnet-helper" \
    "This provides host networking so guests get a stable IP address and .local hostname."
  curl -fsSL https://raw.githubusercontent.com/nirs/vmnet-helper/main/install.sh | sudo bash
  [[ -x /opt/vmnet-helper/bin/vmnet-client ]] || die "vmnet-helper install completed but vmnet-client is still missing"
}

discover_ssh_keys() {
  local key
  for key in "$HOME/.ssh/id_ed25519" "$HOME/.ssh/id_ecdsa" "$HOME/.ssh/id_rsa"; do
    if [[ -f "$key" && -f "$key.pub" ]]; then
      SSH_PRIVATE_KEY=$key
      SSH_PUBLIC_KEY=$key.pub
      return 0
    fi
  done
  return 1
}

ensure_ssh_keys() {
  if discover_ssh_keys; then
    log "SSH key already present: $SSH_PUBLIC_KEY"
    return 0
  fi

  [[ "$NO_SSH_KEYGEN" -eq 0 ]] || die "no SSH key found in ~/.ssh and --no-ssh-keygen was set"

  mkdir -p "$HOME/.ssh"
  chmod 700 "$HOME/.ssh"
  confirm_step \
    "Generate SSH key" \
    "cloud-init injects this key so yolobox can log into new guests automatically."
  ssh-keygen -t ed25519 -f "$HOME/.ssh/id_ed25519" -N ""
  discover_ssh_keys || die "SSH key generation completed but no usable key was found"
}

maybe_local_repo_dir() {
  if [[ -f "$PWD/Cargo.toml" && -f "$PWD/scripts/bootstrap-vm.sh" ]]; then
    printf '%s\n' "$PWD"
    return 0
  fi

  if [[ -n "$SCRIPT_DIR" ]]; then
    local candidate
    candidate=$(cd "$SCRIPT_DIR/.." && pwd)
    if [[ -f "$candidate/Cargo.toml" && -f "$candidate/scripts/bootstrap-vm.sh" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  fi

  return 1
}

download_source_repo() {
  local archive_url=$1
  TEMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/yolobox-setup.XXXXXX")
  curl -fsSL "$archive_url" | tar -xzf - -C "$TEMP_ROOT"

  local extracted_dir
  extracted_dir=$(find "$TEMP_ROOT" -mindepth 1 -maxdepth 1 -type d | head -n 1)
  [[ -n "$extracted_dir" ]] || die "failed to extract yolobox source archive"
  printf '%s\n' "$extracted_dir"
}

resolve_source_repo_dir() {
  local repo_dir
  if repo_dir=$(maybe_local_repo_dir); then
    SOURCE_REPO_DIR=$repo_dir
    return 0
  fi

  local archive_url="https://github.com/${REPO_SLUG}/archive/refs/heads/${REPO_REF}.tar.gz"
  log "Downloading yolobox source from ${REPO_SLUG}@${REPO_REF}..."
  SOURCE_REPO_DIR=$(download_source_repo "$archive_url")
}

build_and_install_yolobox() {
  ensure_cargo_in_path
  resolve_source_repo_dir

  confirm_step \
    "Build and install yolobox" \
    "This compiles the current yolobox source and installs the binary into your local bin directory."
  (
    cd "$SOURCE_REPO_DIR"
    cargo build --release --locked
  )

  install -d "$INSTALL_BIN_DIR"
  install -m 0755 "$SOURCE_REPO_DIR/target/release/yolobox" "$INSTALL_BIN_DIR/yolobox"
  export PATH="$INSTALL_BIN_DIR:$PATH"
}

ubuntu_download_path() {
  local filename
  filename=$(basename "$IMAGE_URL")
  printf '%s/%s' "$CACHE_DIR" "$filename"
}

ubuntu_raw_path() {
  local download_path
  download_path=$(ubuntu_download_path)
  local filename
  filename=$(basename "$download_path")
  filename=${filename%.*}
  printf '%s/%s.raw' "$CACHE_DIR" "$filename"
}

download_ubuntu_image() {
  install -d "$CACHE_DIR"
  local download_path
  download_path=$(ubuntu_download_path)

  if [[ -f "$download_path" ]]; then
    log "Ubuntu image already cached: $download_path" >&2
    printf '%s\n' "$download_path"
    return 0
  fi

  confirm_step \
    "Download Ubuntu cloud image" \
    "yolobox needs a cloud image as the clean base disk before it can create branch VMs."
  curl -fL "$IMAGE_URL" -o "$download_path"
  printf '%s\n' "$download_path"
}

determine_image_format() {
  local image_path=$1
  qemu-img info "$image_path" 2>/dev/null | awk -F': ' '/file format/ {print $2; exit}'
}

prepare_raw_image() {
  local source_image=$1
  local image_format
  image_format=$(determine_image_format "$source_image")
  [[ -n "$image_format" ]] || die "failed to determine image format for $source_image"

  if [[ "$image_format" == "raw" ]]; then
    printf '%s\n' "$source_image"
    return 0
  fi

  local raw_path
  raw_path=$(ubuntu_raw_path)
  if [[ -f "$raw_path" ]]; then
    log "Raw Ubuntu image already prepared: $raw_path" >&2
    printf '%s\n' "$raw_path"
    return 0
  fi

  confirm_step \
    "Convert Ubuntu image to raw" \
    "The built-in runtime imports raw disk images for APFS clonefile-based base image storage."
  qemu-img convert -f "$image_format" -O raw "$source_image" "$raw_path"
  printf '%s\n' "$raw_path"
}

base_exists() {
  [[ -f "$(base_dir_for_name "$1")/base.env" ]]
}

remove_base_if_requested() {
  local name=$1
  local base_dir
  base_dir=$(base_dir_for_name "$name")

  if [[ ! -d "$base_dir" ]]; then
    return 0
  fi

  if [[ "$REPLACE_BASE" -eq 1 ]]; then
    rm -rf "$base_dir"
    return 0
  fi
}

import_clean_base() {
  [[ "$SKIP_IMAGE" -eq 0 ]] || return 0

  if base_exists "$BASE_NAME"; then
    if [[ "$REPLACE_BASE" -eq 1 ]]; then
      confirm_step \
        "Replace base image: $BASE_NAME" \
        "You requested a fresh import, so the existing clean base will be replaced."
      log "Replacing base image: $BASE_NAME"
      rm -rf "$(base_dir_for_name "$BASE_NAME")"
    else
      log "Base image already imported: $BASE_NAME"
      return 0
    fi
  fi

  local source_image
  source_image=$(download_ubuntu_image)
  local raw_image
  raw_image=$(prepare_raw_image "$source_image")

  confirm_step \
    "Import base image: $BASE_NAME" \
    "This registers the clean Ubuntu image in yolobox state so new instances can clone it."
  "$INSTALL_BIN_DIR/yolobox" base import --name "$BASE_NAME" --image "$raw_image"
}

destroy_temp_instance_if_present() {
  if "$INSTALL_BIN_DIR/yolobox" status --name "$TEMP_INSTANCE_NAME" >/dev/null 2>&1; then
    "$INSTALL_BIN_DIR/yolobox" destroy --name "$TEMP_INSTANCE_NAME" --yes >/dev/null 2>&1 || true
  fi
}

verify_guest_tools() {
  cat <<'EOF'
set -euo pipefail
for cmd in rustc cargo node npm codex claude gh python3 pipx; do
  command -v "$cmd" >/dev/null 2>&1
done
exit
EOF
}

run_provision_launch() {
  local verify_script
  verify_script=$(mktemp "${TMPDIR:-/tmp}/yolobox-verify.XXXXXX")
  verify_guest_tools > "$verify_script"

  "$INSTALL_BIN_DIR/yolobox" \
    --name "$TEMP_INSTANCE_NAME" \
    --base "$BASE_NAME" \
    --hostname "$TEMP_HOSTNAME" \
    --user "$CLOUD_USER" \
    --verbose \
    --init-script "$SOURCE_REPO_DIR/scripts/bootstrap-vm.sh" \
    < "$verify_script" &
  ACTIVE_CHILD_PID=$!

  local status=0
  wait "$ACTIVE_CHILD_PID" || status=$?
  ACTIVE_CHILD_PID=""
  rm -f "$verify_script"
  return "$status"
}

prepare_dev_base() {
  [[ "$SKIP_DEV_BASE" -eq 0 ]] || return 0
  base_exists "$BASE_NAME" || die "clean base $BASE_NAME is missing; import it first or remove --skip-image"

  if base_exists "$DEV_BASE_NAME"; then
    if [[ "$REPLACE_BASE" -eq 1 ]]; then
      confirm_step \
        "Replace dev base image: $DEV_BASE_NAME" \
        "You requested a fresh provisioning pass, so the existing prepared base will be replaced."
      log "Replacing dev base image: $DEV_BASE_NAME"
      rm -rf "$(base_dir_for_name "$DEV_BASE_NAME")"
    else
      log "Dev base already captured: $DEV_BASE_NAME"
      return 0
    fi
  fi

  destroy_temp_instance_if_present

  confirm_step \
    "Provision dev base instance: $TEMP_INSTANCE_NAME" \
    "This boots a temporary VM, runs scripts/bootstrap-vm.sh inside it, verifies tools, then captures ubuntu-dev."
  explain "You should see SSH wait progress and cloud-init/bootstrap output after this point."
  run_provision_launch

  step "Capture dev base image: $DEV_BASE_NAME"
  explain "This snapshots the prepared temporary VM into a reusable immutable base image."
  confirm "Continue?" || die "aborted"
  "$INSTALL_BIN_DIR/yolobox" base capture --name "$DEV_BASE_NAME" --instance "$TEMP_INSTANCE_NAME"

  step "Clean up temporary provisioning instance"
  explain "The temporary bootstrap VM is no longer needed after the dev base is captured."
  confirm "Continue?" || die "aborted"
  "$INSTALL_BIN_DIR/yolobox" destroy --name "$TEMP_INSTANCE_NAME" --yes >/dev/null 2>&1 || true
}

check_line() {
  local label=$1
  local status=$2
  printf '%-24s %s\n' "$label" "$status"
}

record_doctor_check() {
  local label=$1
  local required=$2
  local status=$3

  check_line "$label" "$status"
  if [[ "$required" -eq 1 && "$status" == "missing" ]]; then
    DOCTOR_FAILURES=$((DOCTOR_FAILURES + 1))
  fi
}

path_has_install_bin() {
  case ":$PATH:" in
    *":$INSTALL_BIN_DIR:"*) return 0 ;;
    *) return 1 ;;
  esac
}

doctor() {
  DOCTOR_FAILURES=0

  ensure_homebrew_in_path || true
  ensure_cargo_in_path || true
  discover_ssh_keys || true

  if [[ "$(uname -s)" == "Darwin" ]]; then
    record_doctor_check "macOS" 1 "ok"
  else
    record_doctor_check "macOS" 1 "missing"
  fi

  if have_cmd brew; then
    record_doctor_check "Homebrew" 1 "ok"
  else
    record_doctor_check "Homebrew" 1 "missing"
  fi

  if have_cmd cargo; then
    record_doctor_check "cargo" 1 "ok"
  else
    record_doctor_check "cargo" 1 "missing"
  fi

  if have_cmd krunkit; then
    record_doctor_check "krunkit" 1 "ok"
  else
    record_doctor_check "krunkit" 1 "missing"
  fi

  if have_cmd qemu-img; then
    record_doctor_check "qemu-img" 1 "ok"
  else
    record_doctor_check "qemu-img" 1 "missing"
  fi

  if [[ "$SKIP_VMNET" -eq 1 ]]; then
    record_doctor_check "vmnet-client" 0 "skipped"
  elif [[ -x /opt/vmnet-helper/bin/vmnet-client ]]; then
    record_doctor_check "vmnet-client" 1 "ok"
  else
    record_doctor_check "vmnet-client" 1 "missing"
  fi

  if [[ -n "$SSH_PRIVATE_KEY" ]]; then
    record_doctor_check "SSH key" 1 "ok"
  else
    record_doctor_check "SSH key" 1 "missing"
  fi

  if [[ -x "$INSTALL_BIN_DIR/yolobox" ]]; then
    record_doctor_check "yolobox binary" 1 "ok"
  else
    record_doctor_check "yolobox binary" 1 "missing"
  fi

  if path_has_install_bin; then
    record_doctor_check "PATH" 0 "ok"
  else
    record_doctor_check "PATH" 0 "missing"
  fi

  if [[ -f "$(ubuntu_download_path)" || -f "$(ubuntu_raw_path)" ]]; then
    record_doctor_check "Ubuntu image cache" 1 "ok"
  else
    record_doctor_check "Ubuntu image cache" 1 "missing"
  fi

  if base_exists "$BASE_NAME"; then
    record_doctor_check "$BASE_NAME base" 1 "ok"
  else
    record_doctor_check "$BASE_NAME base" 1 "missing"
  fi

  if base_exists "$DEV_BASE_NAME"; then
    record_doctor_check "$DEV_BASE_NAME base" 1 "ok"
  else
    record_doctor_check "$DEV_BASE_NAME base" 1 "missing"
  fi

  if [[ -d "$STATE_HOME" ]]; then
    local fstype
    fstype=$(filesystem_type_for_path "$STATE_HOME")
    check_line "YOLOBOX_HOME fs" "$fstype"
    [[ "$fstype" == "apfs" ]] || DOCTOR_FAILURES=$((DOCTOR_FAILURES + 1))
  else
    record_doctor_check "YOLOBOX_HOME fs" 1 "missing"
  fi

  if [[ -x "$INSTALL_BIN_DIR/yolobox" ]]; then
    "$INSTALL_BIN_DIR/yolobox" doctor
  fi

  return "$DOCTOR_FAILURES"
}

install_all() {
  step "Prepare host environment"
  explain "This checks the macOS host, developer tools, package manager, and SSH prerequisites before any VM work starts."
  confirm "Continue?" || die "aborted"
  ensure_macos
  ensure_xcode_clt
  ensure_homebrew
  HOMEBREW_NO_ENV_HINTS=1 brew tap slp/krunkit
  brew_install_if_missing krunkit
  brew_install_if_missing qemu
  ensure_vmnet_helper
  ensure_rustup
  ensure_cargo_in_path
  ensure_ssh_keys

  install -d "$INSTALL_BIN_DIR"
  install -d "$CACHE_DIR"
  install -d "$STATE_HOME"
  check_apfs "$STATE_HOME" "YOLOBOX_HOME"
  check_apfs "$CACHE_DIR" "YOLOBOX cache"

  build_and_install_yolobox
  import_clean_base
  prepare_dev_base

  log
  log "Validation:"
  doctor
  log
  log "First launch:"
  log "  yolobox --repo git@github.com:org/repo.git --branch main --base $DEV_BASE_NAME"
  if ! path_has_install_bin; then
    warn "$INSTALL_BIN_DIR is not on PATH in this shell; add it before using yolobox directly"
  fi
}

uninstall_all() {
  ensure_homebrew_in_path || true

  if [[ -e "$INSTALL_BIN_DIR/yolobox" ]]; then
    confirm_step \
      "Remove yolobox binary" \
      "This removes the installed yolobox binary from $INSTALL_BIN_DIR."
    rm -f "$INSTALL_BIN_DIR/yolobox"
  else
    log "yolobox binary already absent: $INSTALL_BIN_DIR/yolobox"
  fi

  if have_cmd brew && brew list --versions krunkit >/dev/null 2>&1; then
    confirm_step \
      "Uninstall krunkit" \
      "This removes the built-in VM runtime installed by the setup script."
    brew uninstall --formula krunkit >/dev/null 2>&1 || true
  fi

  if have_cmd brew && brew list --versions qemu >/dev/null 2>&1; then
    confirm_step \
      "Uninstall qemu" \
      "This removes qemu and qemu-img used for image conversion."
    brew uninstall --formula qemu >/dev/null 2>&1 || true
  fi

  if [[ -x /opt/vmnet-helper/uninstall.sh ]]; then
    confirm_step \
      "Uninstall vmnet-helper" \
      "This removes the host networking helper used to give guests a stable IP and .local hostname."
    sudo /opt/vmnet-helper/uninstall.sh || true
  elif [[ -x /opt/vmnet-helper/bin/vmnet-client ]]; then
    warn "vmnet-helper is still installed; remove it manually if you want a full cleanup"
  fi

  if [[ "$PURGE" -eq 1 ]]; then
    confirm_step \
      "Purge yolobox state and cache" \
      "This removes yolobox state at $STATE_HOME and cached images at $CACHE_DIR."
    rm -rf "$STATE_HOME" "$CACHE_DIR"
  fi

  if have_cmd brew && ! brew list --versions krunkit >/dev/null 2>&1; then
    brew untap slp/krunkit >/dev/null 2>&1 || true
  fi

  log "yolobox uninstall complete"
}

main() {
  parse_args "$@"
  find_script_dir

  case "$COMMAND" in
    install)
      install_all
      ;;
    doctor)
      doctor
      ;;
    uninstall)
      uninstall_all
      ;;
  esac
}

main "$@"
