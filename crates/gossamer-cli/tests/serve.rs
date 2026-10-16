//! `gos serve` serves a directory's files over HTTP and lists a directory
//! that has no `index.html`.

#![allow(missing_docs)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// An empty directory of the test's own under the system temp dir.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gos-serve-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// A running `gos serve` and the `host:port` it bound.
struct Server {
    child: Child,
    addr: String,
}

impl Server {
    fn start(dir: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_gos"))
            .arg("serve")
            .arg(dir)
            .args(["--port", "0"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn gos serve");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .expect("read the serving line");
        let url = line
            .split_whitespace()
            .find(|word| word.starts_with("http://"))
            .unwrap_or_else(|| panic!("no URL in {line:?}"));
        let addr = url
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        Self { child, addr }
    }

    /// The status line and body `gos serve` answers `method target` with.
    fn request(&self, method: &str, target: &str) -> (String, String) {
        let mut stream = TcpStream::connect(&self.addr).expect("connect");
        write!(
            stream,
            "{method} {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            self.addr
        )
        .expect("send request");
        let mut reply = String::new();
        stream.read_to_string(&mut reply).expect("read reply");
        let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
        let status = head.lines().next().unwrap_or_default().to_string();
        (status, body.to_string())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn serve_answers_files_listings_and_errors() {
    let dir = scratch("files");
    std::fs::write(dir.join("a.txt"), "hello\n").expect("write a.txt");
    std::fs::create_dir(dir.join("sub dir")).expect("mkdir");
    std::fs::write(dir.join("sub dir").join("b c.txt"), "nested\n").expect("write");
    let server = Server::start(&dir);
    assert!(
        server.addr.starts_with("127.0.0.1:"),
        "binds loopback: {}",
        server.addr
    );

    let (status, body) = server.request("GET", "/a.txt");
    assert!(status.contains(" 200 "), "{status}");
    assert_eq!(body, "hello\n");

    let (status, body) = server.request("GET", "/");
    assert!(status.contains(" 200 "), "{status}");
    assert!(body.contains("<a href=\"a.txt\">a.txt</a>"), "{body}");
    assert!(
        body.contains("<a href=\"sub%20dir/\">sub dir/</a>"),
        "{body}"
    );

    let (status, _) = server.request("GET", "/sub%20dir");
    assert!(status.contains(" 301 "), "{status}");
    let (_, body) = server.request("GET", "/sub%20dir/b%20c.txt");
    assert_eq!(body, "nested\n");

    let (status, body) = server.request("HEAD", "/a.txt");
    assert!(status.contains(" 200 "), "{status}");
    assert!(body.is_empty(), "{body}");

    let (status, _) = server.request("GET", "/missing.txt");
    assert!(status.contains(" 404 "), "{status}");
    let (status, _) = server.request("GET", "/../../etc/passwd");
    assert!(status.contains(" 404 "), "{status}");
    let (status, _) = server.request("POST", "/a.txt");
    assert!(status.contains(" 405 "), "{status}");
}

#[test]
fn serve_refuses_a_path_that_is_not_a_directory() {
    let dir = scratch("not-a-dir");
    let file = dir.join("a.txt");
    std::fs::write(&file, "x").expect("write");
    let out = Command::new(env!("CARGO_BIN_EXE_gos"))
        .arg("serve")
        .arg(&file)
        .args(["--port", "0"])
        .output()
        .expect("run gos serve");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("is not a directory"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
