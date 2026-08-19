#!/usr/bin/env bash
# Install Rift from checksummed GitHub Release archive.
#
#   curl --proto '=https' --tlsv1.2 -fsSL https://volar.sh/rift/install.sh | bash
#   curl --proto '=https' --tlsv1.2 -fsSL https://volar.sh/rift/install.sh | bash -s -- v1.2.3
set -euo pipefail

readonly RIFT_REPOSITORY="${RIFT_REPOSITORY:-volarized/rift}"
readonly RIFT_GITHUB_API="${RIFT_GITHUB_API:-https://api.github.com}"
readonly RIFT_DOWNLOAD_BASE="${RIFT_DOWNLOAD_BASE:-https://github.com/${RIFT_REPOSITORY}/releases/download}"
readonly RIFT_INSTALL_DIR="${RIFT_INSTALL_DIR:-${HOME}/.rift/bin}"
readonly REQUESTED_VERSION="${1:-${RIFT_VERSION:-latest}}"

work_dir=""
candidate=""

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

cleanup() {
  if [[ -n "$candidate" ]]; then
    rm -f "$candidate"
  fi
  if [[ -n "$work_dir" ]]; then
    rm -rf "$work_dir"
  fi
}

trap cleanup EXIT

require_https() {
  [[ "$1" == https://* ]] || fail "refusing non-HTTPS URL: $1"
}

fetch() {
  local url="$1"
  shift
  require_https "$url"
  curl \
    --proto '=https' \
    --proto-redir '=https' \
    --tlsv1.2 \
    --location \
    --max-redirs 5 \
    --connect-timeout 15 \
    --retry 3 \
    --fail \
    --silent \
    --show-error \
    "$@" \
    "$url"
}

valid_version() {
  [[ "$1" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]
}

resolve_version() {
  if [[ "$REQUESTED_VERSION" != latest ]]; then
    valid_version "$REQUESTED_VERSION" || fail "version must match vX.Y.Z: $REQUESTED_VERSION"
    printf '%s\n' "$REQUESTED_VERSION"
    return
  fi

  local response tag url
  url="${RIFT_GITHUB_API%/}/repos/${RIFT_REPOSITORY}/releases/latest"
  response="$(fetch "$url")" || fail "failed to resolve latest Rift release"
  tag="$(printf '%s\n' "$response" | sed -nE 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' | head -n 1)"
  valid_version "$tag" || fail "latest release returned invalid tag: ${tag:-missing}"
  printf '%s\n' "$tag"
}

detect_target() {
  local architecture system
  architecture="$(uname -m)"
  system="$(uname -s)"

  case "$architecture" in
    x86_64 | amd64) architecture=x86_64 ;;
    arm64 | aarch64) architecture=aarch64 ;;
    *) fail "unsupported architecture: $architecture" ;;
  esac

  case "$system" in
    Linux) system=unknown-linux-gnu ;;
    Darwin) system=apple-darwin ;;
    *) fail "unsupported operating system: $system" ;;
  esac

  printf '%s-%s\n' "$architecture" "$system"
}

file_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    fail "sha256sum or shasum is required"
  fi
}

verify_checksum() {
  local archive="$1" manifest="$2" expected actual
  expected="$(awk -v name="$(basename "$archive")" '$2 == name { print $1 }' "$manifest")"
  [[ "$expected" =~ ^[0-9a-fA-F]{64}$ ]] || fail "checksum manifest has no unique entry for $(basename "$archive")"
  actual="$(file_sha256 "$archive")"
  expected="$(printf '%s' "$expected" | tr '[:upper:]' '[:lower:]')"
  actual="$(printf '%s' "$actual" | tr '[:upper:]' '[:lower:]')"
  [[ "$actual" == "$expected" ]] || fail "checksum mismatch for $(basename "$archive")"
}

verify_archive() {
  local archive="$1" root="$2" expected actual
  expected="${root}/rift
${root}/README.md
${root}/LICENSE"
  actual="$(tar -tzf "$archive")"
  [[ "$actual" == "$expected" ]] || fail "archive contains unexpected files"
}

main() {
  command -v curl >/dev/null 2>&1 || fail "curl is required"
  command -v install >/dev/null 2>&1 || fail "install is required"
  command -v tar >/dev/null 2>&1 || fail "tar is required"

  local version target root archive checksums base
  version="$(resolve_version)"
  target="$(detect_target)"
  root="rift-${version}-${target}"
  archive="${root}.tar.gz"
  checksums="rift-${version}-checksums.sha256"
  base="${RIFT_DOWNLOAD_BASE%/}/${version}"

  work_dir="$(mktemp -d)"
  fetch "${base}/${archive}" --output "${work_dir}/${archive}"
  fetch "${base}/${checksums}" --output "${work_dir}/${checksums}"
  verify_checksum "${work_dir}/${archive}" "${work_dir}/${checksums}"
  verify_archive "${work_dir}/${archive}" "$root"
  tar -xzf "${work_dir}/${archive}" -C "$work_dir" "${root}/rift"

  mkdir -p "$RIFT_INSTALL_DIR"
  candidate="${RIFT_INSTALL_DIR}/.rift.$$"
  install -m 0755 "${work_dir}/${root}/rift" "$candidate"
  mv -f "$candidate" "${RIFT_INSTALL_DIR}/rift"
  candidate=""

  printf 'Installed Rift %s to %s/rift\n' "$version" "$RIFT_INSTALL_DIR"
  case ":${PATH}:" in
    *":${RIFT_INSTALL_DIR}:"*) ;;
    *) printf 'Add %s to PATH.\n' "$RIFT_INSTALL_DIR" ;;
  esac
}

main
