#!/bin/sh
# Publishes a release from this Mac, for when the Release workflow gets no macOS runner.
# Usage: publish-release.sh check | upload | cask
#   check   stops unless the release can go out from this commit
#   upload  creates the GitHub release from target/dist, then runs cask
#   cask    points the Homebrew cask at the release. Run it alone if upload stopped at this step
set -eu

owner="kujjwal02"
repository="$owner/meeting-capture"
tap_repository="$owner/homebrew-tap"

fail() {
  echo "publish-release: $1" >&2
  exit 1
}

# The active gh account may be a work account with no access to these repositories.
GH_TOKEN="${GH_TOKEN:-$(gh auth token --user "$owner")}"
export GH_TOKEN

version=$(cargo metadata --no-deps --format-version 1 |
  jq -r '.packages[] | select(.name == "meeting-capture") | .version')
tag="v$version"
dmg_checksum_name="Meeting-Capture_${version}_aarch64.dmg.sha256"

check() {
  [ -z "$(git status --porcelain)" ] || fail "the working tree has changes"
  git fetch --quiet origin main --tags
  git merge-base --is-ancestor HEAD origin/main || fail "HEAD is not on origin/main"
  [ "$(git rev-parse "$tag^{commit}" 2>/dev/null)" = "$(git rev-parse HEAD)" ] ||
    fail "tag $tag does not point at HEAD. Push it first: git tag $tag && git push origin $tag"
  [ -n "$(git ls-remote --tags origin "refs/tags/$tag")" ] ||
    fail "tag $tag is not on origin. Push it first: git push origin $tag"
  if view_error=$(gh release view "$tag" --repo "$repository" 2>&1 >/dev/null); then
    fail "release $tag already exists"
  fi
  [ "$view_error" = "release not found" ] || fail "cannot read release $tag: $view_error"
}

upload() {
  [ -f "target/dist/$dmg_checksum_name" ] || fail "target/dist/$dmg_checksum_name is missing. Run just release first"
  gh release create "$tag" target/dist/* --repo "$repository" --verify-tag \
    --title "Meeting Capture $version" --generate-notes
  cask
}

cask() {
  # A later local build overwrites target/dist, so the published checksum is the one to trust.
  dmg_checksum=$(gh release download "$tag" --repo "$repository" --pattern "$dmg_checksum_name" --output -)
  dmg_sha256=${dmg_checksum%% *}
  tap_dir=$(mktemp -d)
  trap 'rm -rf "$tap_dir"' EXIT
  git clone --quiet "https://github.com/$tap_repository.git" "$tap_dir"
  cask_file="$tap_dir/Casks/meeting-capture.rb"
  sed -E \
    -e "s/^  version \".*\"$/  version \"$version\"/" \
    -e "s/^  sha256 \".*\"$/  sha256 \"$dmg_sha256\"/" \
    "$cask_file" >"$cask_file.new"
  mv "$cask_file.new" "$cask_file"
  if git -C "$tap_dir" diff --exit-code --quiet; then
    echo "publish-release: the cask already points at $version"
    return
  fi
  account_id=$(gh api "users/$owner" -q .id)
  git -C "$tap_dir" -c user.name="$owner" \
    -c user.email="$account_id+$owner@users.noreply.github.com" \
    commit --quiet -am "meeting-capture $version"
  git -C "$tap_dir" push --quiet
}

case "${1:-}" in
  check) check ;;
  upload) upload ;;
  cask) cask ;;
  *) fail "usage: publish-release.sh check | upload | cask" ;;
esac
