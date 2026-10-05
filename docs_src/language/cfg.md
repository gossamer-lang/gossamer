# `lang::cfg`

Conditional compilation attribute, resolved for the target a build produces (`gos build --target`, `gos check --target`) rather than the host; `feature = "name"` reads the declaring package's `[features]`, and `#[cfg]` applies to a `use` too.
