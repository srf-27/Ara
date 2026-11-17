#!/usr/bin/env bash
set -euo pipefail

prefix="${ARA_PREFIX:-/usr/local/bin}"
mirror="${ARA_MIRROR:-https://ghfast.top/https://github.com/srf-27/Ara/releases/latest/download}"
asset="${ARA_ASSET:-ara-x86_64-unknown-linux-musl.tar.gz}"
src="${1:-}"

if [ -d "$src" ]; then
  dir="$src"
else
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  if [ -f "$src" ]; then
    archive="$src"
  else
    archive="$tmp/$asset"
    curl -fL --retry 2 -o "$archive" "$mirror/$asset"
  fi
  tar -xzf "$archive" -C "$tmp"
  dir="$tmp/ara"
fi

[ -f "$dir/ara" ] || { echo "ara binary missing in $dir" >&2; exit 1; }
mkdir -p "$prefix"
install -m 755 "$dir/ara" "$prefix/ara"
if [ -f "$dir/ara-server" ]; then install -m 755 "$dir/ara-server" "$prefix/ara-server"; fi
if [ ! -e /dev/net/tun ]; then
  echo "warning: /dev/net/tun missing; TUN needs root and a kernel tun device" >&2
fi
"$prefix/ara" --help >/dev/null
echo "installed $prefix/ara; edit ara.toml.example then run: sudo $prefix/ara up --config ara.toml"
