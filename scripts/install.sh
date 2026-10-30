#!/usr/bin/env sh
set -eu

REPO="RooAGI/Membrick"
BASE_URL="https://github.com/${REPO}/releases/latest/download"
INSTALL_DIR="${MEMBRICK_INSTALL_DIR:-${HOME}/.local/bin}"
AGENT=""
PROJECT="."
SHARE_INSTALL_TELEMETRY=0
INSTALL_TELEMETRY_URL="https://license.rooagi.com/v1/telemetry/install"

usage() {
  cat <<'EOF'
Install the latest official Membrick release and optionally configure an agent.

Usage:
  install.sh [--agent codex|claude|gemini|agy] [--project PATH] [--install-dir DIR]
            [--share-telemetry]

Examples:
  install.sh
  install.sh --agent codex --project .
  install.sh --agent claude --project /path/to/project
  install.sh --share-telemetry
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --agent)
      [ "$#" -ge 2 ] || { echo "--agent requires a value" >&2; exit 2; }
      AGENT="$2"
      shift 2
      ;;
    --project)
      [ "$#" -ge 2 ] || { echo "--project requires a value" >&2; exit 2; }
      PROJECT="$2"
      shift 2
      ;;
    --install-dir)
      [ "$#" -ge 2 ] || { echo "--install-dir requires a value" >&2; exit 2; }
      INSTALL_DIR="$2"
      shift 2
      ;;
    --share-telemetry)
      SHARE_INSTALL_TELEMETRY=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

case "$AGENT" in
  ""|codex|claude|gemini|agy) ;;
  *) echo "unsupported agent: $AGENT" >&2; exit 2 ;;
esac

OS="$(uname -s)"
ARCH="$(uname -m)"
case "${OS}:${ARCH}" in
  Linux:x86_64|Linux:amd64)
    ASSET="membrick-linux-x86_64"
    ;;
  Darwin:x86_64|Darwin:amd64)
    ASSET="membrick-macos-x86_64"
    ;;
  Darwin:arm64|Darwin:aarch64)
    ASSET="membrick-macos-aarch64"
    ;;
  *)
    echo "No prebuilt Membrick release for ${OS} ${ARCH}." >&2
    echo "Fallback: cargo install --git https://github.com/${REPO} --features agent-integrations" >&2
    exit 1
    ;;
esac

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  echo "curl or wget is required" >&2
  exit 1
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

BIN_TMP="${TMP_DIR}/${ASSET}"
SUM_TMP="${TMP_DIR}/SHA256SUMS"

echo "Downloading official Membrick release for ${OS} ${ARCH}..."
fetch "${BASE_URL}/${ASSET}" "$BIN_TMP"
if fetch "${BASE_URL}/SHA256SUMS" "$SUM_TMP"; then
  EXPECTED="$(awk -v asset="$ASSET" '$2 == asset {print $1}' "$SUM_TMP")"
else
  # Support releases published before the aggregate manifest was introduced.
  fetch "${BASE_URL}/${ASSET}.sha256" "$SUM_TMP"
  EXPECTED="$(awk '{print $1}' "$SUM_TMP")"
fi
if [ -z "$EXPECTED" ]; then
  echo "No checksum found for ${ASSET}; refusing to install." >&2
  exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL="$(sha256sum "$BIN_TMP" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL="$(shasum -a 256 "$BIN_TMP" | awk '{print $1}')"
else
  echo "sha256sum or shasum is required to verify the release" >&2
  exit 1
fi

if [ "$EXPECTED" != "$ACTUAL" ]; then
  echo "Checksum verification failed; refusing to install." >&2
  exit 1
fi

echo "Checksum verified."
chmod 0755 "$BIN_TMP"
if [ "$OS" = "Darwin" ]; then
  # Rust's linker emits only a linker-signed ad-hoc signature. Recent macOS
  # releases may SIGKILL that form at launch; replace it with a full ad-hoc
  # signature after verifying the downloaded bytes.
  codesign --force --sign - "$BIN_TMP"
  codesign --verify --strict "$BIN_TMP"
