//! `gos serve`: serves a directory over HTTP, in the manner of
//! `python -m http.server`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use gossamer_std::http::server;
use gossamer_std::http::{Headers, Method, Request, Response, StatusCode};
use gossamer_std::http_static_files::FileServer;

/// Serves the files under `dir` on `bind`:`port` until the process is
/// stopped, listing a directory that has no `index.html`. The first line on
/// stdout names the URL it serves.
pub(crate) fn serve(dir: &Path, port: u16, bind: &str) -> Result<()> {
    let root = dir
        .canonicalize()
        .with_context(|| format!("serve: cannot read directory {}", dir.display()))?;
    if !root.is_dir() {
        bail!("serve: {} is not a directory", dir.display());
    }
    let addr = socket_address(bind, port);
    let listener =
        std::net::TcpListener::bind(&addr).with_context(|| format!("serve: binding {addr}"))?;
    let bound = listener
        .local_addr()
        .context("serve: reading the bound address")?;
    outln!("serve: serving {} at http://{bound}/", root.display());
    std::io::stdout()
        .flush()
        .context("serve: flushing stdout")?;
    let mut files = FileServer::new(root.clone());
    // A file is sent from disk as the client reads it, so its size costs no
    // memory and needs no cap.
    files.max_file_bytes = 0;
    let site = Site { files, root };
    let config = server::Config::default();
    server::run_inline(listener, &config, move |request| {
        let response = site.answer(&request);
        let line = format!(
            "{} \"{} {}\" {}\n",
            request.peer_addr,
            request.method.as_str(),
            request.path,
            response.status.0
        );
        // One write per line keeps lines from concurrent connections whole.
        let _ = std::io::stderr().write_all(line.as_bytes());
        response
    })
    .with_context(|| format!("serve: serving {bound}"))
}

/// `bind` and `port` as one socket address, bracketing an IPv6 host.
fn socket_address(bind: &str, port: u16) -> String {
    if bind.contains(':') && !bind.starts_with('[') {
        format!("[{bind}]:{port}")
    } else {
        format!("{bind}:{port}")
    }
}

/// The directory being served.
struct Site {
    files: FileServer,
    root: PathBuf,
}

impl Site {
    fn answer(&self, request: &Request) -> Response {
        let head = match request.method {
            Method::Get => false,
            Method::Head => true,
            _ => {
                let mut response = Response::text(StatusCode(405), "method not allowed\n");
                response.headers.insert("allow", "GET, HEAD");
                return response;
            }
        };
        let Some(rel) = percent_decode(request.path.trim_start_matches('/')) else {
            return Response::text(StatusCode(400), "malformed path\n");
        };
        let mut response = self
            .directory_redirect(&rel)
            .or_else(|| self.listing(&rel))
            .unwrap_or_else(|| self.files.serve_path(&rel, request));
        if head {
            response.body.clear();
        }
        response
    }

    /// The `301` that sends a directory named without its trailing slash to
    /// the name with one, so the relative links of its page resolve inside
    /// it. Each segment of the location is escaped as a URL spells it.
    fn directory_redirect(&self, rel: &str) -> Option<Response> {
        if rel.is_empty() || rel.ends_with('/') {
            return None;
        }
        // Most requests name a file: one stat settles that before the
        // directory's path is resolved.
        if !std::fs::metadata(self.root.join(rel)).is_ok_and(|meta| meta.is_dir()) {
            return None;
        }
        let dir = self.root.join(rel).canonicalize().ok()?;
        if !dir.starts_with(&self.root) || !dir.is_dir() {
            return None;
        }
        let mut location = String::with_capacity(rel.len() + 2);
        for segment in rel.split('/') {
            location.push('/');
            location.push_str(&percent_encode(segment));
        }
        location.push('/');
        let mut response = Response::text(StatusCode(301), "");
        response.headers.insert("location", &location);
        Some(response)
    }

    /// The HTML index of the directory `rel` names, when it is a directory
    /// under the root, written with its trailing slash, and holds no
    /// `index.html` for the file server to answer with.
    fn listing(&self, rel: &str) -> Option<Response> {
        if !(rel.is_empty() || rel.ends_with('/')) {
            return None;
        }
        let dir = self.root.join(rel).canonicalize().ok()?;
        if !dir.starts_with(&self.root) || !dir.is_dir() || dir.join("index.html").is_file() {
            return None;
        }
        let mut entries: Vec<(String, bool)> = std::fs::read_dir(&dir)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| {
                let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
                (entry.file_name().to_string_lossy().into_owned(), is_dir)
            })
            .collect();
        entries.sort();
        let title = escape_html(&format!("/{rel}"));
        let mut body = format!(
            "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n\
             <title>Directory listing for {title}</title>\n</head>\n<body>\n\
             <h1>Directory listing for {title}</h1>\n<hr>\n<ul>\n"
        );
        for (name, is_dir) in entries {
            let slash = if is_dir { "/" } else { "" };
            body.push_str(&format!(
                "<li><a href=\"{}{slash}\">{}{slash}</a></li>\n",
                percent_encode(&name),
                escape_html(&name),
            ));
        }
        body.push_str("</ul>\n<hr>\n</body>\n</html>\n");
        let mut headers = Headers::new();
        headers.insert("content-type", "text/html; charset=utf-8");
        headers.insert("content-length", &body.len().to_string());
        let mut response = Response::text(StatusCode(200), "");
        response.headers = headers;
        response.body = body.into_bytes();
        Some(response)
    }
}

/// `path` with each `%XX` escape replaced by its byte, or `None` when an
/// escape is malformed, the bytes are not UTF-8, or they spell a NUL.
fn percent_decode(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out)
        .ok()
        .filter(|text| !text.contains('\0'))
}

/// `name` with every byte outside the unreserved URL set escaped.
fn percent_encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{escape_html, percent_decode, percent_encode, socket_address};

    #[test]
    fn paths_decode_and_encode_round_trip() {
        assert_eq!(percent_decode("a%20b/c.txt").as_deref(), Some("a b/c.txt"));
        assert_eq!(percent_decode("%E2%9C%93"), Some("\u{2713}".to_string()));
        assert_eq!(percent_decode("bad%2"), None);
        assert_eq!(percent_decode("nul%00"), None);
        assert_eq!(percent_encode("a b&c.txt"), "a%20b%26c.txt");
    }

    #[test]
    fn an_ipv6_bind_address_is_bracketed() {
        assert_eq!(socket_address("127.0.0.1", 8000), "127.0.0.1:8000");
        assert_eq!(socket_address("::1", 8000), "[::1]:8000");
    }

    #[test]
    fn listing_names_are_escaped() {
        assert_eq!(escape_html("<a&b>"), "&lt;a&amp;b&gt;");
    }
}
