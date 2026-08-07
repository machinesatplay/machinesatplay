#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 VERSION RELEASE_DIRECTORY" >&2
  exit 2
fi

version="$1"
release_directory="$2"
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cache_version="$(awk -F'"' '/const BUILD_CACHE_VERSION/ { print $2; exit }' "$repo_root/cli/src/build_seed.rs")"

verify_archive() {
  archive="$1"
  checksum_file="$archive.sha256"
  test -f "$archive" || { echo "missing $archive" >&2; return 1; }
  test -f "$checksum_file" || { echo "missing $checksum_file" >&2; return 1; }
  expected="$(awk 'NR == 1 { print tolower($1) }' "$checksum_file")"
  actual="$(shasum -a 256 "$archive" | awk '{ print tolower($1) }')"
  [ "$expected" = "$actual" ] || { echo "checksum mismatch for $archive" >&2; return 1; }
  unzip -tq "$archive" >/dev/null
}

verify_tar_zstd_archive() {
  archive="$1"
  checksum_file="$archive.sha256"
  test -f "$archive" || { echo "missing $archive" >&2; return 1; }
  test -f "$checksum_file" || { echo "missing $checksum_file" >&2; return 1; }
  expected="$(awk 'NR == 1 { print tolower($1) }' "$checksum_file")"
  actual="$(shasum -a 256 "$archive" | awk '{ print tolower($1) }')"
  [ "$expected" = "$actual" ] || { echo "checksum mismatch for $archive" >&2; return 1; }
  zstd --long=28 -tq "$archive"
  zstd --long=28 -dcq "$archive" | bsdtar -tf - >/dev/null
}

file_size() {
  if stat -f %z "$1" >/dev/null 2>&1; then
    stat -f %z "$1"
  else
    stat -c %s "$1"
  fi
}

require_zip_entry() {
  archive="$1"
  entry="$2"
  unzip -Z1 "$archive" | grep -Fx "$entry" >/dev/null || {
    echo "$archive is missing $entry" >&2
    return 1
  }
}

reject_unexpected_starter_entries() {
  archive="$1"
  unexpected="$(unzip -Z1 "$archive" | awk '
    $0 != "bin/" &&
    $0 != "bin/mach-client" &&
    $0 != "bin/mach-server" { print; exit }
  ')"
  [ -z "$unexpected" ] || {
    echo "$archive contains unexpected $unexpected" >&2
    return 1
  }
}

require_tar_entry() {
  archive="$1"
  entry="$2"
  zstd --long=28 -dcq "$archive" | bsdtar -tf - \
    | awk -v entry="$entry" '$0 == entry { found = 1 } END { exit !found }' || {
      echo "$archive is missing $entry" >&2
      return 1
    }
}

require_tar_prefix() {
  archive="$1"
  prefix="$2"
  zstd --long=28 -dcq "$archive" | bsdtar -tf - \
    | awk -v prefix="$prefix" 'index($0, prefix) == 1 { found = 1 } END { exit !found }' || {
      echo "$archive is missing $prefix" >&2
      return 1
    }
}

reject_tar_prefix() {
  archive="$1"
  prefix="$2"
  if zstd --long=28 -dcq "$archive" | bsdtar -tf - \
    | awk -v prefix="$prefix" 'index($0, prefix) { found = 1 } END { exit !found }'; then
    echo "$archive contains forbidden cache path $prefix" >&2
    return 1
  fi
}

