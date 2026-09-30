# `std::trace`

Status: experimental

W3C trace-context-compatible distributed tracing. Identifier types, request-scoped SpanContext, process-level Tracer, and OTLP JSON export.

## Items

| Item | Signature | Description |
|---|---|---|
| `TraceId` | `type TraceId` | 128-bit trace identifier (W3C trace-context format). |
| `SpanId` | `type SpanId` | 64-bit span identifier. |
| `SpanContext` | `type SpanContext` | Request-scoped trace + span pair, propagated through `std::context`. |
| `SpanStatus` | `type SpanStatus` | Span outcome: Unset / Ok / Error(message). |
| `Span` | `type Span` | Active span builder. Attributes, events, status; `end()` finalises and records. |
| `EndedSpan` | `type EndedSpan` | Finalised span record; `to_otlp_json()` serialises for OTLP/HTTP export. |
| `Tracer` | `type Tracer` | Process-level span sink. `start_span`, `ended_spans`, `set_global`. |
| `SpanGuard` | `type SpanGuard` | RAII guard returned by `enter_span`; restores the prior active span on drop. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
