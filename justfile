# Development tasks for both crates: the library (this directory, checked by
# .github/workflows/ci.yml) and the command-line crate in rfc2217-cli/
# (ci-cli.yml). The CLI is a workspace of its own, so `cargo -p` cannot reach
# it from here; its recipes pass its manifest instead.
#
# The recipes run what CI runs; the workflows remain the reference. `just`
# lists them, `just check` is the quick loop, `just ci` runs everything.

cli_manifest := "--manifest-path rfc2217-cli/Cargo.toml"

# List the recipes.
default:
    @just --list

# Format check, clippy and tests for both crates.
check: lint test cli-lint cli-test

# Everything CI checks, for both crates. Needs the 1.85 and nightly toolchains.
ci: lint docs docs-rs test msrv no-std minimal-versions package cli-lint cli-cross cli-test cli-msrv cli-package

# Format both crates and the firmware stub.
fmt:
    cargo fmt
    cargo fmt --manifest-path ci/fw-stub/Cargo.toml
    cargo fmt {{ cli_manifest }}

# The tests, with the default, all and no features.
[group('library')]
test:
    cargo test
    cargo test --all-features
    cargo test --no-default-features

# The tests on the minimum supported Rust version.
[group('library')]
msrv:
    cargo +1.85 test
    cargo +1.85 test --all-features
    cargo +1.85 test --no-default-features

# Clippy for each feature set, and the format check.
[group('library')]
lint:
    cargo clippy --all-targets -- -D warnings
    cargo clippy --all-targets --all-features -- -D warnings
    cargo clippy --all-targets --no-default-features -- -D warnings
    cargo fmt --check
    cargo fmt --check --manifest-path ci/fw-stub/Cargo.toml

# The API docs, with all and no features.
[group('library')]
docs:
    cargo doc --no-deps --all-features
    cargo doc --no-deps --no-default-features

# The API docs as docs.rs builds them.
[group('library')]
docs-rs:
    RUSTDOCFLAGS="--cfg docsrs" cargo +nightly doc --no-deps --all-features

# A microcontroller build, and a firmware stub that must link without an allocator.
[group('library')]
no-std:
    rustup target add thumbv6m-none-eabi
    cargo build --lib --target thumbv6m-none-eabi --no-default-features
    cargo build --lib --target thumbv6m-none-eabi
    cargo build --release --target thumbv6m-none-eabi --manifest-path ci/fw-stub/Cargo.toml

# The tests with the lowest dependency versions Cargo.toml allows. Cargo.lock is put back afterwards.
[group('library')]
minimal-versions:
    #!/usr/bin/env bash
    set -euo pipefail
    saved="$(mktemp)"
    cp Cargo.lock "$saved"
    trap 'cp "$saved" Cargo.lock; rm -f "$saved"' EXIT
    cargo +nightly update -Z direct-minimal-versions
    # A target directory of its own keeps the usual build.
    CARGO_TARGET_DIR=target/minimal-versions cargo +1.85 test --all-features

# The crate as it would be published (uncommitted changes included).
[group('library')]
package:
    cargo publish --dry-run --allow-dirty

# Run the CLI from source, e.g. `just cli connect bridge.local -b 115200`. Relative paths are relative to where you are.
[group('cli')]
[no-cd]
[positional-arguments]
cli *args:
    @cargo run --quiet --manifest-path "{{ justfile_directory() }}/rfc2217-cli/Cargo.toml" -- "$@"

# The CLI's tests.
[group('cli')]
cli-test:
    cargo test --locked {{ cli_manifest }}

# The CLI's tests on the minimum supported Rust version.
[group('cli')]
cli-msrv:
    cargo +1.85 test --locked {{ cli_manifest }}

# Clippy and the format check for the CLI.
[group('cli')]
cli-lint:
    cargo clippy --all-targets --locked {{ cli_manifest }} -- -D warnings
    cargo fmt --check {{ cli_manifest }}

# Clippy as the Windows and macOS runners see the code.
[group('cli')]
cli-cross:
    rustup target add x86_64-pc-windows-gnu x86_64-apple-darwin
    cargo clippy --all-targets --locked {{ cli_manifest }} --target x86_64-pc-windows-gnu -- -D warnings
    cargo clippy --all-targets --locked {{ cli_manifest }} --target x86_64-apple-darwin -- -D warnings

# The CLI packaged and built as crates.io users get it, against rfc2217 from crates.io (uncommitted changes included).
[group('cli')]
cli-package:
    cargo package --locked --allow-dirty {{ cli_manifest }}
