//! End-to-end MCP protocol test: drives `gos mcp` over real pipes.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

use gossamer_std::json::{self, Value};

struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl McpClient {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_gos"))
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn gos mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut client = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        let init = client.request(
            "initialize",
            "{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{}}",
        );
        assert!(json::get(&init, "result").is_some());
        client
    }

    /// The server process's id, which every inline source file it writes
    /// carries in its name.
    fn server_pid(&self) -> u32 {
        self.child.id()
    }

    fn request(&mut self, method: &str, params: &str) -> Value {
        self.next_id += 1;
        writeln!(
            self.stdin,
            "{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"{method}\",\"params\":{params}}}",
            self.next_id
        )
        .unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        json::parse(line.trim()).unwrap()
    }

    fn call_tool_result(&mut self, name: &str, arguments: &str) -> Value {
        let reply = self.request(
            "tools/call",
            &format!("{{\"name\":\"{name}\",\"arguments\":{arguments}}}"),
        );
        json::get(&reply, "result").expect("tool result").clone()
    }

    fn call_tool(&mut self, name: &str, arguments: &str) -> String {
        let result = self.call_tool_result(name, arguments);
        let content = json::get(&result, "content")
            .and_then(json::as_array)
            .unwrap();
        json::get(&content[0], "text")
            .and_then(json::as_str)
            .unwrap()
            .to_string()
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn json_path(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "\\\\")
}

