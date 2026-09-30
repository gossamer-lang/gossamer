# `std::metrics`

Status: experimental

Prometheus-compatible primitives (Counter, Gauge, Histogram) and a Registry rendering the standard text-exposition format.

## Items

| Item | Signature | Description |
|---|---|---|
| `Counter` | `type Counter` | Monotonic-increasing u64 counter (lock-free). |
| `Gauge` | `type Gauge` | Set / inc / dec gauge (lock-free). |
| `Histogram` | `type Histogram` | Bucketed observation histogram with sum and count. |
| `Metric` | `type Metric` | Enum holding any of the three primitives for registry storage. |
| `Registry` | `type Registry` | Ordered collection of metrics; `render()` emits the Prometheus text-exposition format. |
| `serve_metrics` | `fn serve_metrics(addr: String) -> Result<(), errors::Error>` | Mounts a registry on `/metrics` over the existing http server loop. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
