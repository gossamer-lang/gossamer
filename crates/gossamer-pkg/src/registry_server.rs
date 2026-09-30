//! A package registry served from a directory, speaking the protocol
//! `gos publish`, `gos fetch`, `gos yank`, and `gos owner` use.
//!
//! Layout under the root:
//!
//! - `index/<id>.json` - `{"versions": [..]}`, the document the resolver reads;
//! - `packages/<id>/<version>.tar` - each published archive, immutable;
//! - `owners/<id>.json` - the users who may publish, yank, or change owners;
//! - `advisories/` - the advisory feed and its signature, served as written;
//! - `tokens.toml` - `[tokens]` mapping each user to a bearer token.
//!
//! A root without `tokens.toml` is an open registry for one machine: every
//! request acts as the user `local`. Serve it on a loopback address only.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::id::ProjectId;
use crate::version::Version;

/// The user every request acts as on a registry with no `tokens.toml`.
pub const OPEN_REGISTRY_USER: &str = "local";

/// Longest request line or header the server reads.
const MAX_HEADER_LINE: usize = 8 * 1024;

/// Most headers one request may carry.
const MAX_HEADERS: usize = 64;

/// Largest JSON body a yank or owner request may carry.
const MAX_JSON_BODY: usize = 64 * 1024;

/// A registry's root directory and the users it knows.
#[derive(Debug)]
pub struct RegistryServer {
    root: PathBuf,
    /// Bearer token to user name. Empty for an open registry.
    tokens: BTreeMap<String, String>,
    /// Serializes every write to the index, owners, and package tree, so
    /// two uploads of one package cannot both claim a version.
    writes: Mutex<()>,
    spool_ids: AtomicU64,
}

/// An archive's digest with the publisher signature over it and the key
/// that made it, all lowercase hex.
struct SignedDigest {
    sha256: String,
    signature: String,
    public_key: String,
}

/// Why a request was refused, as the status and message the client sees.
#[derive(Debug)]
struct Refusal {
    status: u16,
    message: String,
}

impl Refusal {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl From<io::Error> for Refusal {
    fn from(error: io::Error) -> Self {
        Self::new(500, format!("registry storage: {error}"))
    }
}

/// One parsed request: its method, path, headers, and the stream its body
/// is still to be read from.
struct Request<'a> {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: &'a mut BufReader<TcpStream>,
}

impl Request<'_> {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    fn content_length(&self) -> Result<usize, Refusal> {
        if self.header("transfer-encoding").is_some() {
            return Err(Refusal::new(411, "send the body with a Content-Length"));
        }
        self.header("content-length")
            .unwrap_or("0")
            .trim()
            .parse()
            .map_err(|_| Refusal::new(400, "malformed Content-Length"))
    }

    fn json_body(&mut self) -> Result<Value, Refusal> {
        let len = self.content_length()?;
        if len > MAX_JSON_BODY {
            return Err(Refusal::new(413, "request body too large"));
        }
        let mut bytes = vec![0u8; len];
        self.body.read_exact(&mut bytes)?;
        if bytes.is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| Refusal::new(400, format!("malformed JSON: {e}")))
    }
}

