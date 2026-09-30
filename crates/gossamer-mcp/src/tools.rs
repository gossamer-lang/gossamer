//! MCP tool table: schemas, listing, and call dispatch.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gossamer_std::json::{self, Value};

use crate::ServerConfig;
use crate::exec::{self, ExecOutcome};
use crate::nav::NavSession;
use crate::protocol::{field, field_str, obj, response_err, response_ok, s};

/// Default bound on `execute` / `build` / `test` subprocess time.
const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// One argument in a tool's input schema.
struct Arg {
    name: &'static str,
    ty: &'static str,
    description: &'static str,
    required: bool,
}

struct Tool {
    name: &'static str,
    description: &'static str,
    args: &'static [Arg],
}

const TIMEOUT_ARG: Arg = Arg {
    name: "timeout_ms",
    ty: "integer",
    description: "Kill the process after this many milliseconds (default 120000).",
    required: false,
};

const SOURCE_ARG: Arg = Arg {
    name: "source",
    ty: "string",
    description: "Gossamer source text to use instead of `file`. Written to a \
                  temporary .gos file for the run and removed afterwards, so a \
                  snippet needs no file of its own.",
    required: false,
};

const POSITION_ARGS: &[Arg] = &[
    Arg {
        name: "file",
        ty: "string",
        description: "Path to a Gossamer source file.",
        required: true,
    },
    Arg {
        name: "line",
        ty: "integer",
        description: "1-based line number.",
        required: true,
    },
    Arg {
        name: "column",
        ty: "integer",
        description: "1-based column number.",
        required: true,
    },
];

