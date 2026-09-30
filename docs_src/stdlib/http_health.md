# `std::http::health`

Status: experimental

Liveness and readiness endpoints are ordinary handlers over `std::lifecycle`: answer 200 from a liveness route, and 200/503 from `lifecycle::is_ready()` on a readiness route, which drops to false on its own when shutdown begins. A probe registry with per-check timeouts belongs in an application package.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
