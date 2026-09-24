#!/bin/sh
# dbx installer — dipakai oleh satu baris ini:
#
#   curl -fsSL https://raw.githubusercontent.com/alifkhasan21-droid/ryome-db/main/install.sh | sh
#
# Urutan: unduh binary dari GitHub Release → kalau belum ada, bangun dari
# source pakai cargo. Lokasi tujuan bisa diubah lewat DBX_INSTALL_DIR.
set -eu

REPO="alifkhasan21-droid/ryome-db"
BIN="dbx"
INSTALL_DIR="${DBX_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

# ── 1. deteksi platform ────────────────────────────────────────────────
os=$(uname -s)
arch=$(uname -m)

case "$arch" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) fail "arsitektur '$arch' belum didukung" ;;
esac

case "$os-$arch" in
  Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64) target=aarch64-unknown-linux-gnu ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Darwin-aarch64) target=aarch64-apple-darwin ;;
  *) fail "platform '$os/$arch' belum didukung (baru Linux & macOS)" ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# ── 2. ambil binary dari GitHub Release ────────────────────────────────
url="https://github.com/$REPO/releases/latest/download/$BIN-$target"
have_release=1

say "· mengunduh $url"
if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$url" -o "$tmp/$BIN" || have_release=0
elif command -v wget >/dev/null 2>&1; then
  wget -qO "$tmp/$BIN" "$url" || have_release=0
else
  fail "butuh curl atau wget"
fi

if [ "$have_release" -eq 1 ]; then
  chmod +x "$tmp/$BIN"
  # jangan pasang binary yang tidak bisa dijalankan
  "$tmp/$BIN" --version >/dev/null 2>&1 ||
    fail "binary hasil unduhan tidak bisa dijalankan di sistem ini"
  mkdir -p "$INSTALL_DIR"
  cp "$tmp/$BIN" "$INSTALL_DIR/$BIN"
  chmod 755 "$INSTALL_DIR/$BIN"
  say "✓ terpasang: $INSTALL_DIR/$BIN"
else
  # ── 3. fallback: bangun dari source ──────────────────────────────────
  say "! rilis binary untuk $target belum tersedia — membangun dari source"
  command -v cargo >/dev/null 2>&1 ||
    fail "cargo tidak ditemukan. Install Rust dulu: https://rustup.rs"
  say "  (butuh beberapa menit)"
  # --root ~/.local → binary masuk ~/.local/bin, sama dengan jalur di atas
  cargo install --git "https://github.com/$REPO.git" --locked "$BIN" \
    --root "$HOME/.local"
fi

# ── 4. cek PATH ────────────────────────────────────────────────────────
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) say "! tambahkan ke PATH:  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac

say "· selesai — jalankan: dbx doctor"
