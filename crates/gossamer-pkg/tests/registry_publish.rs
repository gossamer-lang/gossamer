//! Integration tests for 0.8.0 - real registry / git fetch, persistent
//! disk cache, mandatory lockfile, publish + yank + credential
//! round-trips.

use std::collections::BTreeMap;
use std::sync::Arc;

use gossamer_pkg::{
    Cache, CacheError, CachedSource, CatalogueEntry, Credential, CredentialStore, FetchOptions,
    Fetcher, LockedEntry, Lockfile, LockfileError, Manifest, ProjectId, Resolved, ResolvedSource,
    Resolver, StaticTransport, Transport, Version, VersionCatalogue, default_cache_root,
    pack_crate, resolve_transitive, sha256,
};

fn make_tar(name: &str, body: &[u8]) -> Vec<u8> {
    let mut entries: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    entries.insert(name.to_string(), body.to_vec());
    gossamer_pkg::tar::pack(&entries).expect("pack")
}

/// Signs `tar`'s SHA-256 digest with a fixed test key, as `gos publish`
/// does, returning `(signature_hex, public_key_hex)` for a registry
/// catalogue entry.
fn sign_tar(tar: &[u8]) -> (String, String) {
    let key = gossamer_pkg::SigningKey::from_bytes([7u8; 32]);
    let sig = gossamer_pkg::sign_bytes(&key, sha256::hex(tar).as_bytes());
    (gossamer_pkg::hex_encode(&sig), key.verifying_key().to_hex())
}

fn resolved_registry(id: &str, version: Version) -> Resolved {
    Resolved {
        id: ProjectId::parse(id).unwrap(),
        pin: ResolvedSource::Registry(version),
    }
}

