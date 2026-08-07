#!/bin/sh
set -eu

release_base="${MACH_RELEASES_URL:-https://machinesatplay.com/releases}"
install_dir="${MACH_INSTALL_DIR:-${HOME}/.local/bin}"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) platform="macos-aarch64" ;;
  Darwin-x86_64) platform="macos-x86_64" ;;
  Linux-x86_64) platform="linux-x86_64" ;;
  Linux-aarch64|Linux-arm64) platform="linux-aarch64" ;;
  *) platform="" ;;
esac

temporary_dir="$(mktemp -d)"
trap 'rm -rf "${temporary_dir}"' EXIT HUP INT TERM

checksum_matches() {
  archive_path="$1"
  checksum_path="$2"
  expected="$(awk 'NR == 1 { print tolower($1) }' "${checksum_path}")"
  if command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "${archive_path}" | awk '{print tolower($1)}')"
  else
    actual="$(sha256sum "${archive_path}" | awk '{print tolower($1)}')"
  fi
  [ "${actual}" = "${expected}" ]
}

version="$(curl --proto '=https' --tlsv1.2 -fsSL "${release_base}/latest/version")"
version="$(printf '%s' "${version}" | tr -d '[:space:]')"
case "${version}" in
  *[!0-9.]*|'') echo "mach: invalid release version" >&2; exit 1 ;;
esac

installed=0
if [ -n "${platform}" ]; then
  archive="mach-cli-${platform}.zip"
  download="${release_base}/v${version}/${archive}"
  if curl --proto '=https' --tlsv1.2 -fsSL "${download}" -o "${temporary_dir}/${archive}" \
    && curl --proto '=https' --tlsv1.2 -fsSL "${download}.sha256" -o "${temporary_dir}/${archive}.sha256"; then
    if ! checksum_matches "${temporary_dir}/${archive}" "${temporary_dir}/${archive}.sha256"; then
      echo "mach: download checksum did not match" >&2
      exit 1
    fi
    unzip -q "${temporary_dir}/${archive}" -d "${temporary_dir}/cli"
    mkdir -p "${install_dir}"
    install -m 755 "${temporary_dir}/cli/mach" "${install_dir}/mach"
    installed=1
  fi
fi

[ "${installed}" -eq 1 ] || {
  echo "mach: no prebuilt CLI is available for $(uname -s)-$(uname -m)" >&2
  exit 1
}

echo "mach: installed ${install_dir}/mach"
if [ "${MACH_SKIP_SETUP:-0}" != "1" ]; then
  "${install_dir}/mach" setup
fi
case ":${PATH}:" in
  *":${install_dir}:"*) ;;
  *) echo "mach: add ${install_dir} to PATH, then run: mach new my-game" ;;
esac
