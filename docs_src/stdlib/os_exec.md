# `std::os::exec`

Deprecated compatibility facade for child processes; new code uses std::process.

## Items

| Item | Signature | Description |
|---|---|---|
| `Path` | `type Path` | Immutable UTF-8 lexical path value with value-returning operations. |
| `Child` | `type Child` | Handle to a still-running child supporting wait / kill. |
| `Pipeline` | `type Pipeline` | Multi-stage subprocess pipeline (stdout-to-stdin chain). |
| `Signal` | `type Signal` | Portable signal selector (Term/Kill/Stop/Cont/Hup/Int/Usr1/Usr2/Pipe/Quit). |
| `run` | `fn run(program: String, args: Vec<String>) -> Result<process::Output, errors::Error>` | One-shot: runs a program with args, captures stdout/stderr, returns Result<{stdout, stderr, code}, String>. |
| `spawn` | `fn spawn(program: String, args: Vec<String>) -> Result<i64, errors::Error>` | Non-blocking launch; returns the child PID as Result<i64, errors::Error>. |
| `spawn_piped` | `fn spawn_piped(program: String, args: Vec<String>) -> Result<process::Child, errors::Error>` | Spawns a child with piped stdin/stdout; returns Result<Child, errors::Error>. The Child's write_stdin / close_stdin / read_line / read_stdout / wait / kill methods drive it interactively. |
| `kill` | `fn kill(pid: i64) -> bool` | Best-effort SIGTERM by pid; returns true on success. |
| `signal` | `fn signal(pid: i64, signum: i64) -> bool` | Send an arbitrary signal number to a pid; returns true on success. |
| `kill_group` | `fn kill_group(pid: i64) -> bool` | Send SIGTERM to the entire process group (Unix); best-effort TerminateProcess on Windows. |
| `wait_timeout` | `fn wait_timeout(pid: i64, ms: i64) -> i64` | Wait up to N ms for a pid to exit; returns exit code, -1 on timeout, -2 on error. |
| `pipeline_run` | `fn pipeline_run(commands: Vec<String>) -> Result<process::Output, errors::Error>` | Run a Vec<String> of shell-tokenised commands as a stdout-to-stdin pipeline. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
