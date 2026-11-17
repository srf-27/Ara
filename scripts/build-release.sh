#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

if [ "${1:-}" = "--musl" ]; then
  cargo build --release -p ara --target x86_64-unknown-linux-musl
  plat="x86_64-unknown-linux-musl"
  bin_dir="target/x86_64-unknown-linux-musl/release"
  files=("ara")
else
  cargo build --release --workspace
  case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) plat="x86_64-pc-windows-msvc"; ext=".exe" ;;
    *) plat="$(rustc -vV | sed -n 's/^host: //p')"; ext="" ;;
  esac
  bin_dir="target/release"
  files=("ara$ext" "ara-server$ext")
fi

out="$root/target/dist/ara"
rm -rf "$out"
mkdir -p "$out"
for f in "${files[@]}"; do cp "$bin_dir/$f" "$out/"; done
cp ara.toml.example "$out/"
cp docs/packaging.md "$out/README.md"
if [ -f ara/vendor/wintun.dll ]; then cp ara/vendor/wintun.dll "$out/"; fi
tar -czf "$root/target/dist/ara-$plat.tar.gz" -C "$root/target/dist" ara
echo "$root/target/dist/ara-$plat.tar.gz"
