# `std::http::state`

Status: experimental

Dependency injection is closure capture: build the router from closures that capture the pool, the cache, and the configuration, and each handler reads what it captured. A captured heap value is shared, so one map serves every request.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