const TOOLS: &[Tool] = &[
    Tool {
        name: "check",
        description: "Parse + resolve + typecheck + exhaustiveness + arena-escape + lints \
                      for a Gossamer file or project. `structuredContent.diagnostics` holds \
                      one parsed object per diagnostic (stable schema); an empty array means \
                      no findings. With fix=true, applies the rewrites the diagnostics carry \
                      and reports what remains.",
        args: &[
            Arg {
                name: "file",
                ty: "string",
                description: "A .gos file or directory; defaults to the project's src/.",
                required: false,
            },
            SOURCE_ARG,
            Arg {
                name: "fix",
                ty: "boolean",
                description: "Apply every machine-applicable suggestion, keeping only edits a \
                              re-check proves better; inline `source` comes back rewritten.",
                required: false,
            },
        ],
    },
    Tool {
        name: "explain",
        description: "Long-form explanation for a Gossamer diagnostic code (GT0017, \
                      GL0001, ...).",
        args: &[Arg {
            name: "code",
            ty: "string",
            description: "The diagnostic code to look up.",
            required: true,
        }],
    },
    Tool {
        name: "execute",
        description: "Execute a Gossamer program on the bytecode VM (with JIT). Returns \
                      exit code, stdout, and stderr.",
        args: &[
            Arg {
                name: "file",
                ty: "string",
                description: "Entry source file, with any filename extension, or project directory.",
                required: false,
            },
            SOURCE_ARG,
            Arg {
                name: "args",
                ty: "array",
                description: "Arguments forwarded to the program.",
                required: false,
            },
            TIMEOUT_ARG,
        ],
    },
    Tool {
        name: "build",
        description: "Compile a Gossamer program to a native executable via LLVM.",
        args: &[
            Arg {
                name: "file",
                ty: "string",
                description: "Entry .gos file; defaults to the project's entry.",
                required: false,
            },
            Arg {
                name: "release",
                ty: "boolean",
                description: "Full LLVM -O3 pipeline instead of the fast unoptimised build.",
                required: false,
            },
            TIMEOUT_ARG,
        ],
    },
    Tool {
        name: "test",
        description: "Run #[test] functions. Returns the test report.",
        args: &[
            Arg {
                name: "path",
                ty: "string",
                description: "A .gos file or project directory; defaults to the project.",
                required: false,
            },
            Arg {
                name: "filter",
                ty: "string",
                description: "Only run tests whose name contains this substring.",
                required: false,
            },
            TIMEOUT_ARG,
        ],
    },
    Tool {
        name: "fmt",
        description: "Format a Gossamer source file in place, or report formatting drift \
                      with check=true.",
        args: &[
            Arg {
                name: "file",
                ty: "string",
                description: "Path to a .gos source file.",
                required: false,
            },
            SOURCE_ARG,
            Arg {
                name: "check",
                ty: "boolean",
                description: "Report instead of rewriting; nonzero exit when unformatted.",
                required: false,
            },
        ],
    },
    Tool {
        name: "doc",
        description: "Item listing plus doc comments for a Gossamer source file, or the \
                      standard library: `std` lists every module, `std::strings` a \
                      module's exports, `std::strings::trim` one item.",
        args: &[
            Arg {
                name: "file",
                ty: "string",
                description: "Path to a .gos source file, or a `std`-rooted query.",
                required: false,
            },
            SOURCE_ARG,
        ],
    },
    Tool {
        name: "lint",
        description: "Run the built-in lint suite. With fix=true, applies every \
                      auto-fixable suggestion and writes the file back.",
        args: &[
            Arg {
                name: "path",
                ty: "string",
                description: "A .gos file or directory; defaults to the project's src/.",
                required: false,
            },
            SOURCE_ARG,
            Arg {
                name: "fix",
                ty: "boolean",
                description: "Apply auto-fixable suggestions instead of only reporting.",
                required: false,
            },
            Arg {
                name: "deny_warnings",
                ty: "boolean",
                description: "Promote every lint hit to an error.",
                required: false,
            },
        ],
    },
    Tool {
        name: "fix",
        description: "Apply the toolchain's source migrations - mechanical upgrades the \
                      toolchain owns, distinct from lint fixes. Every rewrite is re-checked \
                      before it is kept. Use list=true to see what migrations exist.",
        args: &[
            Arg {
                name: "path",
                ty: "string",
                description: "A .gos file or directory; defaults to the project's src/.",
                required: false,
            },
            SOURCE_ARG,
            Arg {
                name: "rewriter",
                ty: "string",
                description: "Apply only this rewriter id. Defaults to all of them.",
                required: false,
            },
            Arg {
                name: "list",
                ty: "boolean",
                description: "List the available rewriters instead of applying any.",
                required: false,
            },
            Arg {
                name: "check",
                ty: "boolean",
                description: "Report pending migrations without writing.",
                required: false,
            },
        ],
    },
    Tool {
        name: "audit",
        description: "Report security advisories this project can reach. Filtered by \
                      reachability - an advisory naming an item the project never \
                      references is not actionable - with all=true to lift the filter. \
                      Emits the same diagnostic schema as `check`.",
        args: &[
            Arg {
                name: "path",
                ty: "string",
                description: "Path inside the project; defaults to the project's src/.",
                required: false,
            },
            Arg {
                name: "all",
                ty: "boolean",
                description: "Report every advisory affecting a resolved version.",
                required: false,
            },
        ],
    },
    Tool {
        name: "feature_status",
        description: "What is actually known about every language feature and stdlib item: \
                      a lifecycle status derived from evidence, not declared, plus the \
                      tier-parity record behind it. `unproven` means no fixture exercises \
                      the surface - nothing is claimed about it, which is different from \
                      `experimental`, a judgment someone made. Use it before relying on an API.",
        args: &[
            Arg {
                name: "filter",
                ty: "string",
                description: "Glob narrowing the entries, e.g. `std::http::*`.",
                required: false,
            },
            Arg {
                name: "status",
                ty: "string",
                description: "Only entries with this lifecycle status.",
                required: false,
            },
        ],
    },
    Tool {
        name: "hover",
        description: "Type and docs for the symbol at a source position.",
        args: POSITION_ARGS,
    },
    Tool {
        name: "definition",
        description: "Location of the declaring item for the symbol at a source position.",
        args: POSITION_ARGS,
    },
    Tool {
        name: "references",
        description: "Every reference to the symbol at a source position.",
        args: POSITION_ARGS,
    },
    Tool {
        name: "workspace_symbols",
        description: "Search item declarations by name across a workspace.",
        args: &[
            Arg {
                name: "query",
                ty: "string",
                description: "Substring to match against item names.",
                required: true,
            },
            Arg {
                name: "root",
                ty: "string",
                description: "Workspace root directory (default: current directory).",
                required: false,
            },
        ],
    },
];

/// `tools/list` result.
pub(crate) fn list() -> Value {
    let tools = TOOLS.iter().map(tool_value).collect();
    obj(vec![("tools", Value::Array(tools))])
}

fn tool_value(tool: &Tool) -> Value {
    let props = tool
        .args
        .iter()
        .map(|a| {
            (
                a.name.to_string(),
                obj(vec![("type", s(a.ty)), ("description", s(a.description))]),
            )
        })
        .collect();
    let required = tool
        .args
        .iter()
        .filter(|a| a.required)
        .map(|a| s(a.name))
        .collect();
    obj(vec![
        ("name", s(tool.name)),
        ("description", s(tool.description)),
        (
            "inputSchema",
            obj(vec![
                ("type", s("object")),
                ("properties", Value::Object(props)),
                ("required", Value::Array(required)),
            ]),
        ),
    ])
}

