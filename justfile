set windows-shell := ["pwsh.exe", "-NoLogo", "-Command"]

# Every check. The check workflow runs the same recipe on each push.
check: fmt-check lint deny shear

# Rewrite files in place.
fmt:
    cargo fmt --all
    taplo fmt
    biome check --write
    rumdl fmt

fmt-check:
    cargo fmt --all -- --check
    taplo fmt --check
    biome check
    rumdl check

lint:
    typos
    cargo clippy --workspace --all-targets --locked -- -D warnings

deny:
    cargo deny --locked check

shear:
    cargo shear

# Install the git hooks once per clone.
hooks:
    lefthook install

# Run the tray app in dev mode.
dev:
    cd crates/app && cargo tauri dev

# The self-signed identity that signs the Mac app, here and in the release workflow.
signing-identity := "Meeting Capture"

# Create the signing identity in the login keychain, once. Keep the .p12 it writes.
[macos]
signing-cert:
    mkdir -p ~/.tauri
    scripts/create-signing-cert.sh "{{signing-identity}}" ~/.tauri/meeting-capture-signing.p12

# Check, then build the release files. Run `just signing-cert` once first.
[macos]
release: check bundle

# Build the signed DMG, the signed updater package and latest.json into target/dist/. The release
# workflow sets the updater key variables; locally they default to the files in ~/.tauri.
[macos]
bundle:
    #!/bin/sh
    set -eu
    export TAURI_SIGNING_PRIVATE_KEY="${TAURI_SIGNING_PRIVATE_KEY:-$(cat ~/.tauri/meeting-capture-updater.key)}"
    export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-$(cat ~/.tauri/meeting-capture-updater.key.password)}"
    version=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "meeting-capture") | .version')
    (cd crates/app && APPLE_SIGNING_IDENTITY="{{signing-identity}}" cargo tauri build --bundles app,dmg)
    bundle=target/release/bundle
    rm -rf target/dist && mkdir -p target/dist
    # GitHub turns spaces in asset names into dots, which would break the checksum file.
    dmg="Meeting-Capture_${version}_aarch64.dmg"
    package="Meeting-Capture_${version}_aarch64.app.tar.gz"
    cp "$bundle/dmg/Meeting Capture_${version}_aarch64.dmg" "target/dist/$dmg"
    cp "$bundle/macos/Meeting Capture.app.tar.gz" "target/dist/$package"
    cp "$bundle/macos/Meeting Capture.app.tar.gz.sig" "target/dist/$package.sig"
    scripts/updater-manifest.sh "$version" "target/dist/$package" > target/dist/latest.json
    (cd target/dist && shasum -a 256 "$dmg" > "$dmg.sha256")
    # The DMG holds the app. A second copy left in target/ shows up in Spotlight and Launch Services.
    rm -rf "$bundle/macos"
