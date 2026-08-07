#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 VERSION RELEASE_DIRECTORY" >&2
  exit 2
fi

version="$1"
release_directory="$2"
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
package_version="$(awk '/^version = / { gsub(/[\" ]/, "", $3); print $3; exit }' "$repo_root/cli/Cargo.toml")"
if [ "$version" != "$package_version" ]; then
  echo "mach: requested version $version does not match CLI version $package_version" >&2
  exit 1
fi

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) platform="macos-aarch64" ;;
  Darwin-x86_64) platform="macos-x86_64" ;;
  Linux-x86_64) platform="linux-x86_64" ;;
  Linux-aarch64|Linux-arm64) platform="linux-aarch64" ;;
  *) echo "mach: unsupported release host $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

mkdir -p "$release_directory"
release_directory="$(cd "$release_directory" && pwd)"
temporary_dir="$(mktemp -d)"
trap 'rm -rf "$temporary_dir"' EXIT
target_directory="${MACH_RELEASE_TARGET_DIR:-/tmp/mach-release-target}"

MACH_OFFICIAL_RELEASE=1 CARGO_TARGET_DIR="$target_directory" \
  cargo build --locked --release -p mach-cli --manifest-path "$repo_root/Cargo.toml"

cli_bundle="$temporary_dir/cli"
mkdir -p "$cli_bundle"
cp "$target_directory/release/mach" "$cli_bundle/mach"
cli_archive="$release_directory/mach-cli-$platform.zip"
(cd "$cli_bundle" && zip -q "$cli_archive" mach)

archive_directory="$(dirname "$cli_archive")"
archive_name="$(basename "$cli_archive")"
(cd "$archive_directory" && shasum -a 256 "$archive_name" > "$archive_name.sha256")

unzip -tq "$cli_archive" >/dev/null
unzip -Z1 "$cli_archive" | grep -Fx mach >/dev/null || {
  echo "mach: $cli_archive is missing mach" >&2
  exit 1
}
echo "mach: release files are in $release_directory"
