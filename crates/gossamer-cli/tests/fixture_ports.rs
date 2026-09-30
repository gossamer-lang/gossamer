//! Server fixtures that run side by side keep their default ports apart.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Hosts a fixture binds or calls a loopback server on.
const HOSTS: [&str; 4] = ["127.0.0.1:", "0.0.0.0:", "localhost:", "[::1]:"];

/// The examples' documented server port. No program the harnesses run
/// unattended binds it: `examples/web_server.gos` serves on it only when a
/// reader starts it, and the other uses are text a program parses.
const DOCUMENTED_PORT: u16 = 8080;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the workspace root is two levels above this crate")
        .to_path_buf()
}

/// Every fixed port a loopback address in `source` names. Port 0 asks the
/// system for a free port, so it is never shared.
fn loopback_ports(source: &str) -> Vec<u16> {
    let mut ports = Vec::new();
    for host in HOSTS {
        for (at, _) in source.match_indices(host) {
            let digits: String = source[at + host.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(port) = digits.parse::<u16>()
                && port != 0
            {
                ports.push(port);
            }
        }
    }
    ports
}

/// The sweeps run the fixtures in parallel with no arguments, so each server
/// fixture falls back to its default address. Two fixtures sharing a default
/// port would answer each other's requests, and which one answered would
/// depend on scheduling.
#[test]
fn no_two_fixtures_share_a_default_port() {
    let root = workspace_root();
    let mut users: BTreeMap<u16, Vec<String>> = BTreeMap::new();
    for dir in ["examples", "feature-testing-examples"] {
        let entries = std::fs::read_dir(root.join(dir)).expect("read the fixture directory");
        for entry in entries {
            let path = entry.expect("a directory entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("gos") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read the fixture");
            let mut ports = loopback_ports(&source);
            ports.sort_unstable();
            ports.dedup();
            let name = format!(
                "{dir}/{}",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
            );
            for port in ports {
                users.entry(port).or_default().push(name.clone());
            }
        }
    }
    let shared: Vec<String> = users
        .iter()
        .filter(|(port, files)| **port != DOCUMENTED_PORT && files.len() > 1)
        .map(|(port, files)| format!("{port}: {}", files.join(", ")))
        .collect();
    assert!(
        shared.is_empty(),
        "fixtures share a default port; give each its own:\n{}",
        shared.join("\n")
    );
}
