#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 VERSION RELEASE_DIRECTORY" >&2
  exit 2
fi

version="$1"
release_directory="$2"
repo_root="$(cd "$(dirname "$0")/.." && pwd)"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) platform="macos-aarch64" ;;
  *) echo "mach: the prebuilt cache is only available for apple silicon macs" >&2; exit 1 ;;
esac

cache_version="$(awk -F'"' '/const BUILD_CACHE_VERSION/ { print $2; exit }' "$repo_root/cli/src/build_seed.rs")"
if [ "$version" != "$cache_version" ]; then
  echo "mach: requested version $version does not match build cache version $cache_version" >&2
  exit 1
fi

rust_version="$(rustc --version | awk '{ print $2 }')"
if [ "$rust_version" != "1.96.0" ]; then
  echo "mach: rust 1.96.0 is required to create the build cache" >&2
  exit 1
fi
sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{ print tolower($1) }'
  else
    sha256sum "$1" | awk '{ print tolower($1) }'
  fi
}

file_size() {
  if stat -f %z "$1" >/dev/null 2>&1; then
    stat -f %z "$1"
  else
    stat -c %s "$1"
  fi
}

append_registry_sources() {
  local name="$1"
  local archive_list="$2"
  shift 2
  local source_list="$temporary_dir/$name-registry-sources"
  local source_archive_list="$temporary_dir/$name-registry-source-files"
  rg -o --no-filename "$build_root/cargo-home/registry/src/[^ :\\\\]+" \
    "$@" -g '*.d' \
    | sed -E "s#($build_root/cargo-home/registry/src/[^/]+/[^/]+).*#\1#" \
    | sort -u > "$source_list"
  while IFS= read -r source; do
    case "$(realpath "$source")" in
      "$build_root"/cargo-home/registry/src/*)
        find "$source" -type f -print | sed "s#^$build_root/##"
        ;;
      *)
        echo "mach: registry source escaped the build root: $source" >&2
        exit 1
        ;;
    esac
  done < "$source_list" | sort -u > "$source_archive_list"
  cat "$source_archive_list" >> "$archive_list"
}

package_cache() {
  local archive="$1"
  local archive_list="$2"
  local uncompressed_archive="$temporary_dir/$(basename "$archive").tar"
  rm -f "$archive" "$archive.sha256" "$archive.parts.json" "$archive".part-*
  sort -u -o "$archive_list" "$archive_list"
  (cd "$build_root" && bsdtar -cf "$uncompressed_archive" -T "$archive_list")
  zstd -15 --long=27 -T0 -q "$uncompressed_archive" -o "$archive"
  local archive_sha256
  archive_sha256="$(sha256_file "$archive")"
  printf '%s  %s\n' "$archive_sha256" "$(basename "$archive")" > "$archive.sha256"
  zstd --long=28 -tq "$archive"

  split -b 250m -d -a 3 "$archive" "$archive.part-"
  local archive_size
  archive_size="$(file_size "$archive")"
  local first_part=1
  {
    printf '{"schemaVersion":1,"sha256":"%s","size":%s,"parts":[' "$archive_sha256" "$archive_size"
    for part in "$archive".part-*; do
      [ "$first_part" -eq 1 ] || printf ','
      first_part=0
      local part_name part_sha256 part_size
      part_name="$(basename "$part")"
      part_sha256="$(sha256_file "$part")"
      part_size="$(file_size "$part")"
      printf '{"name":"%s","sha256":"%s","size":%s}' "$part_name" "$part_sha256" "$part_size"
    done
    printf ']}\n'
  } > "$archive.parts.json"
}

release_directory="$(mkdir -p "$release_directory" && cd "$release_directory" && pwd)"
build_root="/private/tmp/mach-build-$version-$platform"
temporary_dir="$(mktemp -d)"
trap 'rm -rf "$temporary_dir"' EXIT
project="$temporary_dir/project"
package_only="${MACH_CACHE_PACKAGE_ONLY:-0}"
deploy_only="${MACH_CACHE_DEPLOY_ONLY:-0}"

case "$build_root" in
  /private/tmp/mach-build-[0-9]*-macos-aarch64) ;;
  *) echo "mach: refusing to reset unsafe build root $build_root" >&2; exit 1 ;;
esac
if [ "$package_only" = "1" ] || [ "$deploy_only" = "1" ]; then
  test -d "$build_root/target" -a -d "$build_root/cargo-home" || {
    echo "mach: build cache does not exist at $build_root" >&2
    exit 1
  }
else
  rm -rf "$build_root"
  mkdir -p "$build_root/target" "$build_root/cargo-home"
fi
cargo run --quiet --locked -p mach-cli --manifest-path "$repo_root/Cargo.toml" -- new "$project"

export CARGO_HOME="$build_root/cargo-home"
export CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR="$build_root/target"

if [ "$deploy_only" != "1" ]; then
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

  starter_root="$temporary_dir/starter"
  mkdir -p "$starter_root/bin"
  install -m 755 "$build_root/target/mach-dev/mach" "$starter_root/bin/mach-client"
  install -m 755 "$build_root/target/mach-dev/mach-server" "$starter_root/bin/mach-server"
fi

rustup target add wasm32-unknown-unknown x86_64-unknown-linux-musl

RUSTFLAGS='--cfg getrandom_backend="wasm_js"' cargo build --locked \
  --manifest-path "$project/Cargo.toml" \
  --profile mach-deploy \
  --target wasm32-unknown-unknown \
  --bin mach \
  --no-default-features \
  --features client,browser-webgpu

RUSTFLAGS='--cfg getrandom_backend="wasm_js"' cargo build --locked \
  --manifest-path "$project/Cargo.toml" \
  --profile mach-deploy \
  --target wasm32-unknown-unknown \
  --bin mach \
  --no-default-features \
  --features client,browser-webgl2

cargo zigbuild --locked \
  --manifest-path "$project/Cargo.toml" \
  --profile mach-deploy \
  --target x86_64-unknown-linux-musl \
  --package game-server \
  --bin mach-server \
  --no-default-features
if [ "$deploy_only" != "1" ]; then
  starter_archive="$release_directory/mach-starter-$platform.zip"
  rm -f "$starter_archive" "$starter_archive.sha256"
  (cd "$starter_root" && zip -9 -qr "$starter_archive" bin)
  starter_sha256="$(sha256_file "$starter_archive")"
  printf '%s  %s\n' "$starter_sha256" "$(basename "$starter_archive")" \
    > "$starter_archive.sha256"

  cargo clean --quiet \
    --manifest-path "$project/Cargo.toml" \
    --profile mach-dev \
    -p mach \
    -p game-client \
    -p game-core \
    -p game-format \
    -p game-server \
    -p render-api \
    -p render-fn
fi

for target in wasm32-unknown-unknown x86_64-unknown-linux-musl; do
  cargo clean --quiet \
    --manifest-path "$project/Cargo.toml" \
    --profile mach-deploy \
    --target "$target" \
    -p mach \
    -p game-client \
    -p game-core \
    -p game-format \
    -p game-server \
    -p render-api \
    -p render-fn
done

cargo clean --quiet \
  --manifest-path "$project/Cargo.toml" \
  --profile mach-deploy \
  -p mach \
  -p game-client \
  -p game-core \
  -p game-format \
  -p game-server \
  -p render-api \
  -p render-fn

find "$build_root/target" -type d \( -name incremental -o -name examples \) -prune -exec rm -rf {} +
find "$build_root/target" -type f \( -name 'mach' -o -name 'mach.wasm' -o -name 'mach.d' -o -name 'mach-server' -o -name 'mach-server.d' \) -delete
for stale in \
  "$build_root/target/debug"
do
  [ ! -e "$stale" ] || rm -rf "$stale"
done

if find "$build_root/cargo-home" -maxdepth 2 -type f \
  \( -name credentials -o -name credentials.toml \) | grep -q .; then
  echo "mach: refusing to package Cargo credentials" >&2
  exit 1
fi

if [ "$deploy_only" != "1" ]; then
  printf '{"schemaVersion":1,"machVersion":"%s","platform":"%s","rustVersion":"%s"}\n' \
    "$version" "$platform" "$rust_version" > "$build_root/seed.json"
fi
printf '{"schemaVersion":1,"machVersion":"%s","platform":"%s","rustVersion":"%s","cache":"deploy"}\n' \
  "$version" "$platform" "$rust_version" > "$build_root/deploy-seed.json"

if [ "$deploy_only" != "1" ]; then
  build_archive="$release_directory/mach-build-cache-$platform.tar.zst"
  build_archive_list="$temporary_dir/build-archive-files"
  for relative in cargo-home/.global-cache seed.json; do
    [ ! -f "$build_root/$relative" ] || printf '%s\n' "$relative" >> "$build_archive_list"
  done
  for relative in \
    cargo-home/registry/index \
    target/mach-dev
  do
    [ ! -d "$build_root/$relative" ] || find "$build_root/$relative" \
      -type f ! -name '*.rmeta' -print \
      | sed "s#^$build_root/##" >> "$build_archive_list"
  done
  append_registry_sources build "$build_archive_list" \
    "$build_root/target/mach-dev"
  package_cache "$build_archive" "$build_archive_list"
fi

deploy_archive="$release_directory/mach-deploy-cache-$platform.tar.zst"
deploy_archive_list="$temporary_dir/deploy-archive-files"
printf '%s\n' deploy-seed.json >> "$deploy_archive_list"
for relative in \
  target/mach-deploy \
  target/wasm32-unknown-unknown/mach-deploy \
  target/x86_64-unknown-linux-musl/mach-deploy
do
  [ ! -d "$build_root/$relative" ] || find "$build_root/$relative" \
    -type f ! -name '*.rmeta' -print \
    | sed "s#^$build_root/##" >> "$deploy_archive_list"
done
find "$build_root/target/x86_64-unknown-linux-musl/mach-deploy" \
  -type f -name '*.rmeta' -print \
  | sed "s#^$build_root/##" >> "$deploy_archive_list"
append_registry_sources deploy "$deploy_archive_list" \
  "$build_root/target/mach-deploy" \
  "$build_root/target/wasm32-unknown-unknown/mach-deploy" \
  "$build_root/target/x86_64-unknown-linux-musl/mach-deploy"
package_cache "$deploy_archive" "$deploy_archive_list"

if [ "$deploy_only" != "1" ]; then
  echo "mach: $platform build cache is in $build_archive"
  echo "mach: $platform starter bundle is in $starter_archive"
fi
echo "mach: $platform deploy cache is in $deploy_archive"
