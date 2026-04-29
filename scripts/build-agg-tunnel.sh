#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST_DIR="$REPO_ROOT/dist"

MAC_TARGET="aarch64-apple-darwin"
LINUX_TARGET="x86_64-unknown-linux-gnu"

BIN_NAME="agg-tunnel"
PKG_NAME="aggligator-util"

MAC_OUT="$DIST_DIR/${BIN_NAME}-macos-arm64"
LINUX_OUT="$DIST_DIR/${BIN_NAME}-linux-x64"

log() {
  printf "\033[1;34m[build]\033[0m %s\n" "$*"
}

err() {
  printf "\033[1;31m[error]\033[0m %s\n" "$*" >&2
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1
}

ensure_rust() {
  if need_cmd cargo && need_cmd rustup; then
    return
  fi

  err "cargo/rustup 未找到。请先安装 Rust toolchain。"
  err "建议：brew install rustup-init && rustup default stable"
  exit 1
}

ensure_zig() {
  if need_cmd zig; then
    return
  fi

  if need_cmd brew; then
    log "检测到缺少 zig，正在通过 Homebrew 安装..."
    brew install zig
    return
  fi

  err "zig 未安装，且未检测到 Homebrew，无法自动安装。"
  exit 1
}

ensure_cargo_zigbuild() {
  if cargo zigbuild --help >/dev/null 2>&1; then
    return
  fi

  log "检测到缺少 cargo-zigbuild，正在安装..."
  cargo install cargo-zigbuild --locked
}

ensure_target() {
  local target="$1"
  if rustup target list --installed | grep -qx "$target"; then
    return
  fi

  log "安装 Rust target: $target"
  rustup target add "$target"
}

build_mac() {
  log "构建 macOS arm64 二进制..."
  cargo build \
    -p "$PKG_NAME" \
    --bin "$BIN_NAME" \
    --target "$MAC_TARGET" \
    --release \
    --no-default-features

  cp "$REPO_ROOT/target/$MAC_TARGET/release/$BIN_NAME" "$MAC_OUT"
  chmod +x "$MAC_OUT"
}

build_linux() {
  log "交叉构建 Linux x64 二进制..."
  cargo zigbuild \
    -p "$PKG_NAME" \
    --bin "$BIN_NAME" \
    --target "$LINUX_TARGET" \
    --release \
    --no-default-features

  cp "$REPO_ROOT/target/$LINUX_TARGET/release/$BIN_NAME" "$LINUX_OUT"
  chmod +x "$LINUX_OUT"
}

package_artifacts() {
  log "打包产物到 tar.gz..."
  tar -C "$DIST_DIR" -czf "$DIST_DIR/${BIN_NAME}-macos-arm64.tar.gz" "$(basename "$MAC_OUT")"
  tar -C "$DIST_DIR" -czf "$DIST_DIR/${BIN_NAME}-linux-x64.tar.gz" "$(basename "$LINUX_OUT")"
}

main() {
  cd "$REPO_ROOT"
  mkdir -p "$DIST_DIR"

  ensure_rust
  ensure_zig
  ensure_cargo_zigbuild
  ensure_target "$LINUX_TARGET"

  build_mac
  build_linux
  package_artifacts

  log "完成。产物如下："
  ls -lh "$DIST_DIR/${BIN_NAME}-macos-arm64" "$DIST_DIR/${BIN_NAME}-linux-x64" \
         "$DIST_DIR/${BIN_NAME}-macos-arm64.tar.gz" "$DIST_DIR/${BIN_NAME}-linux-x64.tar.gz"
}

main "$@"