/// A `.gos` file holding an inline `source` argument for the duration of
/// one tool call. Removed when the guard drops, so a snippet leaves
/// nothing behind in the caller's workspace.
struct SourceFile {
    path: PathBuf,
}

impl SourceFile {
    fn write(source: &str) -> Result<Self, String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "gos-mcp-{}-{}.gos",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, source).map_err(|e| format!("writing inline source: {e}"))?;
        Ok(Self { path })
    }

    fn display(&self) -> String {
        self.path.display().to_string()
    }

    /// The file's text as it stands, after any tool rewrote it.
    fn read(&self) -> Result<String, String> {
        std::fs::read_to_string(&self.path).map_err(|e| format!("reading inline source: {e}"))
    }
}

impl Drop for SourceFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Resolves a tool's target: the inline `source` when present, otherwise
/// the named path argument. The returned guard must outlive the call.
fn target_of(args: &Value, path_key: &str) -> Result<(Option<String>, Option<SourceFile>), String> {
    if let Some(source) = field_str(args, "source") {
        let file = SourceFile::write(source)?;
        return Ok((Some(file.display()), Some(file)));
    }
    Ok((field_str(args, path_key).map(String::from), None))
}

/// `tools/call` dispatch: returns a complete JSON-RPC response.
pub(crate) fn call(
    id: Value,
    params: &Value,
    config: &ServerConfig,
    nav: &mut NavSession,
) -> Value {
    let Some(name) = field_str(params, "name") else {
        return response_err(id, -32602, "tools/call requires a `name`");
    };
    let args = field(params, "arguments");
    let outcome = match name {
        "check" => check_tool(config, args),

        "explain" => match field_str(args, "code") {
            Some(code) => exec_tool(config, vec!["explain".into(), code.into()], args),
            None => Err("`code` is required".to_string()),
        },
        "execute" => match target_of(args, "file") {
            Err(e) => Err(e),
            Ok((None, _)) => Err("`file` or `source` is required".to_string()),
            Ok((Some(file), _guard)) => {
                let mut command = vec!["run".to_string(), file];
                if let Some(extra) = json::as_array(field(args, "args")) {
                    command.extend(extra.iter().filter_map(json::as_str).map(String::from));
                }
                exec::run_gos(&config.gos_exe, &command, timeout_from(args))
                    .map(|outcome| exec_result(&outcome))
            }
        },
        "build" => {
            let mut command = vec!["build".to_string()];
            if json::as_bool(field(args, "release")) == Some(true) {
                command.push("--release".to_string());
            }
            if let Some(file) = field_str(args, "file") {
                command.push(file.to_string());
            }
            exec_tool(config, command, args)
        }
        "test" => {
            let mut command = vec!["test".to_string()];
            if let Some(path) = field_str(args, "path") {
                command.push(path.to_string());
            }
            if let Some(filter) = field_str(args, "filter") {
                command.push("--run".to_string());
                command.push(filter.to_string());
            }
            exec_tool(config, command, args)
        }
        "fmt" => match target_of(args, "file") {
            Err(e) => Err(e),
            Ok((None, _)) => Err("`file` or `source` is required".to_string()),
            Ok((Some(file), guard)) => {
                let mut command = vec!["fmt".to_string()];
                let check = json::as_bool(field(args, "check")) == Some(true);
                if check {
                    command.push("--check".to_string());
                }
                command.push(file);
                let rewritten = if check { None } else { guard.as_ref() };
                rewriting_tool(config, command, args, rewritten)
            }
        },
        "doc" => match target_of(args, "file") {
            Err(e) => Err(e),
            Ok((None, _)) => Err("`file` or `source` is required".to_string()),
            Ok((Some(file), _guard)) => exec_tool(config, vec!["doc".to_string(), file], args),
        },
        "lint" => match target_of(args, "path") {
            Err(e) => Err(e),
            Ok((path, guard)) => {
                let mut command = vec!["lint".to_string()];
                let fix = json::as_bool(field(args, "fix")) == Some(true);
                if fix {
                    command.push("--fix".to_string());
                }
                if json::as_bool(field(args, "deny_warnings")) == Some(true) {
                    command.push("--deny-warnings".to_string());
                }
                if let Some(path) = path {
                    command.push(path);
                }
                let rewritten = if fix { guard.as_ref() } else { None };
                rewriting_tool(config, command, args, rewritten)
            }
        },
        "fix" => fix_tool(config, args),
        "audit" => audit_tool(config, args),
        "feature_status" => feature_status_tool(config, args),
        "hover" | "definition" | "references" => nav.position_tool(name, args),
        "workspace_symbols" => nav.workspace_symbols(args),
        other => return response_err(id, -32602, &format!("unknown tool: {other}")),
    };
    match outcome {
        Ok(result) => response_ok(id, result),
        Err(message) => response_ok(id, text_result(&message, true)),
    }
}

