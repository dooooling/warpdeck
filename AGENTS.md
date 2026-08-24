# AGENTS.md

## Project state

- **v0.1 released; feature development ongoing** (single `main` + short-lived branches):
  - `DESIGN_AND_DEVELOPMENT.md` — architecture, models, API, security (the *what/why*)
  - `DEVELOPMENT_PLAN.md` — phased plan P0–P12 (the *how/sequence*), all phases complete
- Docs are written in Chinese; keep them in sync when design changes (doc change → plan update, per plan §1).
- Build/test entry points are unified under `cargo xtask` (see Commands below). Do not reintroduce ad-hoc scripts.

## Fixed design baselines (non-negotiable)

- **MVP protocol scope**: SOCKS5 + HTTP only. No Direct Proxy, no Shadowsocks — no placeholder fields, listeners, or UI toggles for them.
- **Fixed ports**: Web/API `9000`, SOCKS5 `11080`, HTTP `18080` in-container. WARP instance internal ports `40000 + instance_id`, loopback only — **never publish internal `40000+` ports to the Docker host**.
- Host port mapping is owned by Compose `.env`; the Web UI/API must not modify it.
- Architecture: SQLite holds **desired state**; a Runtime Registry holds **actual state**; a **Reconciler** loop converges the two. HTTP handlers only mutate desired state and notify — never act as supervisor.
- API must never expose arbitrary command execution (`warp-cli ...` passthrough is forbidden); only explicit domain actions like `POST /api/v1/instances/:id/restart`.
- Secrets (WARP+ license, Zero Trust creds, proxy password) are encrypted at rest (XChaCha20-Poly1305, master key from `WARPDECK_MASTER_KEY` env or `master.key` 0600), never returned plaintext by GET, never logged (central redactor). Passwords use Argon2id — never SHA256(password)/MD5/SHA1.
- One instance = one independent subset of: state dir, `/run` runtime dir, D-Bus socket, port, PID. Instance failure must not pollute others.

## Development discipline

- **Never use repeated `docker build` as the dev/test loop.** Normal work: `cargo run`/`cargo test` + fixed dev-base image `warpdeck-dev-base:1` (bind-mount the built binary) + fake runtimes. Docker E2E (`docker build -t warpdeck:e2e .`) only for candidate integrations.
- Never run `docker system prune -a --volumes` in test scripts.
- PR CI always builds the image and runs the E2E-01 smoke (single-instance closed loop, 1 WARP registration). Full E2E matrix (1..=8) additionally triggers on path changes: `Dockerfile*`, `docker/**`, `compose*.yml`, WARP install, listener/bootstrap code. Rationale: the image is the shipping artifact; registration frequency vs Cloudflare is the only reason the full matrix is not run on every PR.
- Domain code must not hardcode `Command::new("warp-cli")`: go through `WarpControl` trait + `ProcessSpawner`/`Clock`/`BackoffPolicy` traits with Fake implementations, so ≥80% of tests run without real WARP.
- Order: single instance lifecycle first, then multi-instance, then health, then GOST, then persistence/reconciler, then API/auth/UI. (2026-08-24: GOST retired — builtin in-process gateway is the only proxy path, DESIGN §35 / P13.)
- `warp-cli` commands: `Command::new` + `.arg` (no shell string concat), timeout, capture stderr, typed errors. Port calculation centralized via typed `InstanceId`/`InternalProxyPort` with `u16` overflow checks.

## Commands (once code exists)

```bash
# Backend (workspace under crates/)
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace

# Build entry points (replaced scripts/*.ps1 orchestration layer; 2026-08-21)
# WARP deps download in-image via docker/fetch-deps.sh (cache mount + forced sha256),
# single source = crates/xtask/src/versions.json (Dockerfiles COPY it from the build
# context and parse with jq — no --build-arg copies); CN network adds --proxy socks5h://host.docker.internal:10808
cargo xtask release                 # release image (default tag warpdeck:local)
cargo xtask dev-base                # runtime dev image warpdeck-dev-base:1 (rare)
cargo xtask in-container            # compile Linux ELF -> target/linux-artifacts/
cargo xtask check-linux --test      # Linux-side clippy+test; run BEFORE pushing to prevent platform drift
cargo xtask smoke-dev-base --full   # dev-base component + data-plane smoke (warp=on)
cargo xtask backup | restore --archive A | backups   # data-volume backup/restore (compose stop window)
cargo xtask e2e                     # E2E matrix 1..=8 vs warpdeck:e2e (--only 2,3 subset)

# Frontend (web/)
cd web && pnpm install && pnpm lint && pnpm typecheck && pnpm test
# dev servers: cargo run / pnpm dev

# Real WARP smoke (data plane)
curl --socks5-hostname 127.0.0.1:11080 https://cloudflare.com/cdn-cgi/trace   # expect warp=on
curl -x http://127.0.0.1:18080 https://cloudflare.com/cdn-cgi/trace           # expect warp=on
```

Health states: `Healthy` (requires real data-plane probe with `warp=on`, not just PID alive), `Degraded` (transient), `Failed` (consecutive threshold). The builtin gateway (P13-C, sole proxy path) applies config in-process (hot rebuild, supervised restart on panic); apply failure must be surfaced to the UI, never faked as success.