impl RegistryServer {
    /// A registry over `root`, reading its users from `root/tokens.toml`
    /// when that file exists.
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let tokens = match fs::read_to_string(root.join("tokens.toml")) {
            Ok(text) => parse_tokens(&text)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            root,
            tokens,
            writes: Mutex::new(()),
            spool_ids: AtomicU64::new(0),
        })
    }

    /// Serves every connection `listener` accepts, one thread each, until
    /// accepting fails.
    pub fn serve(self: Arc<Self>, listener: &TcpListener) -> io::Result<()> {
        for stream in listener.incoming() {
            let stream = stream?;
            let server = Arc::clone(&self);
            std::thread::spawn(move || {
                if let Err(error) = server.handle(stream) {
                    eprintln!("registry: connection: {error}");
                }
            });
        }
        Ok(())
    }

    fn handle(&self, stream: TcpStream) -> io::Result<()> {
        let mut writer = stream.try_clone()?;
        let mut reader = BufReader::new(stream);
        let outcome = match read_head(&mut reader)? {
            Some(RequestHead {
                method,
                path,
                headers,
            }) => {
                let mut request = Request {
                    method,
                    path,
                    headers,
                    body: &mut reader,
                };
                self.route(&mut request, &mut writer)
            }
            None => return Ok(()),
        };
        match outcome {
            Ok(()) => Ok(()),
            Err(refusal) => {
                let body = json!({ "error": refusal.message }).to_string();
                write_response(
                    &mut writer,
                    refusal.status,
                    "application/json",
                    body.as_bytes(),
                )
            }
        }
    }

    fn route(&self, request: &mut Request<'_>, out: &mut TcpStream) -> Result<(), Refusal> {
        let path = request.path.clone();
        match request.method.as_str() {
            "GET" => {
                if let Some(rest) = path.strip_prefix("/v1/index/") {
                    let id = rest
                        .strip_suffix(".json")
                        .ok_or_else(|| Refusal::new(404, "no such index"))?;
                    let id = parse_id(id)?;
                    return self.send_file(out, &self.index_path(&id), "application/json");
                }
                if let Some(rest) = path.strip_prefix("/v1/download/") {
                    let (id, version) = split_versioned(
                        rest.strip_suffix(".tar")
                            .ok_or_else(|| Refusal::new(404, "no such archive"))?,
                    )?;
                    return self.send_file(
                        out,
                        &self.package_path(&id, &version),
                        "application/x-tar",
                    );
                }
                if let Some(name) = path.strip_prefix("/advisories/") {
                    if name.contains('/') || name.starts_with('.') {
                        return Err(Refusal::new(404, "no such advisory file"));
                    }
                    return self.send_file(
                        out,
                        &self.root.join("advisories").join(name),
                        "application/octet-stream",
                    );
                }
                Err(Refusal::new(404, "not found"))
            }
            "POST" => {
                let user = self.authenticate(request)?;
                let reply = if let Some(rest) = path.strip_prefix("/v1/upload/") {
                    let (id, version) = split_versioned(rest)?;
                    self.upload(request, &user, &id, &version)?
                } else if let Some(rest) = path.strip_prefix("/v1/yank/") {
                    let (id, version) = split_versioned(rest)?;
                    let body = request.json_body()?;
                    self.yank(&user, &id, &version, &body)?
                } else if let Some(rest) = path.strip_prefix("/v1/owners/") {
                    let id = parse_id(rest)?;
                    let body = request.json_body()?;
                    self.owners(&user, &id, &body)?
                } else {
                    return Err(Refusal::new(404, "not found"));
                };
                write_response(out, 200, "application/json", reply.to_string().as_bytes())
                    .map_err(Refusal::from)
            }
            _ => Err(Refusal::new(405, "method not allowed")),
        }
    }

    /// The user a request's bearer token names; every request is
    /// [`OPEN_REGISTRY_USER`] on a registry with no tokens.
    fn authenticate(&self, request: &Request<'_>) -> Result<String, Refusal> {
        if self.tokens.is_empty() {
            return Ok(OPEN_REGISTRY_USER.to_string());
        }
        let token = request
            .header("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or_else(|| Refusal::new(401, "a bearer token is required; run `gos login`"))?;
        self.tokens
            .get(token.trim())
            .cloned()
            .ok_or_else(|| Refusal::new(401, "unknown token"))
    }

    fn upload(
        &self,
        request: &mut Request<'_>,
        user: &str,
        id: &ProjectId,
        version: &Version,
    ) -> Result<Value, Refusal> {
        if request.header("x-gossamer-publish-protocol") != Some("2") {
            return Err(Refusal::new(
                400,
                "this registry takes publish protocol 2 (a raw archive body)",
            ));
        }
        let expected = request
            .header("x-gossamer-artifact-sha256")
            .ok_or_else(|| Refusal::new(400, "missing X-Gossamer-Artifact-Sha256"))?
            .to_ascii_lowercase();
        let (signature, public_key) = match (
            request.header("x-gossamer-signature"),
            request.header("x-gossamer-public-key"),
        ) {
            (Some(signature), Some(key)) => (signature.to_string(), key.to_string()),
            _ => {
                return Err(Refusal::new(
                    400,
                    "the upload is unsigned; a registry package must carry its publisher's \
                     signature (configure a key under ~/.gossamer/keys or GOS_PUBLISH_KEY)",
                ));
            }
        };
        if request.header("x-gossamer-signature-input") != Some("sha256") {
            return Err(Refusal::new(
                400,
                "the signature must cover the archive's sha256",
            ));
        }
        let len = request.content_length()?;
        let limit = crate::tar::PackLimits::default().max_archive_bytes;
        if len > limit {
            return Err(Refusal::new(
                413,
                format!("archives are limited to {limit} bytes"),
            ));
        }
        let spool = self.spool_path();
        let signed = SignedDigest {
            sha256: expected,
            signature,
            public_key,
        };
        let result = self
            .receive_archive(request, len, &spool, &signed.sha256)
            .and_then(|()| {
                crate::signing::verify_signature_hex(
                    &signed.public_key,
                    signed.sha256.as_bytes(),
                    &signed.signature,
                )
                .map_err(|_| Refusal::new(400, "the signature does not verify"))?;
                check_archive_manifest(&spool, id, version)?;
                self.admit(user, (id, version), &spool, &signed)
            });
        let _ = fs::remove_file(&spool);
        result?;
        Ok(json!({ "ok": true, "id": id.as_str(), "version": version.to_string() }))
    }

    /// Copies the request body into `spool`, answering whether its digest is
    /// `expected`.
    fn receive_archive(
        &self,
        request: &mut Request<'_>,
        len: usize,
        spool: &Path,
        expected: &str,
    ) -> Result<(), Refusal> {
        if let Some(dir) = spool.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut file = fs::File::create(spool)?;
        let mut hasher = crate::sha256::Hasher::new();
        let mut remaining = len;
        let mut buffer = [0u8; 16 * 1024];
        while remaining > 0 {
            let want = remaining.min(buffer.len());
            let read = request.body.read(&mut buffer[..want])?;
            if read == 0 {
                return Err(Refusal::new(
                    400,
                    "the body ended before its Content-Length",
                ));
            }
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read])?;
            remaining -= read;
        }
        file.sync_all()?;
        if hasher.finalize_hex() != expected {
            return Err(Refusal::new(400, "the archive does not match its sha256"));
        }
        Ok(())
    }

    fn admit(
        &self,
        user: &str,
        (id, version): (&ProjectId, &Version),
        spool: &Path,
        signed: &SignedDigest,
    ) -> Result<(), Refusal> {
        let _writing = self.writes.lock();
        let mut owners = self.read_owners(id)?;
        if owners.is_empty() {
            owners.push(user.to_string());
        } else if !owners.iter().any(|owner| owner == user) {
            return Err(Refusal::new(403, format!("{user} is not an owner of {id}")));
        }
        let mut index = self.read_index(id)?;
        let versions = index_versions(&mut index)?;
        let text = version.to_string();
        if versions
            .iter()
            .any(|entry| entry.get("version").and_then(Value::as_str) == Some(text.as_str()))
        {
            return Err(Refusal::new(
                409,
                format!("{id}@{text} is already published"),
            ));
        }
        let package = self.package_path(id, version);
        if let Some(dir) = package.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::copy(spool, &package)?;
        versions.push(json!({
            "version": text,
            "sha256": signed.sha256,
            "signature": signed.signature,
            "public_key": signed.public_key,
            "yanked": false,
        }));
        self.write_json(&self.index_path(id), &index)?;
        self.write_json(&self.owners_path(id), &json!(owners))?;
        Ok(())
    }

    fn yank(
        &self,
        user: &str,
        id: &ProjectId,
        version: &Version,
        body: &Value,
    ) -> Result<Value, Refusal> {
        let _writing = self.writes.lock();
        self.require_owner(user, id)?;
        let mut index = self.read_index(id)?;
        let text = version.to_string();
        let entry = index_versions(&mut index)?
            .iter_mut()
            .find(|entry| entry.get("version").and_then(Value::as_str) == Some(text.as_str()))
            .ok_or_else(|| Refusal::new(404, format!("{id}@{text} is not published")))?;
        entry["yanked"] = json!(true);
        let reason = body.get("reason").and_then(Value::as_str).unwrap_or("");
        if !reason.is_empty() {
            entry["yank_reason"] = json!(reason);
        }
        self.write_json(&self.index_path(id), &index)?;
        Ok(json!({ "ok": true }))
    }

    fn owners(&self, user: &str, id: &ProjectId, body: &Value) -> Result<Value, Refusal> {
        let _writing = self.writes.lock();
        let mut owners = self.require_owner(user, id)?;
        let op = body.get("op").and_then(Value::as_str).unwrap_or("");
        let named = body.get("user").and_then(Value::as_str).unwrap_or("");
        match op {
            "list" => return Ok(json!({ "owners": owners })),
            "add" if !named.is_empty() => {
                if !owners.iter().any(|owner| owner == named) {
                    owners.push(named.to_string());
                }
            }
            "remove" if !named.is_empty() => {
                owners.retain(|owner| owner != named);
                if owners.is_empty() {
                    return Err(Refusal::new(
                        400,
                        format!("{id} must keep at least one owner"),
                    ));
                }
            }
            _ => {
                return Err(Refusal::new(
                    400,
                    "expected op add/remove with a user, or list",
                ));
            }
        }
        self.write_json(&self.owners_path(id), &json!(owners))?;
        Ok(json!({ "owners": owners }))
    }

    fn require_owner(&self, user: &str, id: &ProjectId) -> Result<Vec<String>, Refusal> {
        let owners = self.read_owners(id)?;
        if owners.is_empty() {
            return Err(Refusal::new(404, format!("{id} is not published")));
        }
        if !owners.iter().any(|owner| owner == user) {
            return Err(Refusal::new(403, format!("{user} is not an owner of {id}")));
        }
        Ok(owners)
    }

    fn read_owners(&self, id: &ProjectId) -> Result<Vec<String>, Refusal> {
        match fs::read(self.owners_path(id)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| Refusal::new(500, format!("owners file for {id}: {e}"))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn read_index(&self, id: &ProjectId) -> Result<Value, Refusal> {
        match fs::read(self.index_path(id)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| Refusal::new(500, format!("index for {id}: {e}"))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(json!({ "versions": [] })),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes `value` to `path` through a sibling file and a rename, so a
    /// reader sees the old document or the new one and never a partial one.
    fn write_json(&self, path: &Path, value: &Value) -> Result<(), Refusal> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let staged = path.with_extension("json.partial");
        fs::write(&staged, value.to_string())?;
        fs::rename(&staged, path)?;
        Ok(())
    }

    fn send_file(
        &self,
        out: &mut TcpStream,
        path: &Path,
        content_type: &str,
    ) -> Result<(), Refusal> {
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(Refusal::new(404, "not found"));
            }
            Err(e) => return Err(e.into()),
        };
        let len = file.metadata()?.len();
        write!(
            out,
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\n\
             Connection: close\r\n\r\n"
        )?;
        io::copy(&mut file, out)?;
        out.flush()?;
        Ok(())
    }

    fn index_path(&self, id: &ProjectId) -> PathBuf {
        self.root
            .join("index")
            .join(format!("{}.json", id.as_str()))
    }

    fn owners_path(&self, id: &ProjectId) -> PathBuf {
        self.root
            .join("owners")
            .join(format!("{}.json", id.as_str()))
    }

    fn package_path(&self, id: &ProjectId, version: &Version) -> PathBuf {
        self.root
            .join("packages")
            .join(id.as_str())
            .join(format!("{version}.tar"))
    }

    fn spool_path(&self) -> PathBuf {
        let n = self.spool_ids.fetch_add(1, Ordering::Relaxed);
        self.root
            .join("spool")
            .join(format!("upload-{}-{n}.tar", std::process::id()))
    }
}

