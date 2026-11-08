#!/bin/sh
# Install the latest acpc release.
#
#   curl -fsSL https://raw.githubusercontent.com/AkaraChen/acpc/main/install.sh | sh
#
# Environment:
#   ACPC_VERSION       release to install, e.g. v0.2.0 (default: latest)
#   ACPC_INSTALL_DIR   where to put the binary (default: ~/.local/bin)
set -eu

repo="AkaraChen/acpc"
install_dir="${ACPC_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf 'acpc-install: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }

command -v curl >/dev/null || die "curl is required"
command -v tar >/dev/null || die "tar is required"

os="$(uname -s)"
arch="$(uname -m)"
case "$arch" in
  x86_64 | amd64) arch="x86_64" ;;
  arm64 | aarch64) arch="aarch64" ;;
  *) die "unsupported architecture: $arch" ;;
esac
case "$os" in
  Linux) target="$arch-unknown-linux-musl" ;;
  Darwin)
    # Rosetta shells report x86_64; prefer the native build.
    if [ "$arch" = "x86_64" ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = "1" ]; then
      arch="aarch64"
    fi
    target="$arch-apple-darwin"
    ;;
  *) die "unsupported OS: $os (on Windows, use install.ps1)" ;;
esac

version="${ACPC_VERSION:-}"
if [ -z "$version" ]; then
  # The latest-release page redirects to .../releases/tag/<version>.
  url="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest")" \
    || die "could not resolve the latest release"
  version="${url##*/}"
  case "$version" in
    v*) ;;
    *) die "no published release found" ;;
  esac
fi
case "$version" in v*) ;; *) version="v$version" ;; esac

archive="acpc-$target.tar.gz"
base="https://github.com/$repo/releases/download/$version"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "downloading acpc $version for $target"
curl -fsSL "$base/$archive" -o "$tmp/$archive" || die "download failed: $base/$archive"
curl -fsSL "$base/$archive.sha256" -o "$tmp/$archive.sha256" || die "download failed: $archive.sha256"

expected="$(cut -d ' ' -f 1 < "$tmp/$archive.sha256")"
if command -v sha256sum >/dev/null; then
  actual="$(sha256sum "$tmp/$archive" | cut -d ' ' -f 1)"
else
  actual="$(shasum -a 256 "$tmp/$archive" | cut -d ' ' -f 1)"
fi
[ "$expected" = "$actual" ] || die "checksum mismatch for $archive"

tar -xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$install_dir"
# Install through a temp name so a running acpc is replaced, not overwritten.
cp "$tmp/acpc" "$install_dir/.acpc.new"
chmod 755 "$install_dir/.acpc.new"
mv -f "$install_dir/.acpc.new" "$install_dir/acpc"
say "installed $install_dir/acpc ($version)"

case ":$PATH:" in
  *":$install_dir:"*) ;;
  *) say "$install_dir is not on your PATH; add it, e.g.: export PATH=\"$install_dir:\$PATH\"" ;;
esac
