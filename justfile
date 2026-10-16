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