fi

if VERSION="$("$BIN_TMP" --version 2>&1)"; then
  if [ -z "$VERSION" ]; then
    echo "Downloaded Membrick executable returned no version; refusing to install." >&2
    exit 1
  fi
else
  printf '%s\n' "$VERSION" >&2
  echo "Downloaded Membrick executable failed its startup check; refusing to install." >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
cp "$BIN_TMP" "${INSTALL_DIR}/membrick"
BIN="${INSTALL_DIR}/membrick"
touch "${INSTALL_DIR}/.membrick-managed"
printf '%s\n' standalone > "${INSTALL_DIR}/membrick-install-method"

echo "Installed ${VERSION} to ${BIN}"

case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    echo "Note: ${INSTALL_DIR} is not currently on PATH."
    echo "Add it to your shell profile, for example: export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac

if [ -n "$AGENT" ]; then
  echo "Configuring Membrick for ${AGENT} in ${PROJECT}..."
  "$BIN" install "$AGENT" "$PROJECT"
  echo "Verifying the Membrick MCP runtime..."
  "$BIN" verify "$AGENT" "$PROJECT"
  echo "Membrick is installed, configured for ${AGENT}, and MCP verification passed."
else
  echo "Membrick is installed. To configure an agent, rerun with --agent codex|claude|gemini|agy."
fi

share_install_telemetry() {
  if [ "$SHARE_INSTALL_TELEMETRY" -ne 1 ]; then
    [ -r /dev/tty ] && [ -w /dev/tty ] || return 0
    printf '\nShare pseudonymous install details with RooAGI? This sends the Membrick version, OS, CPU architecture, installer type, selected agent, and a random install ID. It does not send names, email, project paths, or memory data. [y/N] ' >/dev/tty
    IFS= read -r answer </dev/tty || return 0
    case "$answer" in y|Y|yes|YES|Yes) ;; *) return 0 ;; esac
  fi

  command -v curl >/dev/null 2>&1 || return 0
  config_dir="${XDG_CONFIG_HOME:-${HOME}/.config}/membrick"
  id_file="${config_dir}/install-id"
  mkdir -p "$config_dir" 2>/dev/null || return 0
  if [ ! -s "$id_file" ]; then
    install_id="$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
    [ "${#install_id}" -eq 32 ] || return 0
    (umask 077; printf '%s\n' "$install_id" > "$id_file") || return 0
  else
    install_id="$(cat "$id_file" 2>/dev/null || :)"
    case "$install_id" in
      *[!A-Fa-f0-9]*|'') install_id="" ;;
    esac
    [ "${#install_id}" -eq 32 ] || install_id=""
    if [ -z "$install_id" ]; then
      install_id="$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
      [ "${#install_id}" -eq 32 ] || return 0
      (umask 077; printf '%s\n' "$install_id" > "$id_file") || return 0
    fi
  fi
  event_id="$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
  [ "${#event_id}" -eq 32 ] || return 0
  version="$(printf '%s\n' "$VERSION" | sed -n '1s/.* //p')"
  case "$version" in ''|*[!A-Za-z0-9._+-]*) version="unknown" ;; esac
  case "$OS" in Darwin) os="macos" ;; Linux) os="linux" ;; *) os="other" ;; esac
  case "$ARCH" in x86_64|amd64) architecture="x86_64" ;; arm64|aarch64) architecture="aarch64" ;; *) architecture="other" ;; esac
  agent="${AGENT:-none}"
  payload="{\"schema_version\":1,\"event_id\":\"${event_id}\",\"install_id\":\"${install_id}\",\"installed_at_unix\":$(date +%s),\"version\":\"${version}\",\"os\":\"${os}\",\"architecture\":\"${architecture}\",\"installer\":\"install_sh\",\"agent\":\"${agent}\",\"outcome\":\"success\",\"failure_stage\":\"none\"}"
  curl --silent --fail --max-time 4 -H 'Content-Type: application/json' -d "$payload" "$INSTALL_TELEMETRY_URL" >/dev/null 2>&1 || :
}

share_install_telemetry || :
