//! Phase-2 proc-macro coverage: `#[gos_module]`, `#[gos_opaque]`,
//! `#[gos_blocking]`, and `#[derive(GosStruct)]`.

#![allow(missing_docs, dead_code)]
//!
//! The derive emits `FromGos`/`ToGos`/`SigType` impls that
//! round-trip a Rust struct through `Value::Struct`. The
//! attribute proc-macros are exercised indirectly through their
//! generated `register_module!` calls - verifying the resulting
//! module-table entries exist with the expected items.

use gossamer_binding::{FromGos, GosError, GosStruct, ToGos, gos_blocking, gos_module};

#[derive(GosStruct, Clone, Debug)]
pub struct Server {
    pub address: String,
    pub port: i64,
    pub healthy: bool,
}

#[gos_module("p2")]
mod bindings {
    /// Build a default server.
    pub fn make_server() -> super::Server {
        super::Server {
            address: "127.0.0.1".to_string(),
            port: 8080,
            healthy: true,
        }
    }

    /// Echo a server's port back.
    pub fn server_port(s: super::Server) -> i64 {
        s.port
    }

    /// Fallible parse via `?`-propagation.
    pub fn parse_port(s: String) -> Result<i64, ::gossamer_binding::GosError> {
        Ok(s.parse::<i64>()?)
    }
}

#[gos_blocking]
fn _gos_blocking_smoke(s: String) -> String {
    s.to_uppercase()
}

#[test]
fn gos_module_registers_under_supplied_path() {
    gossamer_binding::testing::install_host();
    let m = gossamer_binding::modules()
        .iter()
        .find(|m| m.path == "p2")
        .copied()
        .expect("p2 module registered");
    let names: Vec<&str> = m.items.iter().map(|i| i.name).collect();
    assert!(names.contains(&"make_server"), "items: {names:?}");
    assert!(names.contains(&"server_port"));
    assert!(names.contains(&"parse_port"));
}

#[test]
fn doc_comments_through_proc_macro_flow() {
    gossamer_binding::testing::install_host();
    let m = gossamer_binding::modules()
        .iter()
        .find(|m| m.path == "p2")
        .copied()
        .expect("p2 module");
    let make_server = m.items.iter().find(|i| i.name == "make_server").unwrap();
    assert!(
        make_server.doc.contains("default"),
        "expected doc, got: {:?}",
        make_server.doc
    );
}

#[test]
fn gos_struct_derive_round_trips_through_value() {
    gossamer_binding::testing::install_host();
    let s = Server {
        address: "0.0.0.0".to_string(),
        port: 9000,
        healthy: false,
    };
    let v = s.clone().to_gos();
    let back = Server::from_gos(&v).expect("round-trip");
    assert_eq!(back.address, s.address);
    assert_eq!(back.port, s.port);
    assert_eq!(back.healthy, s.healthy);
}

#[test]
fn gos_blocking_attribute_is_transparent_inline() {
    gossamer_binding::testing::install_host();
    assert_eq!(_gos_blocking_smoke("abc".to_string()), "ABC");
}

#[test]
fn gos_error_propagates_through_question_mark() {
    fn run() -> Result<i64, GosError> {
        let s = "12x".to_string();
        let n: i64 = s.parse::<i64>()?;
        Ok(n)
    }
    let err = run().unwrap_err();
    assert!(err.render().contains("parse int"));
}
