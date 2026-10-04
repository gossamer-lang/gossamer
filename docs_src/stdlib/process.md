# `std::process`

Canonical process control and child-process API; std::os::exec is compatibility-only.

## Items

| Item | Signature | Description |
|---|---|---|
| `Child` | `type Child` | Handle to a still-running child supporting wait / kill. |
| `run` | `fn run(program: String, args: Vec<String>) -> Result<process::Output, errors::Error>` | One-shot: runs a program with args, captures stdout/stderr plus the exit code. |
| `run_in` | `fn run_in(program: String, args: Vec<String>, dir: String, env: Vec<(String, String)>) -> Result<process::Output, errors::Error>` | run_in(program, args, dir, env): the same one-shot run with the child's working directory and environment supplied. An empty dir inherits the caller's; the env pairs override the inherited environment rather than replacing it, so a caller sets the two variables it cares about without restating PATH. |
| `run_inherit` | `fn run_inherit(program: String, args: Vec<String>) -> Result<i64, errors::Error>` | `run_inherit(program, args) -> Result<i64, errors::Error>`: runs the program on this process's own standard input, output, and error - a terminal included - and answers its exit code once it ends (128 plus the signal number for one a signal ended on Unix). The goroutine parks meanwhile. Go's `exec.Cmd.Run` with `os.Stdin`, `os.Stdout`, and `os.Stderr` attached. |
| `spawn` | `fn spawn(program: String, args: Vec<String>) -> Result<i64, errors::Error>` | Spawns a child process and returns its PID. |
| `spawn_piped` | `fn spawn_piped(program: String, args: Vec<String>) -> Result<process::Child, errors::Error>` | Spawns a child with piped stdin/stdout; returns Result<Child, errors::Error>. The Child's write_stdin / close_stdin / read_line / read_stdout / wait / kill methods drive it interactively. |
| `kill` | `fn kill(pid: i64) -> bool` | Sends SIGKILL (or equivalent) to a Child. |
| `exit` | `fn exit(code: i64) -> !` | Exits the current process with the given status code. |
| `id` | `fn id() -> i64` | Returns the current process ID. |
| `abort` | `fn abort() -> !` | Aborts the current process without unwinding. |
| `signal` | `fn signal(pid: i64, signum: i64) -> bool` | Sends a signal to a process by PID (POSIX). |
| `kill_group` | `fn kill_group(pid: i64) -> bool` | Sends a signal to a process group (POSIX). |
| `wait_timeout` | `fn wait_timeout(pid: i64, ms: i64) -> i64` | Waits for a child with a timeout (POSIX). |
| `pipeline_run` | `fn pipeline_run(commands: Vec<String>) -> Result<process::Output, errors::Error>` | Runs a shell-tokenised pipeline and returns captured stdout/stderr plus the final exit code. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
