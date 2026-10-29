#!/usr/bin/env sh
set -eu

AGENT=""
PROJECT="."
MEMBRICK_BIN=""

usage() {
  cat <<'EOF'
Migrate an existing Lint-AI project integration to Membrick.

Usage:
  migrate-lint-ai.sh --agent codex|claude|gemini|agy [--project PATH] [--membrick-bin PATH]

Membrick must already be installed. By default, this uses `membrick` from PATH.

This updates the selected agent configuration using an existing Membrick
install. Existing .lint-ai project data is moved into .membrick during setup.
If both directories exist, non-conflicting entries are merged and conflicting legacy
entries are preserved under .membrick/migration-archive. The project-root
.lint-ai directory is removed after migration. The old lint-ai executable is
left in place.
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
      [ "$#" -ge 2 ] || { echo "--project requires a path" >&2; exit 2; }
      PROJECT="$2"
      shift 2
      ;;
    --membrick-bin)
      [ "$#" -ge 2 ] || { echo "--membrick-bin requires a path" >&2; exit 2; }
      MEMBRICK_BIN="$2"
      shift 2
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
  codex|claude|gemini|agy) ;;
  *) echo "choose an agent with --agent codex|claude|gemini|agy" >&2; exit 2 ;;
esac

[ -d "$PROJECT" ] || { echo "project directory does not exist: $PROJECT" >&2; exit 2; }
PROJECT="$(cd "$PROJECT" && pwd -P)"

if [ -z "$MEMBRICK_BIN" ]; then
  if command -v membrick >/dev/null 2>&1; then
    MEMBRICK_BIN="$(command -v membrick)"
  else
    echo "Membrick is not installed or not on PATH." >&2
    echo "Install Membrick first, or pass --membrick-bin PATH to an existing executable." >&2
    exit 1
  fi
fi

if [ ! -x "$MEMBRICK_BIN" ]; then
  echo "Membrick executable is missing or not executable: $MEMBRICK_BIN" >&2
  exit 1
fi

if VERSION="$("$MEMBRICK_BIN" --version 2>&1)"; then
  if [ -z "$VERSION" ]; then
    echo "Membrick executable returned no version: $MEMBRICK_BIN" >&2
    exit 1
  fi
else
  printf '%s\n' "$VERSION" >&2
  echo "Membrick executable failed its startup check: $MEMBRICK_BIN" >&2
  exit 1
fi

echo "Using $VERSION from $MEMBRICK_BIN"
"$MEMBRICK_BIN" install "$AGENT" "$PROJECT"
"$MEMBRICK_BIN" verify "$AGENT" "$PROJECT"

cat <<EOF

Lint-AI migration finished for $AGENT.
The existing Membrick executable updated and verified that agent's connection.
Existing project data is preserved under .membrick, and the project-root
.lint-ai directory has been removed. Any conflicting legacy state is preserved under
.membrick/migration-archive.
The old lint-ai executable was not removed; remove it after confirming Membrick
works in your projects.
EOF
