#!/usr/bin/env sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
version=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$project_root/Cargo.toml" | head -n 1)
if [ -z "$version" ]; then
    echo "Could not read the package version from Cargo.toml." >&2
    exit 1
fi

if [ "${1:-}" != "--skip-build" ]; then
    cargo build --release --locked
fi

binary="$project_root/target/release/ohmyeyes"
if [ ! -x "$binary" ]; then
    echo "Release binary not found at $binary." >&2
    exit 1
fi

dist="$project_root/dist"
archive="$dist/OhMyEyes-$version-linux-x86_64-portable.tar.gz"
staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT HUP INT TERM

bundle="$staging/OhMyEyes-$version-linux-x86_64"
mkdir -p "$bundle" "$dist"
install -m 0755 "$binary" "$bundle/ohmyeyes"
install -m 0644 "$project_root/README.md" "$bundle/README.md"
install -m 0644 "$project_root/LICENSE" "$bundle/LICENSE"
tar -C "$staging" -czf "$archive" "$(basename "$bundle")"
sha256sum "$archive" > "$archive.sha256"
printf '%s\n' "$archive"