#[test]
fn check_execute_and_timeout_work_end_to_end() {
    let dir = std::env::temp_dir().join(format!("gos-mcp-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut client = McpClient::start();

    // A parser error surfaces through `check` as a failed tool call whose
    // diagnostics arrive parsed, not as a blob the caller has to re-split.
    let bad = dir.join("bad.gos");
    std::fs::write(&bad, "fn main() { let x = }\n").unwrap();
    let result = client.call_tool_result("check", &format!("{{\"file\":\"{}\"}}", json_path(&bad)));
    assert_eq!(
        json::get(&result, "isError").and_then(json::as_bool),
        Some(true),
        "invalid syntax must fail MCP check: {result:?}"
    );
    let report = json::get(&result, "structuredContent").expect("structured check report");
    assert_eq!(
        json::get(report, "exitCode").and_then(json::as_i64),
        Some(1)
    );
    let diagnostics = json::get(report, "diagnostics")
        .and_then(json::as_array)
        .expect("parsed diagnostics");
    assert!(
        diagnostics
            .iter()
            .any(|diag| json::get(diag, "code").and_then(json::as_str) == Some("GP0001")),
        "MCP check omitted parser diagnostic: {diagnostics:?}"
    );

    // A clean program runs and its stdout comes back.
    let ok = dir.join("ok.gos");
    std::fs::write(&ok, "fn main() { println(\"mcp says {}\", 21 * 2) }\n").unwrap();
    let text = client.call_tool("execute", &format!("{{\"file\":\"{}\"}}", json_path(&ok)));
    assert!(text.contains("exit code: 0"), "execute output was: {text}");
    assert!(text.contains("mcp says 42"), "execute output was: {text}");

    // An infinite loop is killed at the timeout instead of hanging.
    let spin = dir.join("spin.gos");
    std::fs::write(&spin, "fn main() { loop { } }\n").unwrap();
    let text = client.call_tool(
        "execute",
        &format!("{{\"file\":\"{}\",\"timeout_ms\":500}}", json_path(&spin)),
    );
    assert!(text.contains("timed out"), "timeout output was: {text}");

    // The skill card resource reads back.
    let reply = client.request("resources/read", "{\"uri\":\"gossamer://skill-card\"}");
    let contents = json::get(json::get(&reply, "result").unwrap(), "contents")
        .and_then(json::as_array)
        .unwrap();
    assert!(
        json::get(&contents[0], "text")
            .and_then(json::as_str)
            .unwrap()
            .contains("Gossamer")
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// An agent iterating on a snippet should not have to write a file first.
/// `source` covers the tools that take a target, and the temporary file the
/// server writes must not survive the call.
#[test]
fn inline_source_drives_check_execute_and_lint() {
    let mut client = McpClient::start();

    let result = client.call_tool_result("check", "{\"source\":\"fn main() { let x = }\\n\"}");
    assert_eq!(
        json::get(&result, "isError").and_then(json::as_bool),
        Some(true),
        "inline source with a syntax error must fail check: {result:?}"
    );
    let report = json::get(&result, "structuredContent").expect("structured report");
    let diagnostics = json::get(report, "diagnostics")
        .and_then(json::as_array)
        .expect("parsed diagnostics");
    assert!(
        diagnostics
            .iter()
            .any(|diag| json::get(diag, "code").and_then(json::as_str) == Some("GP0001")),
        "inline check omitted the parser diagnostic: {diagnostics:?}"
    );

    let text = client.call_tool(
        "execute",
        "{\"source\":\"fn main() { println(\\\"inline {}\\\", 6 * 7) }\\n\"}",
    );
    assert!(text.contains("exit code: 0"), "execute output was: {text}");
    assert!(text.contains("inline 42"), "execute output was: {text}");

    let text = client.call_tool(
        "lint",
        "{\"source\":\"fn main() { let unused = 1\\n    println(\\\"hi\\\") }\\n\"}",
    );
    assert!(text.contains("unused_variable"), "lint output was: {text}");

    // The name carries the server's pid, and the temp directory is
    // shared with every other case running beside this one: a sweep for
    // `gos-mcp-*` would fail on a neighbour's file that is still in use.
    let mine = format!("gos-mcp-{}-", client.server_pid());
    let leaked: Vec<String> = std::fs::read_dir(std::env::temp_dir())
        .expect("read temp dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.starts_with(&mine)
                && std::path::Path::new(name)
                    .extension()
                    .is_some_and(|e| e == "gos")
        })
        .collect();
    assert!(
        leaked.is_empty(),
        "inline source files outlived their calls: {leaked:?}"
    );
}

/// Inline source is a temporary file the call removes, so a tool that
/// rewrites it hands the rewritten text back: `check` with `fix` in its
/// report's `source`, and `fix` and `fmt` as a second content item.
#[test]
fn inline_source_comes_back_rewritten() {
    let mut client = McpClient::start();
    let old = "use std::regex\n\nfn main() {\n    let xs = vec![1, 2]\n    \
               let (a, b) = (1, 2)\n    let r = regex::compile(\"a+\").unwrap()\n    \
               println(\"{} {} {} {}\", xs.len(), a, b, regex::is_match(r, \"a\"))\n}\n";
    let arguments = format!(
        "{{\"source\":{},\"fix\":true}}",
        json::to_string(&Value::String(old.to_string()))
    );
    let result = client.call_tool_result("check", &arguments);
    let report = json::get(&result, "structuredContent").expect("structured report");
    let repaired = json::get(report, "source")
        .and_then(json::as_str)
        .expect("rewritten source")
        .to_string();
    assert!(repaired.contains("let xs = #[1, 2]"), "{repaired}");
    assert!(repaired.contains("let a, b = (1, 2)"), "{repaired}");

    let arguments = format!(
        "{{\"source\":{}}}",
        json::to_string(&Value::String(repaired))
    );
    let result = client.call_tool_result("fix", &arguments);
    let content = json::get(&result, "content")
        .and_then(json::as_array)
        .expect("content");
    let migrated = content
        .get(1)
        .and_then(|item| json::get(item, "text"))
        .and_then(json::as_str)
        .expect("rewritten source item");
    assert!(
        migrated.contains("let r = regex::compile(\"a+\")\n"),
        "{migrated}"
    );

    let result =
        client.call_tool_result("fmt", "{\"source\":\"fn main() {\\n  println(1)\\n}\\n\"}");
    let content = json::get(&result, "content")
        .and_then(json::as_array)
        .expect("content");
    let formatted = content
        .get(1)
        .and_then(|item| json::get(item, "text"))
        .and_then(json::as_str)
        .expect("formatted source item");
    assert!(formatted.contains("\n    println(1)\n"), "{formatted}");
}

/// `feature_status` lets an agent see whether an API is settled before it
/// commits to one, and `doc` answers stdlib queries without a file.
#[test]
fn feature_status_and_stdlib_doc_are_reachable_as_tools() {
    let mut client = McpClient::start();

    let text = client.call_tool("feature_status", "{\"filter\":\"std::strings\"}");
    assert!(
        text.contains("std::strings"),
        "feature_status output: {text}"
    );

    let text = client.call_tool("doc", "{\"file\":\"std::strings\"}");
    assert!(
        text.contains("std::strings::trim"),
        "stdlib doc output: {text}"
    );

    let listed = client.request("tools/list", "{}");
    let tools = json::get(json::get(&listed, "result").unwrap(), "tools")
        .and_then(json::as_array)
        .expect("tool list");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| json::get(t, "name").and_then(json::as_str))
        .collect();
    for expected in ["lint", "feature_status"] {
        assert!(
            names.contains(&expected),
            "tools/list omitted {expected}: {names:?}"
        );
    }
}
