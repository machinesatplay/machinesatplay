#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
manifest="$repo_root/Cargo.toml"

server_tree="$(cargo tree --manifest-path "$manifest" -p game-server --no-default-features -e normal)"
for dependency in wgpu bevy_render bevy_winit bevy_audio bevy_ui render-fn; do
  if printf '%s\n' "$server_tree" | rg -q "(^| )${dependency} v"; then
    printf 'server dependency leak: %s\n' "$dependency" >&2
    exit 1
  fi
done

server_features="$(cargo tree --manifest-path "$manifest" -p game-server --no-default-features -e features -i lightyear)"
if printf '%s\n' "$server_features" | rg -Fq 'lightyear feature "client"'; then
  printf 'server dependency leak: lightyear client feature\n' >&2
  exit 1
fi

printf 'server dependency boundary ok\n'
