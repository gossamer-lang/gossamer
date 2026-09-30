# `std::jwt`

Status: experimental

RFC 7519 tokens. Signs with HS256 / HS384 / HS512, ES256, and EdDSA; verifies those plus the RS256 / RS384 / RS512 family every mainstream identity provider mints with. Claims cross the boundary as JSON text.

## Items

| Item | Signature | Description |
|---|---|---|
| `verify` | `fn verify(token: String, alg: String, key: String, leeway_secs: i64, issuer: String, audience: String) -> Result<String, errors::Error>` | `verify(token, alg, key, leeway_secs, issuer, audience) -> Result<String, errors::Error>` - the verifier a service protecting an endpoint uses. One entry point for every algorithm, RS* included; `key` is the shared secret for HS* and the PEM public key for the rest. `issuer` and `audience` are enforced when non-empty - leaving them empty accepts a token minted for another service by anyone sharing the key. |
| `header` | `fn header(token: String) -> Result<String, errors::Error>` | `header(token) -> Result<String, errors::Error>` - the JOSE header as JSON, read WITHOUT verifying the signature. Read `kid` to choose a key from a key set; nothing in it is trustworthy until `verify` succeeds. |
| `sign_hs` | `fn sign_hs(alg: String, claims_json: String, key: Vec<u8>) -> Result<String, errors::Error>` | Sign claims with HMAC-SHA family using a shared key. |
| `verify_hs` | `fn verify_hs(token: String, alg: String, key: Vec<u8>, leeway_secs: i64) -> Result<String, errors::Error>` | Verify an HS* token against a shared key with a clock-skew allowance. Prefer `verify`, which also enforces the issuer and the audience. |
| `sign_es256` | `fn sign_es256(claims_json: String, signing_key_pem: String) -> Result<String, errors::Error>` | Sign with ECDSA P-256 from a PEM-encoded private key. |
| `verify_es256` | `fn verify_es256(token: String, verifying_key_pem: String, leeway_secs: i64) -> Result<String, errors::Error>` | Verify an ES256 token against a PEM-encoded public key. Prefer `verify`, which also enforces the issuer and the audience. |
| `sign_eddsa` | `fn sign_eddsa(claims_json: String, signing_key_pem: String) -> Result<String, errors::Error>` | Sign with Ed25519 from a PEM-encoded private key. |
| `verify_eddsa` | `fn verify_eddsa(token: String, verifying_key_pem: String, leeway_secs: i64) -> Result<String, errors::Error>` | Verify an EdDSA token against a PEM-encoded public key. Prefer `verify`, which also enforces the issuer and the audience. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