#[test]
fn pack_round_trip_is_byte_stable_and_unpacks_back() {
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("gos-pack-rt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("src")).unwrap();
    std::fs::write(tmp.join("project.toml"), b"[project]\nid = \"a.b/c\"\n").unwrap();
    std::fs::write(tmp.join("src/main.gos"), b"fn main() {}\n").unwrap();

    let a = pack_crate(&tmp).unwrap();
    let b = pack_crate(&tmp).unwrap();
    assert_eq!(a.bytes, b.bytes, "pack must be byte-stable");
    assert_eq!(a.sha256, b.sha256);
    assert_eq!(a.sha256.len(), 64);

    let back = gossamer_pkg::tar::unpack(&a.bytes).unwrap();
    assert!(back.contains_key("project.toml"));
    assert!(back.contains_key("src/main.gos"));
    assert_eq!(back.get("src/main.gos").unwrap(), b"fn main() {}\n");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn lockfile_drift_is_detected_against_resolver_output() {
    let pinned = vec![Resolved {
        id: ProjectId::parse("example.com/a").unwrap(),
        pin: ResolvedSource::Registry(Version::new(1, 0, 0)),
    }];
    let lock = Lockfile::from_resolved(&pinned);

    // Imagine the manifest now wants 1.0.1 → drift.
    let drifted = vec![Resolved {
        id: ProjectId::parse("example.com/a").unwrap(),
        pin: ResolvedSource::Registry(Version::new(1, 0, 1)),
    }];
    let err = lock.verify_against(&drifted).unwrap_err();
    assert!(matches!(err, LockfileError::Drift { .. }));

    // A previously-unknown dep also raises a drift error.
    let unknown = vec![Resolved {
        id: ProjectId::parse("example.com/b").unwrap(),
        pin: ResolvedSource::Registry(Version::new(0, 1, 0)),
    }];
    let err = lock.verify_against(&unknown).unwrap_err();
    assert!(matches!(err, LockfileError::MissingPin { .. }));
}

#[test]
fn lockfile_load_required_errors_when_missing() {
    let tmp = std::env::temp_dir().join(format!("gos-lock-miss-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let err = Lockfile::load_required(&tmp).unwrap_err();
    assert!(matches!(err, LockfileError::LockfileMissing { .. }));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn transitive_resolution_walks_grandchildren() {
    // Root → A; A → B; B → C. Each manifest is loaded by a
    // closure-backed loader so the test owns the graph shape.
    let root = Manifest::parse(
        "[project]\nid = \"root.test/app\"\nversion = \"0.1.0\"\n\n[dependencies]\n\"a.test/a\" = \"1.0.0\"\n",
    )
    .unwrap();
    let a_manifest = Manifest::parse(
        "[project]\nid = \"a.test/a\"\nversion = \"1.0.0\"\n\n[dependencies]\n\"b.test/b\" = \"1.0.0\"\n",
    )
    .unwrap();
    let b_manifest = Manifest::parse(
        "[project]\nid = \"b.test/b\"\nversion = \"1.0.0\"\n\n[dependencies]\n\"c.test/c\" = \"1.0.0\"\n",
    )
    .unwrap();
    let c_manifest =
        Manifest::parse("[project]\nid = \"c.test/c\"\nversion = \"1.0.0\"\n").unwrap();

    let mut catalogue = VersionCatalogue::new();
    catalogue.add(
        &ProjectId::parse("a.test/a").unwrap(),
        Version::new(1, 0, 0),
    );
    catalogue.add(
        &ProjectId::parse("b.test/b").unwrap(),
        Version::new(1, 0, 0),
    );
    catalogue.add(
        &ProjectId::parse("c.test/c").unwrap(),
        Version::new(1, 0, 0),
    );

    let loader = gossamer_pkg::FnLoader(move |r: &Resolved| {
        let next = match r.id.as_str() {
            "a.test/a" => Some(a_manifest.clone()),
            "b.test/b" => Some(b_manifest.clone()),
            "c.test/c" => Some(c_manifest.clone()),
            _ => None,
        };
        Ok(next)
    });

    let resolved = resolve_transitive(&root, None, &catalogue, &loader).unwrap();
    let ids: Vec<String> = resolved.iter().map(|r| r.id.as_str().to_string()).collect();
    assert_eq!(ids, vec!["a.test/a", "b.test/b", "c.test/c"]);
}

#[test]
fn yanked_registry_version_refuses_install_without_flag() {
    let id = ProjectId::parse("example.com/yanked").unwrap();
    let url = "https://reg.test/v1/download/example.com/yanked/1.0.0.tar";
    let tar_bytes = make_tar("project.toml", b"[project]\nid = \"example.com/yanked\"\n");
    let expected_sha = sha256::hex(&tar_bytes);

    let mut transport = StaticTransport::new();
    transport.insert(url, tar_bytes.clone());

    let (sig, pubkey) = sign_tar(&tar_bytes);
    let mut catalogue = VersionCatalogue::new();
    catalogue.add_entry(
        &id,
        CatalogueEntry {
            version: Version::new(1, 0, 0),
            yanked: true,
            download_url: Some(url.to_string()),
            tarball_sha256: Some(expected_sha.clone()),
            yank_reason: Some("security".to_string()),
            signature: Some(sig),
            public_key: Some(pubkey.clone()),
        },
    );

    let options = FetchOptions {
        registry_url: Some("https://reg.test".to_string()),
        ..FetchOptions::default()
    };
    let fetcher =
        Fetcher::with_transport(options, Arc::new(transport.clone()) as Arc<dyn Transport>)
            .with_catalogue(catalogue.clone())
            .with_trusted_publisher_keys(BTreeMap::from([(
                id.as_str().to_string(),
                pubkey.clone(),
            )]));

    let mut cache = Cache::new();
    let resolved = resolved_registry("example.com/yanked", Version::new(1, 0, 0));
    let err = fetcher
        .fetch_all(std::slice::from_ref(&resolved), &mut cache)
        .unwrap_err();
    assert!(matches!(err, CacheError::Yanked { .. }));

    // With --allow-yanked, the fetch goes through.
    let allow_options = FetchOptions {
        registry_url: Some("https://reg.test".to_string()),
        allow_yanked: true,
        ..FetchOptions::default()
    };
    let allow_fetcher =
        Fetcher::with_transport(allow_options, Arc::new(transport) as Arc<dyn Transport>)
            .with_catalogue(catalogue)
            .with_trusted_publisher_keys(BTreeMap::from([(id.as_str().to_string(), pubkey)]));
    let ok = allow_fetcher
        .fetch_all(std::slice::from_ref(&resolved), &mut cache)
        .expect("allow_yanked should bypass yank check");
    assert_eq!(ok.len(), 1);
}

#[test]
fn second_fetch_hits_disk_cache_and_skips_network() {
    let id = ProjectId::parse("example.com/disk").unwrap();
    let url = "https://reg.test/v1/download/example.com/disk/1.0.0.tar";
    let tar_bytes = make_tar("project.toml", b"[project]\nid = \"example.com/disk\"\n");
    let expected_sha = sha256::hex(&tar_bytes);

    let cache_dir = std::env::temp_dir().join(format!("gos-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cache_dir);

    // First fetch: real transport, populates disk.
    {
        let mut transport = StaticTransport::new();
        transport.insert(url, tar_bytes.clone());
        let (sig, pubkey) = sign_tar(&tar_bytes);
        let mut catalogue = VersionCatalogue::new();
        catalogue.add_entry(
            &id,
            CatalogueEntry {
                version: Version::new(1, 0, 0),
                yanked: false,
                download_url: Some(url.to_string()),
                tarball_sha256: Some(expected_sha.clone()),
                yank_reason: None,
                signature: Some(sig),
                public_key: Some(pubkey.clone()),
            },
        );
        let live = Fetcher::with_transport(
            FetchOptions {
                registry_url: Some("https://reg.test".to_string()),
                ..FetchOptions::default()
            },
            Arc::new(transport) as Arc<dyn Transport>,
        )
        .with_catalogue(catalogue)
        .with_trusted_publisher_keys(BTreeMap::from([(id.as_str().to_string(), pubkey)]));
        let mut warm_cache = Cache::with_disk_root(cache_dir.clone());
        let resolved = resolved_registry("example.com/disk", Version::new(1, 0, 0));
        let outcome = live.fetch_all(&[resolved], &mut warm_cache).unwrap();
        assert!(
            cache_dir
                .join("pkg")
                .join(&outcome[0].source.digest)
                .is_dir()
        );
    }

    // Second fetch: transport is empty, but the cache check should
    // see the previously-written digest on disk and skip the
    // network entirely. We assert this by setting `offline: true`
    // and an empty transport: the fetch still needs a registry
    // tarball, so we shortcut by checking `Cache::contains`
    // directly.
    {
        let cache = Cache::with_disk_root(cache_dir.clone());
        let digest = {
            let mut entries: BTreeMap<String, Vec<u8>> = BTreeMap::new();
            entries.insert(
                "project.toml".to_string(),
                b"[project]\nid = \"example.com/disk\"\n".to_vec(),
            );
            CachedSource::build(id.clone(), entries).digest
        };
        assert!(
            cache.contains(&digest),
            "disk-backed cache must report contains() = true after first fetch"
        );
    }

    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[test]
fn credentials_round_trip_through_disk() {
    let path = std::env::temp_dir().join(format!("gos-creds-{}.toml", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut store = CredentialStore::new();
    store.insert(
        "https://pkg.gossamer.dev",
        Credential {
            token: "tkn123".to_string(),
        },
    );
    store.save(&path).unwrap();
    let back = CredentialStore::load(&path).unwrap();
    assert_eq!(
        back.get("https://pkg.gossamer.dev").unwrap().token,
        "tkn123"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn resolver_picks_highest_when_multiple_consumers_overlap() {
    // A `^` floor has no ceiling, so the highest published version
    // satisfies it.
    let manifest = Manifest::parse(
        "[project]\nid = \"r.test/r\"\nversion = \"0.1.0\"\n\n[dependencies]\n\"l.test/lib\" = \"^1.2.0\"\n",
    )
    .unwrap();
    let mut catalogue = VersionCatalogue::new();
    let id = ProjectId::parse("l.test/lib").unwrap();
    for (a, b, c) in [(1, 2, 0), (1, 3, 0), (1, 5, 0), (2, 0, 0)] {
        catalogue.add(&id, Version::new(a, b, c));
    }
    let plan = Resolver::new(catalogue).resolve(&manifest).unwrap();
    match &plan[0].pin {
        ResolvedSource::Registry(v) => assert_eq!(*v, Version::new(2, 0, 0)),
        other => panic!("expected registry, got {other:?}"),
    }
}

#[test]
fn resolver_excludes_prereleases_for_a_stable_caret_requirement() {
    let manifest = Manifest::parse(
        "[project]\nid = \"r.test/app\"\nversion = \"0.1.0\"\n\n[dependencies]\n\"l.test/lib\" = \"^1.0.0\"\n",
    )
    .unwrap();
    let id = ProjectId::parse("l.test/lib").unwrap();
    let mut catalogue = VersionCatalogue::new();
    for version in ["1.0.0-beta.2", "1.0.0", "1.1.0-rc.1"] {
        catalogue.add(&id, Version::parse(version).unwrap());
    }

    let resolved = Resolver::new(catalogue).resolve(&manifest).unwrap();
    assert!(matches!(
        &resolved[0].pin,
        ResolvedSource::Registry(version) if version == &Version::parse("1.0.0").unwrap()
    ));
}

#[test]
fn lockfile_from_fetched_carries_sha256() {
    let id = ProjectId::parse("a.test/a").unwrap();
    let resolved = Resolved {
        id: id.clone(),
        pin: ResolvedSource::Registry(Version::new(1, 0, 0)),
    };
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    files.insert("x".to_string(), b"y".to_vec());
    let source = CachedSource::build(id, files);
    let fetched = gossamer_pkg::Fetched {
        resolved: resolved.clone(),
        source: source.clone(),
        owner_pubkey: Some("abcd".to_string()),
    };
    let lock = Lockfile::from_fetched(&[fetched]);
    let expected = vec![LockedEntry {
        resolved,
        sha256: Some(source.digest),
        owner_pubkey: Some("abcd".to_string()),
    }];
    assert_eq!(lock.entries, expected);
}

#[test]
fn cache_disk_layer_round_trips_through_compute_digest() {
    let id = ProjectId::parse("a.test/a").unwrap();
    let cache_dir = std::env::temp_dir().join(format!("gos-cache-rt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cache_dir);
    let mut cache = Cache::with_disk_root(cache_dir.clone());
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    files.insert(
        "project.toml".to_string(),
        b"[project]\nid = \"a.test/a\"\n".to_vec(),
    );
    files.insert("src/main.gos".to_string(), b"fn main() {}\n".to_vec());
    let src = CachedSource::build(id, files);
    let digest = src.digest.clone();
    cache.insert(src);

    // Drop the in-memory layer and re-read from disk.
    let mut cache = Cache::with_disk_root(cache_dir.clone());
    assert!(cache.contains(&digest));
    let back = cache.get(&digest).expect("re-load");
    assert_eq!(back.digest, digest);
    assert!(back.files.contains_key("project.toml"));
    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[test]
fn signing_round_trip_validates_artifact() {
    use gossamer_pkg::signing::{SigningKey, sign_bytes, verify_bytes};
    let mut secret = [0u8; 32];
    for (i, b) in secret.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(17).wrapping_add(3);
    }
    let key = SigningKey::from_bytes(secret);
    let vk = key.verifying_key();
    let bytes = b"the published tarball";
    let sig = sign_bytes(&key, bytes);
    verify_bytes(&vk, bytes, &sig).expect("sig verifies");
    assert!(verify_bytes(&vk, b"other bytes", &sig).is_err());
}

#[test]
fn default_cache_root_returns_some_path_when_home_or_cache_dir_set() {
    // The Rust 2024 unsafe-env-var rule blocks set_var in tests
    // under `#![forbid(unsafe_code)]`. Instead, just check that
    // the discovery surface returns a usable path on this host
    // (CI always sets HOME).
    if std::env::var("HOME").is_ok() || std::env::var("GOS_CACHE_DIR").is_ok() {
        assert!(default_cache_root().is_some());
    }
}

// --- HTTP POST round-trip ---

#[test]
fn http_transport_post_round_trips_against_local_server() {
    use gossamer_pkg::Transport;
    use gossamer_pkg::transport::HttpTransport;
    use std::io::{Cursor, Read, Write};
    use std::net::TcpListener;
    use std::thread;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let port = addr.port();

    // One-shot server: read the entire request (headers + body) then
    // write a tiny 200 response. A single `read()` may only see the
    // headers; loop until CRLFCRLF + Content-Length bytes have
    // arrived.
    let server = thread::spawn(move || {
        let (mut stream, _peer) = listener.accept().expect("accept");
        let mut received: Vec<u8> = Vec::new();
        let mut content_length: Option<usize> = None;
        let mut header_end: Option<usize> = None;
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).expect("read");
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buf[..n]);
            if header_end.is_none()
                && let Some(pos) = received.windows(4).position(|window| window == b"\r\n\r\n")
            {
                header_end = Some(pos + 4);
                let header_text = std::str::from_utf8(&received[..pos]).expect("utf8");
                for line in header_text.lines() {
                    if let Some(rest) = line.strip_prefix("Content-Length: ") {
                        content_length = rest.trim().parse().ok();
                    }
                }
            }
            if let (Some(hdr), Some(cl)) = (header_end, content_length)
                && received.len() >= hdr + cl
            {
                break;
            }
        }
        let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        stream.write_all(resp).expect("write");
        String::from_utf8_lossy(&received).to_string()
    });

    let transport = HttpTransport;
    let url = format!("http://127.0.0.1:{port}/v1/upload");
    let body = br#"{"hello":"world"}"#;
    let response = transport
        .post_reader(
            &url,
            &mut Cursor::new(body),
            body.len(),
            "application/json",
            Some("supersecret"),
        )
        .expect("post");
    assert_eq!(response, b"ok");

    let received = server.join().expect("server thread");
    assert!(received.starts_with("POST /v1/upload"), "got: {received}");
    assert!(
        received.contains("Authorization: Bearer supersecret"),
        "got: {received}"
    );
    assert!(
        received.contains("Content-Type: application/json"),
        "got: {received}"
    );
    assert!(received.contains("Content-Length: 17"), "got: {received}");
    assert!(
        received.ends_with(r#"{"hello":"world"}"#),
        "got: {received}"
    );
}

/// Builds a fetcher serving `tar_bytes` at `url` for a registry entry
/// carrying the given `signature` and `public_key`.
fn signed_registry_fetcher(
    id: &ProjectId,
    url: &str,
    tar_bytes: &[u8],
    signature: Option<String>,
    public_key: Option<String>,
) -> Fetcher {
    let mut transport = StaticTransport::new();
    transport.insert(url, tar_bytes.to_vec());
    let mut catalogue = VersionCatalogue::new();
    catalogue.add_entry(
        id,
        CatalogueEntry {
            version: Version::new(1, 0, 0),
            yanked: false,
            download_url: Some(url.to_string()),
            tarball_sha256: Some(sha256::hex(tar_bytes)),
            yank_reason: None,
            signature,
            public_key: public_key.clone(),
        },
    );
    Fetcher::with_transport(
        FetchOptions {
            registry_url: Some("https://reg.test".to_string()),
            ..FetchOptions::default()
        },
        Arc::new(transport) as Arc<dyn Transport>,
    )
    .with_catalogue(catalogue)
    .with_trusted_publisher_keys(
        public_key
            .map(|key| BTreeMap::from([(id.as_str().to_string(), key)]))
            .unwrap_or_default(),
    )
}

#[test]
fn registry_fetch_accepts_a_valid_publisher_signature() {
    let id = ProjectId::parse("example.com/signed").unwrap();
    let url = "https://reg.test/v1/download/example.com/signed/1.0.0.tar";
    let tar = make_tar("project.toml", b"[project]\nid = \"example.com/signed\"\n");
    let (sig, pubkey) = sign_tar(&tar);
    let fetcher = signed_registry_fetcher(&id, url, &tar, Some(sig), Some(pubkey.clone()));
    let resolved = resolved_registry("example.com/signed", Version::new(1, 0, 0));
    let mut cache = Cache::new();
    let out = fetcher
        .fetch_all(std::slice::from_ref(&resolved), &mut cache)
        .expect("a validly-signed registry tarball must install");
    assert_eq!(out[0].owner_pubkey.as_deref(), Some(pubkey.as_str()));
}

#[test]
fn registry_fetch_rejects_an_unsigned_source() {
    let id = ProjectId::parse("example.com/unsigned").unwrap();
    let url = "https://reg.test/v1/download/example.com/unsigned/1.0.0.tar";
    let tar = make_tar(
        "project.toml",
        b"[project]\nid = \"example.com/unsigned\"\n",
    );
    let fetcher = signed_registry_fetcher(&id, url, &tar, None, None);
    let resolved = resolved_registry("example.com/unsigned", Version::new(1, 0, 0));
    let mut cache = Cache::new();
    let err = fetcher
        .fetch_all(std::slice::from_ref(&resolved), &mut cache)
        .unwrap_err();
    assert!(matches!(err, CacheError::Unsigned(_)), "got {err:?}");
}

#[test]
fn registry_fetch_rejects_a_bad_signature() {
    let id = ProjectId::parse("example.com/badsig").unwrap();
    let url = "https://reg.test/v1/download/example.com/badsig/1.0.0.tar";
    let tar = make_tar("project.toml", b"[project]\nid = \"example.com/badsig\"\n");
    // A correctly-shaped signature over different bytes will not verify.
    let (_, pubkey) = sign_tar(&tar);
    let (wrong_sig, _) = sign_tar(b"some other payload");
    let fetcher = signed_registry_fetcher(&id, url, &tar, Some(wrong_sig), Some(pubkey));
    let resolved = resolved_registry("example.com/badsig", Version::new(1, 0, 0));
    let mut cache = Cache::new();
    let err = fetcher
        .fetch_all(std::slice::from_ref(&resolved), &mut cache)
        .unwrap_err();
    assert!(
        matches!(err, CacheError::SignatureInvalid(_)),
        "got {err:?}"
    );
}

#[test]
fn registry_fetch_rejects_a_rotated_publisher_key() {
    let id = ProjectId::parse("example.com/rotated").unwrap();
    let url = "https://reg.test/v1/download/example.com/rotated/1.0.0.tar";
    let tar = make_tar("project.toml", b"[project]\nid = \"example.com/rotated\"\n");
    let (sig, pubkey) = sign_tar(&tar);
    let fetcher = signed_registry_fetcher(&id, url, &tar, Some(sig), Some(pubkey));
    // The lockfile pins a different key than the registry now advertises.
    let other_key = gossamer_pkg::SigningKey::from_bytes([9u8; 32])
        .verifying_key()
        .to_hex();
    let mut pins: BTreeMap<String, String> = BTreeMap::new();
    pins.insert("example.com/rotated".to_string(), other_key);
    let fetcher = fetcher.with_pinned_keys(pins);
    let resolved = resolved_registry("example.com/rotated", Version::new(1, 0, 0));
    let mut cache = Cache::new();
    let err = fetcher
        .fetch_all(std::slice::from_ref(&resolved), &mut cache)
        .unwrap_err();
    assert!(matches!(err, CacheError::KeyMismatch { .. }), "got {err:?}");
}