/// Runs `gos feature-status` with the optional narrowing flags.
fn feature_status_tool(config: &ServerConfig, args: &Value) -> Result<Value, String> {
    let mut command = vec!["feature-status".to_string()];
    if let Some(filter) = field_str(args, "filter") {
        command.push("--filter".to_string());
        command.push(filter.to_string());
    }
    if let Some(status) = field_str(args, "status") {
        command.push("--status".to_string());
        command.push(status.to_string());
    }
    exec_tool(config, command, args)
}

/// Runs `gos audit`, always in the shared diagnostic JSON shape.
fn audit_tool(config: &ServerConfig, args: &Value) -> Result<Value, String> {
    let mut command = vec![
        "audit".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ];
    if json::as_bool(field(args, "all")) == Some(true) {
        command.push("--all".to_string());
    }
    if let Some(path) = field_str(args, "path") {
        command.push(path.to_string());
    }
    exec_tool(config, command, args)
}

/// Runs `gos fix`, resolving an inline `source` to a temporary file.
fn fix_tool(config: &ServerConfig, args: &Value) -> Result<Value, String> {
    let (path, guard) = target_of(args, "path")?;
    let mut command = vec!["fix".to_string()];
    let list = json::as_bool(field(args, "list")) == Some(true);
    let check = json::as_bool(field(args, "check")) == Some(true);
    if list {
        command.push("--list".to_string());
    }
    if check {
        command.push("--check".to_string());
    }
    if let Some(id) = field_str(args, "rewriter") {
        command.push("--rewriter".to_string());
        command.push(id.to_string());
    }
    if let Some(path) = path {
        command.push(path);
    }
    let rewritten = if list || check { None } else { guard.as_ref() };
    rewriting_tool(config, command, args, rewritten)
}

fn check_args(target: Option<&str>, fix: bool) -> Vec<String> {
    let mut command = vec![
        "check".to_string(),
        "--message-format".to_string(),
        "json".to_string(),
    ];
    if fix {
        command.push("--fix".to_string());
    }
    if let Some(file) = target {
        command.push(file.to_string());
    }
    command
}

fn timeout_from(args: &Value) -> Duration {
    let ms = json::as_i64(field(args, "timeout_ms"))
        .and_then(|n| u64::try_from(n).ok())
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    Duration::from_millis(ms)
}

fn exec_tool(config: &ServerConfig, command: Vec<String>, args: &Value) -> Result<Value, String> {
    let outcome = exec::run_gos(&config.gos_exe, &command, timeout_from(args))?;
    Ok(exec_result(&outcome))
}

/// Runs `gos check` and returns its diagnostics already parsed.
///
/// `check_args` forces `--message-format json`, so the child writes one
/// JSON object per diagnostic; parsing them here hands the caller a
/// ready array instead of a text blob it would have to re-split.
fn check_tool(config: &ServerConfig, args: &Value) -> Result<Value, String> {
    let (target, guard) = target_of(args, "file")?;
    let fix = json::as_bool(field(args, "fix")) == Some(true);
    let outcome = exec::run_gos(
        &config.gos_exe,
        &check_args(target.as_deref(), fix),
        timeout_from(args),
    )?;
    let mut structured = check_report(&outcome);
    if fix
        && let Some(file) = &guard
        && let Value::Object(fields) = &mut structured
    {
        fields.insert("source".to_string(), s(&file.read()?));
    }
    let mut result = text_result(&json::to_string(&structured), tool_failed(&outcome));
    if let Value::Object(fields) = &mut result {
        fields.insert("structuredContent".to_string(), structured);
    }
    Ok(result)
}