/// The `[tokens]` table of a registry's `tokens.toml`, keyed by token.
fn parse_tokens(text: &str) -> io::Result<BTreeMap<String, String>> {
    let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidData, message);
    let document: toml::Table =
        toml::from_str(text).map_err(|e| invalid(format!("tokens.toml: {e}")))?;
    let mut tokens = BTreeMap::new();
    if let Some(table) = document.get("tokens") {
        let table = table
            .as_table()
            .ok_or_else(|| invalid("tokens.toml: `tokens` must be a table".to_string()))?;
        for (user, token) in table {
            let token = token.as_str().ok_or_else(|| {
                invalid(format!(
                    "tokens.toml: the token for {user} must be a string"
                ))
            })?;
            tokens.insert(token.to_string(), user.clone());
        }
    }
    Ok(tokens)
}

/// A request line and its headers, names lowercased.
struct RequestHead {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
}

/// Reads a request line and headers, or `None` when the peer closed the
/// connection before sending one.
fn read_head(reader: &mut BufReader<TcpStream>) -> io::Result<Option<RequestHead>> {
    let Some(line) = read_line(reader)? else {
        return Ok(None);
    };
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed request line",
        ));
    };
    let path = target.split('?').next().unwrap_or(target).to_string();
    let mut headers = BTreeMap::new();
    loop {
        let Some(line) = read_line(reader)? else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "headers ended early",
            ));
        };
        if line.is_empty() {
            break;
        }
        if headers.len() == MAX_HEADERS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many headers",
            ));
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    Ok(Some(RequestHead {
        method: method.to_string(),
        path,
        headers,
    }))
}

