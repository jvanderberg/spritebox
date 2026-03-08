#!/usr/bin/env bash
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive

log() {
  printf '[bootstrap] %s\n' "$*"
}

apt_packages=(
  build-essential
  ca-certificates
  curl
  gcc
  gh
  git
  pkg-config
  libssl-dev
  libsqlite3-dev
  make
  unzip
  zip
  python3-pip
  python3-venv
  python3-dev
  pipx
)

sudo apt-get update
sudo apt-get install -y "${apt_packages[@]}"

verify_node_tool() {
  local cmd=$1
  local resolved output version_line

  resolved=$(type -P "$cmd" || true)
  if [[ -z "$resolved" ]]; then
    printf '[bootstrap] ERROR: %s is not on PATH\n' "$cmd" >&2
    return 1
  fi
  resolved=$(readlink -f "$resolved")
  if [[ -z "$resolved" || ! -s "$resolved" ]]; then
    printf '[bootstrap] ERROR: %s resolved to an invalid or empty file: %s\n' "$cmd" "$resolved" >&2
    return 1
  fi

  output=$(command "$cmd" --version 2>&1) || {
    printf '[bootstrap] ERROR: %s --version failed\n' "$cmd" >&2
    return 1
  }
  if [[ -z "$output" ]]; then
    printf '[bootstrap] ERROR: %s --version produced no output\n' "$cmd" >&2
    return 1
  fi

  version_line=$(printf '%s\n' "$output" | awk 'NF { line=$0 } END { print line }')
  if [[ -z "$version_line" ]]; then
    printf '[bootstrap] ERROR: %s --version did not produce a usable version line\n' "$cmd" >&2
    return 1
  fi

  log "$cmd ok: $version_line"
}

latest_npm_log() {
  ls -1t "$HOME"/.npm/_logs/*-debug-0.log 2>/dev/null | head -n1 || true
}

print_npm_failure_context() {
  local log_file=$1
  printf '[bootstrap] ERROR: npm install failed\n' >&2
  if [[ -f "$log_file" ]]; then
    printf '[bootstrap] npm install log: %s\n' "$log_file" >&2
    tail -n 80 "$log_file" >&2 || true
  fi

  local debug_log
  debug_log=$(latest_npm_log)
  if [[ -n "$debug_log" && -f "$debug_log" && "$debug_log" != "$log_file" ]]; then
    printf '[bootstrap] latest npm debug log: %s\n' "$debug_log" >&2
    tail -n 80 "$debug_log" >&2 || true
  fi
}

install_node_clis() {
  local attempt=$1
  local log_file
  log_file=$(mktemp "${TMPDIR:-/tmp}/yolobox-npm-install.XXXXXX.log")
  log "Installing global Node CLIs (attempt $attempt)"
  if ! npm install -g vite create-vite @openai/codex @anthropic-ai/claude-code >"$log_file" 2>&1; then
    print_npm_failure_context "$log_file"
    return 1
  fi
  log "Global Node CLIs installed (attempt $attempt)"
}

repair_npm_cache() {
  log "Clearing npm cache after failed or unhealthy install"
  rm -rf "$HOME/.npm/_cacache"
  npm cache clean --force >/dev/null 2>&1 || true
}

ensure_node_clis() {
  if install_node_clis 1 && verify_node_tool codex && verify_node_tool claude; then
    return 0
  fi

  repair_npm_cache
  install_node_clis 2
  verify_node_tool codex
  verify_node_tool claude
}

if ! command -v rustup >/dev/null 2>&1; then
  curl https://sh.rustup.rs -sSf | sh -s -- -y --profile default
fi

source "$HOME/.cargo/env"
rustup toolchain install stable
rustup default stable
rustup component add rustfmt clippy

export NVM_DIR="$HOME/.nvm"
if [ ! -s "$NVM_DIR/nvm.sh" ]; then
  mkdir -p "$NVM_DIR"
  curl -fsSL https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.3/install.sh | bash
fi

# shellcheck disable=SC1090
source "$NVM_DIR/nvm.sh"
nvm install --lts
nvm alias default 'lts/*'
set +u
nvm use default
set -u

verify_node_tool node
verify_node_tool npm
ensure_node_clis
corepack enable

python3 -m pip install --user --upgrade pip
pipx ensurepath

if ! grep -q 'cargo/env' "$HOME/.bashrc" 2>/dev/null; then
  cat >>"$HOME/.bashrc" <<'EOF'
if [ -f "$HOME/.cargo/env" ]; then
  . "$HOME/.cargo/env"
fi
EOF
fi

if ! grep -q 'nvm use --silent default' "$HOME/.bashrc" 2>/dev/null; then
  cat >>"$HOME/.bashrc" <<'EOF'
export NVM_DIR="$HOME/.nvm"
if [ -s "$NVM_DIR/nvm.sh" ]; then
  . "$NVM_DIR/nvm.sh"
  nvm use --silent default >/dev/null 2>&1 || true
fi
EOF
fi

if ! grep -q 'nvm use --silent default' "$HOME/.profile" 2>/dev/null; then
  cat >>"$HOME/.profile" <<'EOF'
export NVM_DIR="$HOME/.nvm"
if [ -s "$NVM_DIR/nvm.sh" ]; then
  . "$NVM_DIR/nvm.sh"
  nvm use --silent default >/dev/null 2>&1 || true
fi
EOF
fi

cat <<'EOF'
Bootstrap complete.

Installed:
- Rust stable via rustup
- Node LTS via nvm
- npm bundled with the active Node LTS install
- vite and create-vite globally
- OpenAI Codex CLI globally
- Claude Code globally
- GitHub CLI
- python3-pip, python3-venv, pipx
- common native build dependencies
EOF
