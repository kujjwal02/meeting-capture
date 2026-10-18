#!/bin/sh
# Prints the latest.json the app's updater reads from the latest GitHub release.
# Usage: updater-manifest.sh <version> <updater package>. The package's .sig file sits next to it.
set -eu

version=${1:?usage: updater-manifest.sh <version> <updater package>}
package=${2:?usage: updater-manifest.sh <version> <updater package>}
url="https://github.com/kujjwal02/meeting-capture/releases/download/v$version/$(basename "$package")"

jq -n \
  --arg version "$version" \
  --arg notes "https://github.com/kujjwal02/meeting-capture/releases/tag/v$version" \
  --arg pub_date "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --arg signature "$(cat "$package.sig")" \
  --arg url "$url" \
  '{version: $version, notes: $notes, pub_date: $pub_date,
    platforms: {"darwin-aarch64": {signature: $signature, url: $url}}}'