fn read_line(reader: &mut BufReader<TcpStream>) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    let read = reader
        .by_ref()
        .take(MAX_HEADER_LINE as u64 + 2)
        .read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if !line.ends_with(b"\n") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "header line too long",
        ));
    }
    let text = String::from_utf8(line)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "header is not UTF-8"))?;
    Ok(Some(text.trim_end_matches(['\r', '\n']).to_string()))
}

fn write_response(
    out: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        _ => "Internal Server Error",
    };
    write!(
        out,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )?;
    out.write_all(body)?;
    out.flush()
}

fn parse_id(text: &str) -> Result<ProjectId, Refusal> {
    ProjectId::parse(text).map_err(|e| Refusal::new(400, format!("invalid project id: {e}")))
}

/// Splits `<id>/<version>` at its last `/`.
fn split_versioned(text: &str) -> Result<(ProjectId, Version), Refusal> {
    let (id, version) = text
        .rsplit_once('/')
        .ok_or_else(|| Refusal::new(404, "expected <id>/<version>"))?;
    let version =
        Version::parse(version).map_err(|e| Refusal::new(400, format!("invalid version: {e}")))?;
    Ok((parse_id(id)?, version))
}

fn index_versions(index: &mut Value) -> Result<&mut Vec<Value>, Refusal> {
    index
        .get_mut("versions")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| Refusal::new(500, "index document has no `versions` array"))
}

