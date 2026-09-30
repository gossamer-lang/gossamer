//! MCP request-dispatch loop.

use std::io::{BufRead, BufWriter, Write};

use gossamer_std::json::Value;

use crate::ServerConfig;
use crate::protocol::{field, field_str, obj, response_err, response_ok, s};
use crate::transport::{Incoming, Transport};

/// Latest MCP revision the server implements; also the fallback when
/// the client omits `protocolVersion`.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// The canonical skill card; `SKILL.md` at the repo root is the source
/// of truth, mirroring `gos skill-prompt`.
const SKILL_CARD: &str = include_str!("../../../SKILL.md");
const SKILL_CARD_URI: &str = "gossamer://skill-card";

/// Runs the dispatch loop over the supplied streams until EOF.
pub(crate) fn run<R: BufRead, W: Write>(
    reader: R,
    writer: W,
    config: &ServerConfig,
) -> std::io::Result<()> {
    let mut transport = Transport::new(reader, BufWriter::new(writer));
    let mut nav = crate::nav::NavSession::new();
    loop {
        let message = match transport.read_message()? {
            Incoming::Eof => return Ok(()),
            Incoming::ParseError => {
                transport.write_message(&response_err(Value::Null, -32700, "parse error"))?;
                continue;
            }
            Incoming::Message(value) => value,
        };
        let Some(method) = field_str(&message, "method") else {
            continue;
        };
        let id = field(&message, "id").clone();
        let params = field(&message, "params").clone();
        let is_notification = matches!(id, Value::Null);

        let reply = match method {
            "initialize" => Some(response_ok(id, initialize_result(&params))),
            "notifications/initialized" | "notifications/cancelled" => None,
            "ping" => Some(response_ok(id, obj(vec![]))),
            "tools/list" => Some(response_ok(id, crate::tools::list())),
            "tools/call" => Some(crate::tools::call(id, &params, config, &mut nav)),
            "resources/list" => Some(response_ok(id, resources_list())),
            "resources/read" => Some(resources_read(id, &params)),
            "prompts/list" => Some(response_ok(id, prompts_list())),
            "prompts/get" => Some(prompts_get(id, &params)),
            _ if is_notification => None,
            other => Some(response_err(
                id,
                -32601,
                &format!("method not found: {other}"),
            )),
        };
        if let Some(reply) = reply {
            transport.write_message(&reply)?;
        }
    }
}

fn initialize_result(params: &Value) -> Value {
    let version = field_str(params, "protocolVersion").unwrap_or(PROTOCOL_VERSION);
    obj(vec![
        ("protocolVersion", s(version)),
        (
            "capabilities",
            obj(vec![
                ("tools", obj(vec![])),
                ("resources", obj(vec![])),
                ("prompts", obj(vec![])),
            ]),
        ),
        (
            "serverInfo",
            obj(vec![
                ("name", s("gos-mcp")),
                ("version", s(env!("CARGO_PKG_VERSION"))),
            ]),
        ),
        (
            "instructions",
            s(
                "Gossamer toolchain server. Run `check` before `execute`; read the \
               gossamer://skill-card resource (or the skill-card prompt) to learn \
               idiomatic Gossamer before writing .gos code. Prefer receiver methods \
               and metadata fields already returned by standard library records over \
               redundant module calls. Prefer dedicated collection contracts: \
               Stack for LIFO-only values, Queue for FIFO-only values, \
               MinHeap or MaxHeap for priority queues, and Deque only when \
               both ends matter. Keep HTML in its own file and read or embed \
               it from there rather than writing markup inside Gossamer \
               source. The syntax is Rust-flavoured and the semantics are \
               not: there is no ownership transfer, no borrow checker, and \
               no lifetimes. Every parameter is by value - passing a \
               collection copies nothing and the callee still cannot change \
               the caller's value; `mut` on a parameter is the callee's own \
               value, and only a `&mut T` parameter, spelled `&mut x` at the \
               call site, writes back. `let b = a` gives `b` a value of its \
               own, and passing a value twice needs no clone. A `Vec` \
               literal is `#[a, b]` (there is no `vec!`), a fixed array is \
               `[a, b]`, a map is `{\"k\": 1}`, and a set is `#{a, b}`; a \
               string literal already IS a `String`; a file IS a module, \
               reached with `use`, and there is no `include`.",
            ),
        ),
    ])
}

fn resources_list() -> Value {
    obj(vec![(
        "resources",
        Value::Array(vec![obj(vec![
            ("uri", s(SKILL_CARD_URI)),
            ("name", s("Gossamer skill card")),
            (
                "description",
                s("Self-contained idiomatic-Gossamer reference for coding agents."),
            ),
            ("mimeType", s("text/markdown")),
        ])]),
    )])
}

fn resources_read(id: Value, params: &Value) -> Value {
    match field_str(params, "uri") {
        Some(SKILL_CARD_URI) => response_ok(
            id,
            obj(vec![(
                "contents",
                Value::Array(vec![obj(vec![
                    ("uri", s(SKILL_CARD_URI)),
                    ("mimeType", s("text/markdown")),
                    ("text", s(SKILL_CARD)),
                ])]),
            )]),
        ),
        other => response_err(
            id,
            -32002,
            &format!("unknown resource: {}", other.unwrap_or("<missing uri>")),
        ),
    }
}

fn prompts_list() -> Value {
    obj(vec![(
        "prompts",
        Value::Array(vec![obj(vec![
            ("name", s("skill-card")),
            (
                "description",
                s("Teach the model idiomatic Gossamer in one step."),
            ),
        ])]),
    )])
}

fn prompts_get(id: Value, params: &Value) -> Value {
    match field_str(params, "name") {
        Some("skill-card") => response_ok(
            id,
            obj(vec![
                ("description", s("The Gossamer skill card.")),
                (
                    "messages",
                    Value::Array(vec![obj(vec![
                        ("role", s("user")),
                        (
                            "content",
                            obj(vec![("type", s("text")), ("text", s(SKILL_CARD))]),
                        ),
                    ])]),
                ),
            ]),
        ),
        other => response_err(
            id,
            -32602,
            &format!("unknown prompt: {}", other.unwrap_or("<missing name>")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::SKILL_CARD;

    #[test]
    fn skill_card_teaches_explicit_imports_and_direct_metadata_access() {
        assert!(SKILL_CARD.contains("Import everything you name"));
        assert!(SKILL_CARD.contains("`fs::read_dir` entries carry `is_file`"));
        assert!(SKILL_CARD.contains("do not re-query"));
        assert!(SKILL_CARD.contains("the call site spells it"));
    }

    #[test]
    fn skill_card_teaches_collection_literal_spellings() {
        for literal in [
            "`#[1, 2]` Vec",
            "`[1, 2]` fixed array",
            "`{\"k\": 1}` Map",
            "`#{1, 2}`",
            "`#[0; n]`",
            "`[0; 4]`",
            "`T::from([..])`",
        ] {
            assert!(
                SKILL_CARD.contains(literal),
                "skill card should document {literal}"
            );
        }
        // The retired bracket spellings must not read as live syntax.
        for retired in ["`^[]`", "`_[]`", "`<[]`", "`[]>`"] {
            assert!(
                !SKILL_CARD.contains(retired),
                "skill card still presents the removed literal {retired}"
            );
        }
        for contract in [
            "`Stack` for LIFO",
            "`MinHeap` instead of negated keys",
            "`Queue`",
        ] {
            assert!(
                SKILL_CARD.contains(contract),
                "skill card should steer to {contract}"
            );
        }
    }
}