verify_cache_parts() {
  archive="$1"
  test -f "$archive.parts.json" || {
    echo "missing $archive.parts.json" >&2
    return 1
  }
  part_count=0
  for part in "$archive".part-*; do
    [ -f "$part" ] || continue
    part_count=$((part_count + 1))
    [ "$(file_size "$part")" -le $((300 * 1024 * 1024)) ] || {
      echo "$part exceeds the R2 upload limit" >&2
      return 1
    }
  done
  [ "$part_count" -gt 0 ] || { echo "missing cache parts" >&2; return 1; }
  parts_sha256="$(cat "$archive".part-* | shasum -a 256 | awk '{ print $1 }')"
  archive_sha256="$(awk 'NR == 1 { print $1 }' "$archive.sha256")"
  [ "$parts_sha256" = "$archive_sha256" ] || {
    echo "cache parts do not reconstruct $archive" >&2
    return 1
  }
}

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) platform="macos-aarch64" ;;
  Darwin-x86_64) platform="macos-x86_64" ;;
  Linux-x86_64) platform="linux-x86_64" ;;
  Linux-aarch64|Linux-arm64) platform="linux-aarch64" ;;
  *) echo "unsupported release host $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

cli_archive="$release_directory/mach-cli-$platform.zip"
verify_archive "$cli_archive"
require_zip_entry "$cli_archive" mach

case "$platform" in
  macos-aarch64)
    starter_archive="$release_directory/mach-starter-$platform.zip"
    verify_archive "$starter_archive"
    require_zip_entry "$starter_archive" bin/mach-client
    require_zip_entry "$starter_archive" bin/mach-server
    reject_unexpected_starter_entries "$starter_archive"
    if [ "$version" = "$cache_version" ]; then
      cache_archive="$release_directory/mach-build-cache-$platform.tar.zst"
      test -f "$cache_archive" || { echo "missing $cache_archive" >&2; exit 1; }
      verify_tar_zstd_archive "$cache_archive"
      require_tar_entry "$cache_archive" seed.json
      require_tar_prefix "$cache_archive" target/mach-dev/
      reject_tar_prefix "$cache_archive" cargo-home/registry/cache/
      reject_tar_prefix "$cache_archive" target/mach-deploy/
      reject_tar_prefix "$cache_archive" target/wasm32-unknown-unknown/
      reject_tar_prefix "$cache_archive" target/x86_64-unknown-linux-musl/
      reject_tar_prefix "$cache_archive" target/debug/
      reject_tar_prefix "$cache_archive" /incremental/
      reject_tar_prefix "$cache_archive" /mach.wasm
      reject_tar_prefix "$cache_archive" .rmeta
      verify_cache_parts "$cache_archive"

      deploy_archive="$release_directory/mach-deploy-cache-$platform.tar.zst"
      test -f "$deploy_archive" || { echo "missing $deploy_archive" >&2; exit 1; }
      verify_tar_zstd_archive "$deploy_archive"
      require_tar_entry "$deploy_archive" deploy-seed.json
      require_tar_prefix "$deploy_archive" target/mach-deploy/
      require_tar_prefix "$deploy_archive" target/wasm32-unknown-unknown/mach-deploy/
      require_tar_prefix "$deploy_archive" target/x86_64-unknown-linux-musl/mach-deploy/
      reject_tar_prefix "$deploy_archive" cargo-home/.global-cache
      reject_tar_prefix "$deploy_archive" cargo-home/registry/index/
      reject_tar_prefix "$deploy_archive" cargo-home/registry/cache/
      reject_tar_prefix "$deploy_archive" target/mach-dev/
      reject_tar_prefix "$deploy_archive" target/debug/
      reject_tar_prefix "$deploy_archive" /incremental/
      reject_tar_prefix "$deploy_archive" /mach.wasm
      unexpected_rmeta="$(zstd --long=28 -dcq "$deploy_archive" | bsdtar -tf - | awk '
        /\.rmeta$/ && index($0, "target/x86_64-unknown-linux-musl/mach-deploy/") != 1 {
          print
          exit
        }
      ')"
      [ -z "$unexpected_rmeta" ] || {
        echo "$deploy_archive contains unexpected metadata $unexpected_rmeta" >&2
        exit 1
      }
      verify_cache_parts "$deploy_archive"
    fi
    ;;
esac

echo "mach: release assets verified for $version"
