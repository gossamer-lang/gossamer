# Deployment guide

This page walks through building a Gossamer service, shipping the
binary to a Linux server, and supervising it under `systemd`.

The story is intentionally boring: Gossamer compiles to a single
static (or near-static) ELF / Mach-O / PE binary. There is no
JVM, no interpreter shim, no separate runtime to install on the
target. If your CI can produce a Linux x86_64 binary on a Linux
x86_64 runner, you can `scp` it and run it.

## Targets

Pre-built `gos` toolchain binaries ship for:

| Triple | Notes |
|---|---|
| `x86_64-unknown-linux-gnu` | Tier 1 Linux server target. |
| `aarch64-unknown-linux-gnu` | Tier 1 ARM64 Linux server target. |
| `x86_64-apple-darwin` | Artifact-only; no all-tier execution evidence yet. |
| `aarch64-apple-darwin` | Tier 1 Apple Silicon development target. |
| `x86_64-pc-windows-msvc` | Tier 1 Windows server target. |

Compiled programs default to the host triple. The supported cross-ISA path is
Linux-musl AOT output for `{x86_64,aarch64}-unknown-linux-musl`: CI executes
those binaries natively or under QEMU and compares them with the pure bytecode
VM. Cross-host glibc links can be configured with an external sysroot but are
not part of the supported contract. Cross-compiling *to* macOS or Windows as a
target remains out of scope (needs external SDKs). The release matrix in
[`.github/workflows/release.yml`](https://github.com/gossamer-lang/gossamer/blob/main/.github/workflows/release.yml)
is the source of truth for what we test on.

## Building per target

You can either build each target on a native runner of that architecture, or
cross-compile every Linux target from a single host with `--target`. The
release workflow builds one job per target; cross-compilation side-steps the
need for a runner per architecture when a Linux binary is all you need.

On a Linux x86_64 host the deployable artifact is simply:

```sh
gos build --release src/main.gos
```

With the `x86_64-unknown-linux-musl` rustup target installed this is a
fully-static single file - ideal for `scratch` / `distroless/static`
images. Without it, the build falls back to a dynamically-linked glibc
binary, which still ships fine on a glibc base image (`--dynamic`
forces that path explicitly).

## Container images

For services we recommend a `distroless`-based image. A glibc
`gos build --release` binary needs only its libc - use a
`distroless/base` runtime. A fully-static musl binary (built with the
musl rustup target in the build stage) needs nothing and can run on
`scratch` or `distroless/static`. Sample `Dockerfile` for the glibc
path:

```dockerfile
# Build stage
FROM debian:bookworm-slim AS build
RUN apt-get update && apt-get install -y curl ca-certificates build-essential
RUN curl -fsSL https://github.com/gossamer-lang/gossamer/releases/latest/download/gos-x86_64-unknown-linux-musl -o /usr/local/bin/gos && chmod +x /usr/local/bin/gos
WORKDIR /src
COPY . .
RUN gos build --release src/main.gos --out-dir /out

# Runtime stage
FROM gcr.io/distroless/base-debian12:nonroot
COPY --from=build /out/main /server
USER nonroot:nonroot
EXPOSE 8080
ENTRYPOINT ["/server"]
```

Image sizes settle around 20-30 MiB for a typical HTTP service (smaller
on `scratch` with a static musl binary).

## Process supervision: systemd

Drop a unit file at `/etc/systemd/system/myservice.service`:

```ini
[Unit]
Description=My Gossamer service
After=network.target
Documentation=https://example.com/myservice

[Service]
Type=simple
User=myservice
Group=myservice
ExecStart=/usr/local/bin/myservice
Restart=on-failure
RestartSec=5s

# Environment
Environment="GOSSAMER_LOG=info"
Environment="LISTEN_ADDR=0.0.0.0:8080"

# Hardening
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictNamespaces=yes
RestrictRealtime=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallArchitectures=native
SystemCallFilter=@system-service

# Tuning
LimitNOFILE=65536
TasksMax=4096

[Install]
WantedBy=multi-user.target
```

Reload + start:

```sh
systemctl daemon-reload
systemctl enable --now myservice
journalctl -u myservice -f
```

### Graceful shutdown

Gossamer services should handle SIGTERM (sent by `systemctl
stop`) so in-flight requests finish before the process exits.
An in-process signal API is planned (v1.x). Today, drive graceful
shutdown from the supervisor: with systemd, set `KillSignal=SIGTERM`
and `TimeoutStopSec=30s` in the unit file so in-flight requests have
time to finish before the process is forced down.

```ini
[Service]
KillSignal=SIGTERM
TimeoutStopSec=30s
```

systemd escalates to `SIGKILL` only after the timeout.

## Log shipping

Gossamer services log to stdout / stderr by default. systemd
captures both into the journal; ship the journal to your
log aggregator. For structured logs, use `std::slog::JsonHandler`
so the lines are JSON-line-formatted:

```gos
use std::slog

fn main() {
    slog::info("listening", "addr", "0.0.0.0:8080")
    // ...
}
```

For shipping to a log aggregator that doesn't read the journal:

- **Loki**: `promtail` watches stdout via the systemd journal
  driver.
- **Cloudwatch**: `awslogs` agent reads `/var/log/syslog` and
  ships journal-tagged messages.
- **Stdout to network**: write to `/dev/stdout`; let your
  runtime forwarder handle it. Common in Kubernetes setups.

## Tuning

### GOMAXPROCS-equivalent

Gossamer reads the OS CPU count at startup and runs that many
scheduler threads. Override with `GOSSAMER_MAX_PROCS`:

```sh
GOSSAMER_MAX_PROCS=4 ./myservice
```

Set this in the systemd unit's `Environment=` line.

### Stack size per goroutine

Goroutines are stackful coroutines multiplexed M:N onto the worker
thread pool, not OS threads - a blocked goroutine costs its stack of
mmap'd address space, not a thread. Each goroutine reserves 1 MiB by default;
pages are committed as they are touched. Tune the reservation with
`GOSSAMER_GOROUTINE_STACK` (minimum 32 KiB):

```sh
GOSSAMER_GOROUTINE_STACK=2097152 ./myservice  # 2 MiB stacks
```

Idle goroutines are parked on the netpoller and consume constant
memory. The worker-thread count (not the goroutine count) follows
`GOSSAMER_MAX_PROCS`.

### Memory

Compiled tiers reclaim acyclic values with deterministic reference counting.
Their thread-local cycle collector runs under allocation pressure and on demand
through `runtime::collect_cycles()`. Objects shared across goroutines are
excluded from that collector, and the bytecode VM does not collect cycles.
Use `Weak<T>` to break cycles when cross-tier reclamation matters. `arena { }`
regions free short-lived graphs wholesale.

There is no tracing collector or GC tuning knob. Resident memory is still
affected by allocator retention, fixed stack reservations, uncollectable
cycles, JIT code, and caches, so size container limits from measured service
peaks rather than assuming RSS immediately follows the live object graph.

## Health check / readiness

A typical HTTP service exposes `/healthz`:

<!-- fragment -->
```gos
fn handler(req: http::Request) -> http::Response {
    match req.path() {
        "/healthz" => http::Response::text(200, "ok"),
        _ => app_handler(req),
    }
}
```

Wire this into the load balancer's readiness probe. systemd has
no native HTTP probe; use `Type=notify` and a small `sd_notify`
shim, or rely on the load balancer.

## Updates / zero-downtime deploys

The current recommended pattern is rolling restarts behind a
load balancer:

1. Deploy new binary to half the fleet.
2. Wait for healthchecks to pass.
3. Drain old half.
4. Repeat.

In-place hot-swap (`SIGUSR2` exec-the-new-binary-without-dropping-listeners)
is not in v1; `os::exec` and `os::signal` are available building
blocks, but listener handoff and supervisor coordination still need a
dedicated runtime pattern.

## Observability

A production Gossamer service ships with three diagnostic
surfaces that match Go's. Configure as:

### `SIGQUIT` goroutine dump

Sending `SIGQUIT` (or pressing Ctrl-\\ on a foreground process)
prints every live goroutine's last-known frame to stderr, then
exits. The handler is installed automatically on first scheduler
start. Output format mirrors Go's so existing tools
(`stackparse`, log-shipping rules) read it unchanged.

```text
SIGQUIT: dumping 1342 goroutine(s)

goroutine 17 [chan receive]:
  main::handle_request()
        src/main.gos:128
  ...
```

### pprof endpoint (planned)

A `std::pprof` module to mount `/debug/pprof/*` is planned for v1.x.
The intended shape routes the request path and query through
`pprof::route` and returns the profile bytes:

```text
fn pprof_handler(req: &http::Request) -> http::Response {
    match pprof::route(req.path(), req.query()) {
        Some(bytes) => http::Response::json(200, bytes),
        None        => http::Response::text(404, "not found"),
    }
}
```

Then:

```sh
go tool pprof -text http://localhost:8080/debug/pprof/profile
go tool pprof -web  http://localhost:8080/debug/pprof/heap
go tool pprof       http://localhost:8080/debug/pprof/goroutine
```

The legacy text profile format is what Gossamer emits today;
the protobuf-encoded variant lands in Phase 2.

### `gos test --race`

Runs the test suite under the data-race detector. Catches
unsynchronised concurrent writes via vector-clock happens-before
analysis seeded from the scheduler's park/unpark events. CI
gate:

```sh
gos test --race                  # exits non-zero on first race
gos test --race --coverage cov.lcov
```

### Reproducible builds + supply-chain

Production builds:

```sh
gos build --release -g --reproducible
```

The `--reproducible` flag pins `SOURCE_DATE_EPOCH` and the
LLVM tmp-dir layout so two builds of the same source on the
same target produce bit-identical artifacts. Releases shipped
through `.github/workflows/release.yml` are cosign-signed
(keyless / OIDC), carry a SLSA-3 build-provenance attestation,
and ship alongside a CycloneDX SBOM.

## Cross-references

- [`stdlib.md`](stdlib.md) - `slog`, `http`, `os`.
