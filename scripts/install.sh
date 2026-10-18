#!/bin/sh
# Installs or upgrades Meeting Capture from a GitHub release.
# Usage: curl -fsSL https://raw.githubusercontent.com/kujjwal02/meeting-capture/main/scripts/install.sh | sh
# Set MEETING_CAPTURE_VERSION=0.1.0 to pick a release; the default is the latest.
# curl sets no quarantine flag, so macOS opens the self-signed app without the "Apple could not
# verify" prompt that a browser download gets.
set -eu

repo=kujjwal02/meeting-capture
app_name="Meeting Capture"
install_dir=/Applications

fail() {
  echo "error: $*" >&2
  exit 1
}

# The whole script sits in a function, so a download cut short by the pipe runs nothing.
main() {
  [ "$(uname -s)" = Darwin ] || fail "Meeting Capture runs on macOS only."
  [ "$(uname -m)" = arm64 ] || fail "releases are built for Apple silicon only."
  [ -w "$install_dir" ] || fail "$install_dir is not writable by $(id -un). Use an admin account."

  version=${MEETING_CAPTURE_VERSION:-}
  if [ -z "$version" ]; then
    version=$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" |
      sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' | head -n 1)
    [ -n "$version" ] || fail "could not read the latest release from GitHub."
  fi

  dmg="Meeting-Capture_${version}_aarch64.dmg"
  base_url="https://github.com/$repo/releases/download/v$version"
  work=$(mktemp -d)
  mount_dir="$work/mount"
  trap 'hdiutil detach -quiet "$mount_dir" 2>/dev/null || true; rm -rf "$work"' EXIT

  echo "Downloading Meeting Capture $version"
  curl -fL --progress-bar -o "$work/$dmg" "$base_url/$dmg"
  curl -fsSL -o "$work/$dmg.sha256" "$base_url/$dmg.sha256"
  (cd "$work" && shasum -a 256 -c "$dmg.sha256" >/dev/null) ||
    fail "the download does not match its SHA-256 checksum."

  # The app allows one instance, so a running copy would make the new one exit at launch.
  if pgrep -xq meeting-capture; then
    echo "Quitting the running copy"
    osascript -e "quit app \"$app_name\"" >/dev/null 2>&1 || true
    waited_s=0
    while pgrep -xq meeting-capture && [ "$waited_s" -lt 10 ]; do
      sleep 1
      waited_s=$((waited_s + 1))
    done
    pgrep -xq meeting-capture && fail "Meeting Capture is still running. Quit it from the tray and run this again."
  fi

  mkdir "$mount_dir"
  hdiutil attach -quiet -nobrowse -readonly -mountpoint "$mount_dir" "$work/$dmg" 2>/dev/null ||
    fail "could not open $dmg."
  rm -rf "${install_dir:?}/$app_name.app"
  ditto "$mount_dir/$app_name.app" "$install_dir/$app_name.app"

  echo "Installed Meeting Capture $version in $install_dir"
  open "$install_dir/$app_name.app"
  echo "Next: https://github.com/$repo/blob/main/docs/08-install.md#first-time-setup"
}

main "$@"
