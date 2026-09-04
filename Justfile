default:
    @just --list

fmt:
    cargo fmt --check

test:
    cargo test

clippy:
    cargo clippy --all-targets -- -D warnings

# Deterministic mechanical conventions (no Unicode dashes, etc.) over the whole
# tree. Install once: see CONTRIBUTING.md. Also runs as an agent-time hook and in CI.
nudge:
    nudge check

check: fmt test clippy nudge

build:
    cargo build --release

# Run the reviewers triggered by the working tree with automatic PR base selection.
review:
    cargo run -- review

# Run the reviewers against an explicit base branch.
review-base base:
    cargo run -- review --base {{quote(base)}}

version:
    cargo run -- --version
