set windows-shell := ["pwsh.exe", "-NoLogo", "-Command"]

# Every local check. No CI until phase 8, so this is the gate.
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

# The self-signed identity that signs the Mac app.
signing-identity := "Meeting Capture Dev"

# Create the signing identity in the login keychain, once per Mac.
[macos]
signing-cert:
    scripts/create-signing-cert.sh "{{signing-identity}}"

# Build the signed DMG and its checksum into target/dist/. Run `just signing-cert` once first.
[macos]
release: check
    cd crates/app && APPLE_SIGNING_IDENTITY="{{signing-identity}}" cargo tauri build --bundles app,dmg
    rm -rf target/dist && mkdir -p target/dist
    # GitHub turns spaces in asset names into dots, which would break the checksum file.
    for f in target/release/bundle/dmg/*.dmg; do cp "$f" "target/dist/$(basename "$f" | tr ' ' '-')"; done
    cd target/dist && for f in *.dmg; do shasum -a 256 "$f" > "$f.sha256"; done
    # The DMG holds the app. A second copy left in target/ shows up in Spotlight and Launch Services.
    rm -rf target/release/bundle/macos
