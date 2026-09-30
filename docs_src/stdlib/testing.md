# `std::testing`

Status: experimental

Assertions and sub-test harness helpers.

## Items

| Item | Signature | Description |
|---|---|---|
| `Runner` | `type Runner` | Sub-test collector. |
| `check` | `fn check(cond: bool, message: String) -> Result<(), errors::Error>` | Asserts a condition. |
| `check_eq` | `fn check_eq<T: Debug + Eq>(left: T, right: T, message: String) -> Result<(), errors::Error>` | Asserts equality, rendering a diff on failure. |
| `check_ok` | `fn check_ok<T, E: Debug>(result: Result<T, E>, message: String) -> Result<T, errors::Error>` | Asserts a Result is Ok, recording without panicking. |
| `wait_for_scheduler_idle` | `fn wait_for_scheduler_idle(timeout_ms: i64) -> bool` | Waits for the scheduler to become idle within a timeout. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
