#!/bin/sh
# Removes files a build or a fresh clone can recreate, wherever they sit in the working tree.
# Usage: clean.sh [--dry-run]
#
# Removes:
#   - folders marked with a valid CACHEDIR.TAG (Cargo target folders, tool caches)
#   - build output of the release bundle and the Tauri schemas
#   - nested git clones that hold nothing their remote lacks
#
# Keeps everything else. `git clean -X` is unsuitable: it would delete .env, private fixtures and
# recordings, which nothing can recreate.
set -eu

is_dry_run=false
case "${1:-}" in
  "") ;;
  --dry-run) is_dry_run=true ;;
  *)
    echo "clean.sh: unknown argument '$1'. The valid option is --dry-run." >&2
    exit 2
    ;;
esac

cd "$(dirname "$0")/.."

reclaimed_kb=0

remove_folder() {
  [ -d "$1" ] || return 0
  folder_kb=$(du -sk "$1" | cut -f1)
  reclaimed_kb=$((reclaimed_kb + folder_kb))
  printf '%8d MB  %s\n' "$((folder_kb / 1024))" "$1"
  if [ "$is_dry_run" = false ]; then
    rm -rf "$1"
  fi
}

is_recreatable_clone() {
  origin_url=$(git -C "$1" remote get-url origin 2>/dev/null) || return 1
  case "$origin_url" in
    https://* | ssh://* | git@*) ;;
    *) return 1 ;;
  esac
  clone_status=$(git -C "$1" status --porcelain --ignored 2>/dev/null) || return 1
  # A deleted tracked file stays in the remote's history. Clones made without Git LFS show these.
  [ -z "$(printf '%s\n' "$clone_status" | grep -v '^ D ')" ] || return 1
  [ -z "$(git -C "$1" stash list)" ] || return 1
  [ "$(git -C "$1" rev-list --count HEAD --branches --not --remotes)" = 0 ]
}

# The tag file may sit inside a tagged folder. With a trailing slash and byte order, a parent
# sorts directly before its children, so comparing with the last parent skips them.
tagged_folders=$(find . -name .git -prune -o -type f -name CACHEDIR.TAG -print |
  sed 's/CACHEDIR\.TAG$//' | LC_ALL=C sort)
last_tagged_folder=""
while IFS= read -r tagged_folder; do
  [ -n "$tagged_folder" ] || continue
  if [ -n "$last_tagged_folder" ]; then
    case "$tagged_folder" in
      "$last_tagged_folder"*) continue ;;
    esac
  fi
  grep -q '^Signature: 8a477f597d28d172789f06886806bc55' "${tagged_folder}CACHEDIR.TAG" || continue
  last_tagged_folder=$tagged_folder
  remove_folder "${tagged_folder%/}"
done <<EOF
$tagged_folders
EOF

remove_folder crates/app/binaries
remove_folder crates/app/gen

# This search runs after the tagged folders go, so a clone's own target folder does not keep it.
clones=$(find . -mindepth 2 -type d -name .git -prune -print | sed 's|/\.git$||')
while IFS= read -r clone; do
  [ -n "$clone" ] || continue
  if is_recreatable_clone "$clone"; then
    remove_folder "$clone"
  fi
done <<EOF
$clones
EOF

if [ "$is_dry_run" = true ]; then
  echo "Would free $((reclaimed_kb / 1024)) MB."
else
  echo "Freed $((reclaimed_kb / 1024)) MB."
fi
