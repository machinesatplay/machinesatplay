#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 VERSION RELEASE_DIRECTORY" >&2
  exit 2
fi

version="$1"
release_directory="$2"
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
platform="macos-aarch64"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) ;;
  *) echo "mach: starter prebuilds currently require apple silicon macos" >&2; exit 1 ;;
esac

engine_version="$(awk -F'"' '/const ENGINE_VERSION/ { print $2; exit }' "$repo_root/cli/src/main.rs")"
cache_version="$(awk -F'"' '/const BUILD_CACHE_VERSION/ { print $2; exit }' "$repo_root/cli/src/build_seed.rs")"
if [ "$version" != "$engine_version" ]; then
  echo "mach: requested version $version does not match engine version $engine_version" >&2
  exit 1
fi

build_root="/private/tmp/mach-build-$cache_version-$platform"
test -d "$build_root/target" -a -d "$build_root/cargo-home" || {
  echo "mach: build cache does not exist at $build_root" >&2
  exit 1
}

temporary_dir="$(mktemp -d)"
trap 'rm -rf "$temporary_dir"' EXIT
project="$temporary_dir/project"
starter_root="$temporary_dir/starter"
release_directory="$(mkdir -p "$release_directory" && cd "$release_directory" && pwd)"

MACH_SKIP_UPDATE=1 cargo run --quiet --locked -p mach-cli \
  --manifest-path "$repo_root/Cargo.toml" -- new "$project"

export CARGO_HOME="$build_root/cargo-home"
export CARGO_INCREMENTAL=0
project_target="$({
  MACH_SKIP_UPDATE=1 cargo run --quiet --locked -p mach-cli \
    --manifest-path "$repo_root/Cargo.toml" -- prepare-project "$project"
} | tail -n 1)"
test -n "$project_target" -a -d "$project_target" || {
  echo "mach: project build cache preparation failed" >&2
  exit 1
}
export CARGO_TARGET_DIR="$project_target"

cargo build --locked \
  --manifest-path "$project/Cargo.toml" \
  --profile mach-dev \
  --bin mach \
  --no-default-features \
  --features client,browser-webgpu

cargo build --locked \
  --manifest-path "$project/Cargo.toml" \
  --profile mach-dev \
  --package game-server \
  --bin mach-server \
  --no-default-features

mkdir -p "$starter_root/bin"
install -m 755 "$build_root/target/mach-dev/mach" "$starter_root/bin/mach-client"
install -m 755 "$build_root/target/mach-dev/mach-server" "$starter_root/bin/mach-server"

archive="$release_directory/mach-starter-$platform.zip"
rm -f "$archive" "$archive.sha256"
(cd "$starter_root" && zip -9 -qr "$archive" bin)
(cd "$release_directory" && shasum -a 256 "$(basename "$archive")" > "$(basename "$archive").sha256")

echo "mach: starter files are in $release_directory"