/// Runs a tool that may rewrite its target. An inline `source` lives in a
/// temporary file the call removes, so its rewritten text is handed back as
/// a second content item.
fn rewriting_tool(
    config: &ServerConfig,
    command: Vec<String>,
    args: &Value,
    guard: Option<&SourceFile>,
) -> Result<Value, String> {
    let mut result = exec_tool(config, command, args)?;
    if let Some(file) = guard
        && let Value::Object(fields) = &mut result
        && let Some(Value::Array(content)) = fields.get_mut("content")
    {
        let rewritten = file.read()?;
        content.push(obj(vec![
            ("type", s("text")),
            ("text", s(&format!("--- source ---\n{rewritten}"))),
        ]));
    }
    Ok(result)
}

/// Builds the `check` report: the child's diagnostics parsed out of its
/// line-delimited JSON, plus how the child terminated.
///
/// Non-JSON lines are the human-readable summary `check` prints
/// alongside the machine format; they carry no diagnostic and are
/// dropped.
fn check_report(outcome: &ExecOutcome) -> Value {
    let diagnostics: Vec<Value> = outcome
        .stderr
        .lines()
        .chain(outcome.stdout.lines())
        .filter_map(|line| json::parse(line.trim()).ok())
        .filter(|value| matches!(value, Value::Object(_)))
        .collect();
    obj(vec![
        ("diagnostics", Value::Array(diagnostics)),
        (
            "exitCode",
            outcome.exit_code.map_or(Value::Null, Value::Int),
        ),
        ("timedOut", Value::Bool(outcome.timed_out)),
    ])
}

/// A subprocess that timed out or exited non-zero is a failed tool call.
fn tool_failed(outcome: &ExecOutcome) -> bool {
    outcome.timed_out || outcome.exit_code != Some(0)
}

/// Wraps a finished subprocess as MCP tool-call content.
///
/// A non-zero exit is a failed tool call: a caller that only inspects
/// `isError` must not read a failing `check` as a success whose text
/// happens to contain errors.
fn exec_result(outcome: &ExecOutcome) -> Value {
    let status = match (outcome.timed_out, outcome.exit_code) {
        (true, _) => "timed out (killed)".to_string(),
        (false, Some(code)) => format!("exit code: {code}"),
        (false, None) => "killed by signal".to_string(),
    };
    let text = format!(
        "{status}\n\n--- stdout ---\n{}\n--- stderr ---\n{}",
        outcome.stdout, outcome.stderr
    );
    text_result(&text, tool_failed(outcome))
}

/// Wraps `text` as MCP tool-call content.
pub(crate) fn text_result(text: &str, is_error: bool) -> Value {
    obj(vec![
        (
            "content",
            Value::Array(vec![obj(vec![("type", s("text")), ("text", s(text))])]),
        ),
        ("isError", Value::Bool(is_error)),
    ])
}

#[cfg(test)]
mod tools_tests {
    use super::*;

    fn outcome(exit_code: Option<i64>, stdout: &str, stderr: &str) -> ExecOutcome {
        ExecOutcome {
            exit_code,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            timed_out: false,
        }
    }

    #[test]
    fn a_nonzero_exit_is_reported_as_a_tool_error() {
        let failing = exec_result(&outcome(Some(1), "", "error[GT0001]: mismatch\n"));
        assert_eq!(
            json::get(&failing, "isError").and_then(json::as_bool),
            Some(true)
        );
        let passing = exec_result(&outcome(Some(0), "check: ok\n", ""));
        assert_eq!(
            json::get(&passing, "isError").and_then(json::as_bool),
            Some(false)
        );
    }

    #[test]
    fn check_diagnostics_are_returned_parsed_not_as_a_blob() {
        let stderr = "{\"code\":\"GT0001\",\"message\":\"mismatch\"}\n\
                      {\"code\":\"GM0001\",\"message\":\"non-exhaustive\"}\n";
        let report = check_report(&outcome(Some(1), "", stderr));
        let diagnostics = json::get(&report, "diagnostics")
            .and_then(json::as_array)
            .expect("diagnostics array");
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(
            json::get(&diagnostics[1], "code").and_then(json::as_str),
            Some("GM0001")
        );
        assert_eq!(
            json::get(&report, "exitCode").and_then(json::as_i64),
            Some(1)
        );
    }

    #[test]
    fn the_human_readable_summary_line_is_not_a_diagnostic() {
        let report = check_report(&outcome(Some(0), "check: ok (3 items typed)\n", ""));
        let diagnostics = json::get(&report, "diagnostics")
            .and_then(json::as_array)
            .expect("diagnostics array");
        assert!(diagnostics.is_empty(), "got {diagnostics:?}");
    }
}
