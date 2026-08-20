# WarpDeck unified command entry.
# NOTE: these recipes must NEVER trigger a Docker build.
# Docker is reserved for real-WARP runtime, Docker E2E, packaging and release.

check:
    cargo fmt --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace

check-all: check frontend-check

backend-test:
    cargo test --workspace

frontend-check:
    cd web && pnpm lint
    cd web && pnpm typecheck
    cd web && pnpm test

frontend-e2e:
    cd web && pnpm test:e2e

dev:
    cargo run

build-dev:
    cargo build

test-fake-runtime:
    cargo test --test runtime_fake

# Real WARP dev loop: build binary locally (Linux/WSL2), bind-mount into dev-base.
warp-real-restart:
    docker compose -f compose.dev.yml restart warp-dev

# E2E candidate image: only for candidate integrations, one build per round.
docker-e2e:
    docker build -t warpdeck:e2e .
    docker compose -f compose.e2e.yml up -d

fmt:
    cargo fmt
    cd web && pnpm lint --fix