/// Refuses an archive whose manifest does not name the id and version it is
/// published under, so an index entry always describes the package it names.
fn check_archive_manifest(spool: &Path, id: &ProjectId, version: &Version) -> Result<(), Refusal> {
    let file = fs::File::open(spool)?;
    let files = crate::tar::unpack_reader(file)
        .map_err(|e| Refusal::new(400, format!("the archive does not unpack: {e}")))?;
    let manifest = files
        .get("project.toml")
        .ok_or_else(|| Refusal::new(400, "the archive has no project.toml"))?;
    let text = std::str::from_utf8(manifest)
        .map_err(|_| Refusal::new(400, "project.toml is not UTF-8"))?;
    let manifest = crate::manifest::Manifest::parse(text)
        .map_err(|e| Refusal::new(400, format!("project.toml: {e}")))?;
    if manifest.project.id != *id || manifest.project.version != *version {
        return Err(Refusal::new(
            400,
            format!(
                "the archive is {}@{}, not {id}@{version}",
                manifest.project.id, manifest.project.version
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_tokens, split_versioned};

    #[test]
    fn a_versioned_path_splits_at_its_last_slash() {
        let (id, version) = split_versioned("example.com/widget/1.2.3").unwrap();
        assert_eq!(id.as_str(), "example.com/widget");
        assert_eq!(version.to_string(), "1.2.3");
    }

    #[test]
    fn a_path_that_climbs_out_is_refused() {
        assert!(split_versioned("example.com/../escape/1.0.0").is_err());
    }

    #[test]
    fn tokens_are_keyed_by_token() {
        let tokens = parse_tokens("[tokens]\nalice = \"t-1\"\nbob = \"t-2\"\n").unwrap();
        assert_eq!(tokens.get("t-1").map(String::as_str), Some("alice"));
        assert_eq!(tokens.get("t-2").map(String::as_str), Some("bob"));
    }
}
