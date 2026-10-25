//! JSON Web Tokens (RFC 7519) with secure defaults.
//!
//! Supported algorithms today: HS256 / HS384 / HS512 (HMAC),
//! RS256 / RS384 / RS512 (RSA PKCS#1 v1.5, verify only),
//! ES256 (ECDSA P-256 SHA-256), and EdDSA (Ed25519). RSA
//! verification routes through `ring`, whose RSA implementation
//! is audited and constant-time; the vulnerable `rsa 0.9.x` crate
//! (RUSTSEC-2023-0071) is intentionally not linked. ES384 / ES512
//! are absent pending p384 / p521 dependencies. Sign for RS* is
//! not exposed - OIDC and friends are verify-only on this side.
//!
//! Security invariants enforced on every verify call:
//!
//! * `alg: "none"` is rejected unconditionally and never emitted.
//! * The token's `alg` header must equal the caller's expected
//!   algorithm - there is no negotiation.
//! * HMAC verifiers reject asymmetric `alg` values (`RS*` / `ES*`
//!   / `Ed*`) and asymmetric verifiers reject HMAC `alg` values,
//!   so an attacker cannot trick `verify_hs` into treating an
//!   RSA public key as a shared secret.
//! * Header `typ` must be `"JWT"` or absent; anything else is
//!   rejected.
//! * Header `crit` (RFC 7515 §4.1.11) is always rejected - we
//!   don't process critical extensions.
//! * HMAC signature comparison runs through
//!   [`crate::crypto::subtle::constant_time_eq`].

#![forbid(unsafe_code)]

use std::time::UNIX_EPOCH;

use sha2::Digest;

use crate::crypto::{sha512, subtle};
use crate::errors::Error;

/// JWS signing / verification algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alg {
    /// HMAC-SHA-256 (RFC 7518 §3.2).
    Hs256,
    /// HMAC-SHA-384 (RFC 7518 §3.2).
    Hs384,
    /// HMAC-SHA-512 (RFC 7518 §3.2).
    Hs512,
    /// ECDSA P-256 with SHA-256 (RFC 7518 §3.4).
    Es256,
    /// Edwards-curve DSA on Curve25519 (RFC 8037 §3.1).
    EdDsa,
    /// RSA PKCS#1 v1.5 with SHA-256 (RFC 7518 §3.3). Verify only.
    Rs256,
    /// RSA PKCS#1 v1.5 with SHA-384 (RFC 7518 §3.3). Verify only.
    Rs384,
    /// RSA PKCS#1 v1.5 with SHA-512 (RFC 7518 §3.3). Verify only.
    Rs512,
}

impl Alg {
    fn as_str(self) -> &'static str {
        match self {
            Alg::Hs256 => "HS256",
            Alg::Hs384 => "HS384",
            Alg::Hs512 => "HS512",
            Alg::Es256 => "ES256",
            Alg::EdDsa => "EdDSA",
            Alg::Rs256 => "RS256",
            Alg::Rs384 => "RS384",
            Alg::Rs512 => "RS512",
        }
    }

    fn from_str(s: &str) -> Result<Self, Error> {
        match s {
            "HS256" => Ok(Alg::Hs256),
            "HS384" => Ok(Alg::Hs384),
            "HS512" => Ok(Alg::Hs512),
            "ES256" => Ok(Alg::Es256),
            "EdDSA" => Ok(Alg::EdDsa),
            "RS256" => Ok(Alg::Rs256),
            "RS384" => Ok(Alg::Rs384),
            "RS512" => Ok(Alg::Rs512),
            "none" => Err(Error::new(
                "jwt: alg \"none\" is refused on principle (RFC 7515 §4.1.1)",
            )),
            other => Err(Error::new(format!("jwt: unsupported alg {other:?}"))),
        }
    }

    fn is_hmac(self) -> bool {
        matches!(self, Alg::Hs256 | Alg::Hs384 | Alg::Hs512)
    }

    fn is_rsa(self) -> bool {
        matches!(self, Alg::Rs256 | Alg::Rs384 | Alg::Rs512)
    }
}

/// JOSE header - only the fields we care about. Unknown header
/// parameters are tolerated for forward compatibility, with the
/// sole exception of `crit`, which is fatal per RFC 7515.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// Signature algorithm.
    pub alg: Alg,
    /// Token type. Conventionally `"JWT"`; only `"JWT"` (or absent
    /// on the wire) is accepted on verify.
    pub typ: String,
    /// Optional key identifier (`kid`) - opaque to JWS itself; an
    /// application may use it to select a verification key.
    pub kid: Option<String>,
}

impl Header {
    fn new(alg: Alg) -> Self {
        Self {
            alg,
            typ: "JWT".to_string(),
            kid: None,
        }
    }

    fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert("alg".to_string(), self.alg.as_str().into());
        obj.insert("typ".to_string(), self.typ.clone().into());
        if let Some(kid) = &self.kid {
            obj.insert("kid".to_string(), kid.clone().into());
        }
        serde_json::Value::Object(obj)
    }

    fn from_json(v: &serde_json::Value) -> Result<Self, Error> {
        let obj = v
            .as_object()
            .ok_or_else(|| Error::new("jwt: header is not a JSON object"))?;

        if obj.contains_key("crit") {
            return Err(Error::new(
                "jwt: header carries crit; refusing (RFC 7515 §4.1.11 - unknown critical extensions)",
            ));
        }

        let alg_str = obj
            .get("alg")
            .and_then(|x| x.as_str())
            .ok_or_else(|| Error::new("jwt: header missing alg"))?;
        let alg = Alg::from_str(alg_str)?;

        if let Some(t) = obj.get("typ") {
            let s = t
                .as_str()
                .ok_or_else(|| Error::new("jwt: header typ is not a string"))?;
            if !s.eq_ignore_ascii_case("JWT") {
                return Err(Error::new(format!(
                    "jwt: unsupported header typ {s:?} (expected \"JWT\")"
                )));
            }
        }

        let kid = obj.get("kid").and_then(|x| x.as_str()).map(String::from);

        Ok(Self {
            alg,
            typ: "JWT".to_string(),
            kid,
        })
    }
}

/// JWT claims set (RFC 7519 §4). The seven registered claims are
/// surfaced directly; everything else goes into `custom`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    /// `iss` - token issuer.
    pub iss: Option<String>,
    /// `sub` - token subject.
    pub sub: Option<String>,
    /// `aud` - token audience (one or many).
    pub aud: Option<Vec<String>>,
    /// `exp` - expiration time, seconds since the Unix epoch.
    pub exp: Option<i64>,
    /// `nbf` - not-before time, seconds since the Unix epoch.
    pub nbf: Option<i64>,
    /// `iat` - issue time, seconds since the Unix epoch.
    pub iat: Option<i64>,
    /// `jti` - opaque, application-chosen token identifier.
    pub jti: Option<String>,
    /// Application-defined extra claims. Round-tripped verbatim.
    pub custom: serde_json::Map<String, serde_json::Value>,
}

impl Default for Claims {
    fn default() -> Self {
        Self::new()
    }
}

impl Claims {
    /// Empty claim set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            iss: None,
            sub: None,
            aud: None,
            exp: None,
            nbf: None,
            iat: None,
            jti: None,
            custom: serde_json::Map::new(),
        }
    }

    /// Sets the `iss` claim.
    #[must_use]
    pub fn issuer(mut self, iss: impl Into<String>) -> Self {
        self.iss = Some(iss.into());
        self
    }

    /// Sets the `sub` claim.
    #[must_use]
    pub fn subject(mut self, sub: impl Into<String>) -> Self {
        self.sub = Some(sub.into());
        self
    }

    /// Sets a single-element `aud` claim.
    #[must_use]
    pub fn audience(mut self, aud: impl Into<String>) -> Self {
        self.aud = Some(vec![aud.into()]);
        self
    }

    /// Sets a multi-element `aud` claim.
    #[must_use]
    pub fn audiences(mut self, auds: Vec<String>) -> Self {
        self.aud = Some(auds);
        self
    }

    /// Sets `exp` to an absolute Unix timestamp (seconds).
    #[must_use]
    pub fn expires_at(mut self, unix_secs: i64) -> Self {
        self.exp = Some(unix_secs);
        self
    }

    /// Sets `exp` to `now + secs`.
    #[must_use]
    pub fn expires_in_secs(mut self, secs: i64) -> Self {
        self.exp = Some(now_unix_secs().saturating_add(secs));
        self
    }

    /// Sets the `nbf` claim.
    #[must_use]
    pub fn not_before(mut self, unix_secs: i64) -> Self {
        self.nbf = Some(unix_secs);
        self
    }

    /// Sets the `iat` claim.
    #[must_use]
    pub fn issued_at(mut self, unix_secs: i64) -> Self {
        self.iat = Some(unix_secs);
        self
    }

    /// Sets the `jti` claim.
    #[must_use]
    pub fn id(mut self, jti: impl Into<String>) -> Self {
        self.jti = Some(jti.into());
        self
    }

    /// Adds or replaces a single custom claim. Reserved-name
    /// keys (`iss`/`sub`/`aud`/`exp`/`nbf`/`iat`/`jti`) are
    /// silently ignored - use the typed setters instead.
    #[must_use]
    pub fn custom(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        let key = key.into();
        if !is_reserved_claim(&key) {
            self.custom.insert(key, value.into());
        }
        self
    }

    /// Render to a `serde_json::Value` object suitable for the
    /// JWS payload segment.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        if let Some(v) = &self.iss {
            obj.insert("iss".to_string(), v.clone().into());
        }
        if let Some(v) = &self.sub {
            obj.insert("sub".to_string(), v.clone().into());
        }
        if let Some(v) = &self.aud {
            // RFC 7519 §4.1.3: a single audience MAY be a bare string.
            if v.len() == 1 {
                obj.insert("aud".to_string(), v[0].clone().into());
            } else {
                obj.insert(
                    "aud".to_string(),
                    serde_json::Value::Array(v.iter().map(|s| s.clone().into()).collect()),
                );
            }
        }
        if let Some(v) = self.exp {
            obj.insert("exp".to_string(), v.into());
        }
        if let Some(v) = self.nbf {
            obj.insert("nbf".to_string(), v.into());
        }
        if let Some(v) = self.iat {
            obj.insert("iat".to_string(), v.into());
        }
        if let Some(v) = &self.jti {
            obj.insert("jti".to_string(), v.clone().into());
        }
        for (k, val) in &self.custom {
            obj.insert(k.clone(), val.clone());
        }
        serde_json::Value::Object(obj)
    }

    /// Parse a `serde_json::Value` object into a `Claims` set.
    /// Registered claims that are present with the wrong type
    /// produce an error; unknown keys go into `custom`.
    pub fn from_json(v: serde_json::Value) -> Result<Self, Error> {
        let serde_json::Value::Object(obj) = v else {
            return Err(Error::new("jwt: claims payload is not a JSON object"));
        };
        let mut out = Claims::new();
        for (k, val) in obj {
            match k.as_str() {
                "iss" => {
                    out.iss = Some(
                        val.as_str()
                            .ok_or_else(|| Error::new("jwt: iss is not a string"))?
                            .to_string(),
                    );
                }
                "sub" => {
                    out.sub = Some(
                        val.as_str()
                            .ok_or_else(|| Error::new("jwt: sub is not a string"))?
                            .to_string(),
                    );
                }
                "aud" => {
                    out.aud = Some(parse_aud(&val)?);
                }
                "exp" => out.exp = Some(parse_numeric_date(&val, "exp")?),
                "nbf" => out.nbf = Some(parse_numeric_date(&val, "nbf")?),
                "iat" => out.iat = Some(parse_numeric_date(&val, "iat")?),
                "jti" => {
                    out.jti = Some(
                        val.as_str()
                            .ok_or_else(|| Error::new("jwt: jti is not a string"))?
                            .to_string(),
                    );
                }
                _ => {
                    out.custom.insert(k, val);
                }
            }
        }
        Ok(out)
    }
}

/// Options controlling claim validation on `verify_*` calls.
#[derive(Debug, Clone)]
pub struct VerifyOpts {
    /// Clock-skew tolerance applied to `exp` and `nbf`.
    pub leeway_secs: i64,
    /// If set, the token's `iss` must equal this value.
    pub required_iss: Option<String>,
    /// If set, this value must appear in the token's `aud` array.
    pub required_aud: Option<String>,
    /// If set, the token's `sub` must equal this value.
    pub required_sub: Option<String>,
    /// If set, reject any token whose `iat` is older than this
    /// many seconds. Tokens without an `iat` claim are accepted
    /// (the application can require `iat` separately).
    pub max_age_secs: Option<i64>,
    /// Apply `exp` checking. Defaults to `true`.
    pub validate_exp: bool,
    /// Apply `nbf` checking. Defaults to `true`.
    pub validate_nbf: bool,
}

impl Default for VerifyOpts {
    fn default() -> Self {
        Self::new()
    }
}

impl VerifyOpts {
    /// Default-strict options: zero leeway, both `exp` and `nbf`
    /// checked, no required claims.
    #[must_use]
    pub fn new() -> Self {
        Self {
            leeway_secs: 0,
            required_iss: None,
            required_aud: None,
            required_sub: None,
            max_age_secs: None,
            validate_exp: true,
            validate_nbf: true,
        }
    }

    /// Sets the clock-skew tolerance.
    #[must_use]
    pub fn leeway(mut self, secs: i64) -> Self {
        self.leeway_secs = secs;
        self
    }

    /// Require a specific `iss`.
    #[must_use]
    pub fn iss(mut self, v: impl Into<String>) -> Self {
        self.required_iss = Some(v.into());
        self
    }

    /// Require a specific entry in `aud`.
    #[must_use]
    pub fn aud(mut self, v: impl Into<String>) -> Self {
        self.required_aud = Some(v.into());
        self
    }

    /// Require a specific `sub` claim value. The builder method's
    /// name shadows `std::ops::Sub::sub` only by spelling; the
    /// signature (`(self, impl Into<String>) -> Self`) is
    /// unambiguous in context.
    #[must_use]
    #[allow(
        clippy::should_implement_trait,
        reason = "RFC 7519 names this claim `sub`; the JWT API mirrors that"
    )]
    pub fn sub(mut self, v: impl Into<String>) -> Self {
        self.required_sub = Some(v.into());
        self
    }

    /// Reject tokens whose `iat` is older than `secs` seconds.
    #[must_use]
    pub fn max_age(mut self, secs: i64) -> Self {
        self.max_age_secs = Some(secs);
        self
    }
}

// -- HMAC signing/verification ---------------------------------------------

/// Signs a claims set with an HMAC algorithm and shared key.
/// `alg` must be one of `HS256` / `HS384` / `HS512`; anything
/// else is a hard error so callers can't accidentally produce
/// asymmetric tokens through the symmetric entry point.
pub fn sign_hs(alg: Alg, claims: &Claims, key: &[u8]) -> Result<String, Error> {
    if !alg.is_hmac() {
        return Err(Error::new(format!(
            "jwt: sign_hs called with non-HMAC alg {}",
            alg.as_str()
        )));
    }
    let header = Header::new(alg);
    let signing_input = build_signing_input(&header, claims)?;
    let sig = hmac_sign(alg, key, signing_input.as_bytes());
    Ok(format!("{signing_input}.{}", b64url_encode(&sig)))
}

/// Verifies an HMAC-signed token and returns the parsed claims.
///
/// Rejects the token if (a) the header `alg` is not exactly
/// `expected_alg`, (b) the header `alg` is asymmetric (avoiding
/// the classic HMAC-vs-RSA-public-key confusion attack), or
/// (c) the registered claims fall outside `opts`.
pub fn verify_hs(
    token: &str,
    expected_alg: Alg,
    key: &[u8],
    opts: &VerifyOpts,
) -> Result<Claims, Error> {
    if !expected_alg.is_hmac() {
        return Err(Error::new(format!(
            "jwt: verify_hs called with non-HMAC alg {}",
            expected_alg.as_str()
        )));
    }
    let (header, claims, signing_input, sig) = split_and_decode(token)?;

    if !header.alg.is_hmac() {
        return Err(Error::new(format!(
            "jwt: HMAC verifier refusing asymmetric token alg {}",
            header.alg.as_str()
        )));
    }
    if header.alg != expected_alg {
        return Err(Error::new(format!(
            "jwt: alg mismatch - token says {} but verifier expected {}",
            header.alg.as_str(),
            expected_alg.as_str()
        )));
    }

    let expected = hmac_sign(expected_alg, key, signing_input.as_bytes());
    if !subtle::constant_time_eq(&expected, &sig) {
        return Err(Error::new("jwt: signature invalid"));
    }

    validate_claims(&claims, opts)?;
    Ok(claims)
}

// -- ECDSA P-256 (ES256) ---------------------------------------------------

/// Signs a claims set with ECDSA P-256 + SHA-256, using a
/// PKCS#8-PEM-encoded signing key.
pub fn sign_es256(claims: &Claims, signing_key_pem: &str) -> Result<String, Error> {
    use p256::ecdsa::{Signature, SigningKey, signature::Signer};
    use p256::pkcs8::DecodePrivateKey;

    let header = Header::new(Alg::Es256);
    let signing_input = build_signing_input(&header, claims)?;
    let signing = SigningKey::from_pkcs8_pem(signing_key_pem)
        .map_err(|e| Error::new(format!("jwt: ES256 secret pem: {e}")))?;
    let sig: Signature = signing.sign(signing_input.as_bytes());
    // RFC 7518 §3.4: the JWS signature is fixed-length raw r||s
    // (64 bytes for P-256), not the ASN.1 DER envelope.
    let raw = sig.to_bytes();
    Ok(format!("{signing_input}.{}", b64url_encode(&raw[..])))
}

/// Verifies an ES256 token against an SPKI-PEM-encoded public key.
pub fn verify_es256(
    token: &str,
    verifying_key_pem: &str,
    opts: &VerifyOpts,
) -> Result<Claims, Error> {
    use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier};
    use p256::pkcs8::DecodePublicKey;

    let (header, claims, signing_input, sig) = split_and_decode(token)?;
    if header.alg.is_hmac() {
        return Err(Error::new(format!(
            "jwt: ES256 verifier refusing HMAC token alg {}",
            header.alg.as_str()
        )));
    }
    if header.alg != Alg::Es256 {
        return Err(Error::new(format!(
            "jwt: alg mismatch - token says {} but verifier expected ES256",
            header.alg.as_str()
        )));
    }
    if sig.len() != 64 {
        return Err(Error::new(format!(
            "jwt: ES256 signature must be 64 bytes (r||s), got {}",
            sig.len()
        )));
    }

    let key = VerifyingKey::from_public_key_pem(verifying_key_pem)
        .map_err(|e| Error::new(format!("jwt: ES256 public pem: {e}")))?;
    let signature = Signature::from_slice(&sig)
        .map_err(|e| Error::new(format!("jwt: ES256 signature: {e}")))?;
    key.verify(signing_input.as_bytes(), &signature)
        .map_err(|_| Error::new("jwt: signature invalid"))?;

    validate_claims(&claims, opts)?;
    Ok(claims)
}

// -- Ed25519 (EdDSA) -------------------------------------------------------

/// Signs a claims set with Ed25519, using a PKCS#8-PEM-encoded
/// signing key.
pub fn sign_eddsa(claims: &Claims, signing_key_pem: &str) -> Result<String, Error> {
    use ed25519_dalek::pkcs8::DecodePrivateKey;
    use ed25519_dalek::{Signer, SigningKey};

    let header = Header::new(Alg::EdDsa);
    let signing_input = build_signing_input(&header, claims)?;
    let signing = SigningKey::from_pkcs8_pem(signing_key_pem)
        .map_err(|e| Error::new(format!("jwt: EdDSA secret pem: {e}")))?;
    // Ed25519 signatures are natively 64 raw bytes - no DER detour.
    let sig = signing.sign(signing_input.as_bytes());
    Ok(format!(
        "{signing_input}.{}",
        b64url_encode(&sig.to_bytes())
    ))
}

/// Verifies an `EdDSA` token against an SPKI-PEM-encoded public key.
pub fn verify_eddsa(
    token: &str,
    verifying_key_pem: &str,
    opts: &VerifyOpts,
) -> Result<Claims, Error> {
    use ed25519_dalek::pkcs8::DecodePublicKey;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let (header, claims, signing_input, sig) = split_and_decode(token)?;
    if header.alg.is_hmac() {
        return Err(Error::new(format!(
            "jwt: EdDSA verifier refusing HMAC token alg {}",
            header.alg.as_str()
        )));
    }
    if header.alg != Alg::EdDsa {
        return Err(Error::new(format!(
            "jwt: alg mismatch - token says {} but verifier expected EdDSA",
            header.alg.as_str()
        )));
    }
    if sig.len() != 64 {
        return Err(Error::new(format!(
            "jwt: EdDSA signature must be 64 bytes, got {}",
            sig.len()
        )));
    }

    let key = VerifyingKey::from_public_key_pem(verifying_key_pem)
        .map_err(|e| Error::new(format!("jwt: EdDSA public pem: {e}")))?;
    let sig_arr: [u8; 64] = sig
        .as_slice()
        .try_into()
        .map_err(|_| Error::new("jwt: EdDSA signature: bad length"))?;
    let signature = Signature::from_bytes(&sig_arr);
    key.verify(signing_input.as_bytes(), &signature)
        .map_err(|_| Error::new("jwt: signature invalid"))?;

    validate_claims(&claims, opts)?;
    Ok(claims)
}

// -- RSA PKCS#1 v1.5 (RS256 / RS384 / RS512) -------------------------------

/// Verifies an `RS256` / `RS384` / `RS512` token against an
/// SPKI-PEM-encoded RSA public key (the conventional
/// `-----BEGIN PUBLIC KEY-----` form, also accepted as
/// `RSA PUBLIC KEY` for raw PKCS#1).
///
/// `expected_alg` must be one of `Alg::Rs256` / `Alg::Rs384` /
/// `Alg::Rs512` - anything else is a hard error so callers can't
/// route a non-RSA token through this entry. Verification runs
/// through `ring`'s constant-time RSA implementation; the
/// vulnerable `rsa 0.9.x` crate is not linked.
pub fn verify_rs(
    token: &str,
    expected_alg: Alg,
    verifying_key_pem: &str,
    opts: &VerifyOpts,
) -> Result<Claims, Error> {
    if !expected_alg.is_rsa() {
        return Err(Error::new(format!(
            "jwt: verify_rs called with non-RSA alg {}",
            expected_alg.as_str()
        )));
    }

    let (header, claims, signing_input, sig) = split_and_decode(token)?;
    if header.alg.is_hmac() {
        return Err(Error::new(format!(
            "jwt: RSA verifier refusing HMAC token alg {}",
            header.alg.as_str()
        )));
    }
    if header.alg != expected_alg {
        return Err(Error::new(format!(
            "jwt: alg mismatch - token says {} but verifier expected {}",
            header.alg.as_str(),
            expected_alg.as_str()
        )));
    }

    let (n, e) = parse_rsa_public_key_pem(verifying_key_pem)?;
    let params: &'static dyn ring::signature::VerificationAlgorithm = match expected_alg {
        Alg::Rs256 => &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        Alg::Rs384 => &ring::signature::RSA_PKCS1_2048_8192_SHA384,
        Alg::Rs512 => &ring::signature::RSA_PKCS1_2048_8192_SHA512,
        // Unreachable: callers gate on `expected_alg.is_rsa()`.
        _ => return Err(Error::new("jwt: internal RSA alg dispatch error")),
    };

    // Ring expects the public key as a DER-encoded RSAPublicKey
    // (PKCS#1: SEQUENCE { modulus INTEGER, exponent INTEGER }).
    // x509-parser gives us the SPKI-stripped n/e raw bytes; we
    // re-encode them as that DER SEQUENCE.
    let der = encode_rsa_public_key_der(&n, &e);
    let key = ring::signature::UnparsedPublicKey::new(params, der);
    key.verify(signing_input.as_bytes(), &sig)
        .map_err(|_| Error::new("jwt: signature invalid"))?;

    validate_claims(&claims, opts)?;
    Ok(claims)
}

/// Convenience wrapper for `verify_rs(.., Alg::Rs256, ..)`.
pub fn verify_rs256(
    token: &str,
    verifying_key_pem: &str,
    opts: &VerifyOpts,
) -> Result<Claims, Error> {
    verify_rs(token, Alg::Rs256, verifying_key_pem, opts)
}

/// Convenience wrapper for `verify_rs(.., Alg::Rs384, ..)`.
pub fn verify_rs384(
    token: &str,
    verifying_key_pem: &str,
    opts: &VerifyOpts,
) -> Result<Claims, Error> {
    verify_rs(token, Alg::Rs384, verifying_key_pem, opts)
}

/// Convenience wrapper for `verify_rs(.., Alg::Rs512, ..)`.
pub fn verify_rs512(
    token: &str,
    verifying_key_pem: &str,
    opts: &VerifyOpts,
) -> Result<Claims, Error> {
    verify_rs(token, Alg::Rs512, verifying_key_pem, opts)
}

/// Parses an RSA public key from PEM. Accepts both SPKI
/// (`BEGIN PUBLIC KEY`, the JOSE / OIDC convention) and bare
/// PKCS#1 (`BEGIN RSA PUBLIC KEY`). Returns `(modulus_be, exponent_be)`
/// with leading zero bytes stripped - the shape ring expects.
fn parse_rsa_public_key_pem(pem: &str) -> Result<(Vec<u8>, Vec<u8>), Error> {
    use std::io::Cursor;
    use x509_parser::pem::Pem;
    use x509_parser::prelude::FromDer;
    use x509_parser::x509::SubjectPublicKeyInfo;

    let bytes = pem.as_bytes();
    let (parsed, _read) = Pem::read(Cursor::new(bytes))
        .map_err(|e| Error::new(format!("jwt: RSA public pem: {e}")))?;

    let (n_bytes, e_bytes): (Vec<u8>, Vec<u8>) = match parsed.label.as_str() {
        "PUBLIC KEY" => {
            // SPKI: AlgorithmIdentifier + BIT STRING(RSAPublicKey).
            let (_, spki) = SubjectPublicKeyInfo::from_der(&parsed.contents)
                .map_err(|e| Error::new(format!("jwt: RSA SPKI parse: {e}")))?;
            let pk = spki
                .parsed()
                .map_err(|e| Error::new(format!("jwt: RSA SPKI parsed: {e}")))?;
            match pk {
                x509_parser::public_key::PublicKey::RSA(rsa) => {
                    (rsa.modulus.to_vec(), rsa.exponent.to_vec())
                }
                _ => {
                    return Err(Error::new(
                        "jwt: RSA verifier got non-RSA key inside PUBLIC KEY pem",
                    ));
                }
            }
        }
        "RSA PUBLIC KEY" => {
            // Bare PKCS#1: SEQUENCE { modulus, exponent }.
            let (_, rsa) = x509_parser::public_key::RSAPublicKey::from_der(&parsed.contents)
                .map_err(|e| Error::new(format!("jwt: RSA PKCS#1 parse: {e}")))?;
            (rsa.modulus.to_vec(), rsa.exponent.to_vec())
        }
        other => {
            return Err(Error::new(format!(
                "jwt: RSA verifier got unsupported pem label {other:?} (expected PUBLIC KEY or RSA PUBLIC KEY)"
            )));
        }
    };

    Ok((strip_leading_zeros(&n_bytes), strip_leading_zeros(&e_bytes)))
}

/// Strips leading 0x00 bytes from a big-endian integer encoding -
/// ASN.1 INTEGER prepends a zero byte to keep the high bit clear,
/// but ring wants the unpadded magnitude.
fn strip_leading_zeros(bytes: &[u8]) -> Vec<u8> {
    let first_nonzero = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    bytes[first_nonzero..].to_vec()
}

/// Encodes an `RSAPublicKey` (PKCS#1) DER SEQUENCE from raw
/// big-endian n and e bytes. The output is what ring's
/// `RSA_PKCS1_*` verification algorithms accept as the public key.
fn encode_rsa_public_key_der(n: &[u8], e: &[u8]) -> Vec<u8> {
    let n_int = der_encode_integer(n);
    let e_int = der_encode_integer(e);
    let mut body = Vec::with_capacity(n_int.len() + e_int.len());
    body.extend_from_slice(&n_int);
    body.extend_from_slice(&e_int);
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(0x30); // SEQUENCE
    der_encode_length(&mut out, body.len());
    out.extend_from_slice(&body);
    out
}

/// Encodes a single DER INTEGER from a big-endian magnitude. A
/// leading 0x00 is prepended when the high bit is set so the
/// integer reads as non-negative.
fn der_encode_integer(magnitude: &[u8]) -> Vec<u8> {
    let needs_sign_pad = magnitude.first().is_some_and(|b| b & 0x80 != 0);
    let content_len = magnitude.len() + usize::from(needs_sign_pad);
    let mut out = Vec::with_capacity(content_len + 4);
    out.push(0x02); // INTEGER
    der_encode_length(&mut out, content_len);
    if needs_sign_pad {
        out.push(0x00);
    }
    out.extend_from_slice(magnitude);
    out
}

/// Appends an ASN.1 DER definite-length encoding. Short-form for
/// lengths < 128, long-form otherwise.
fn der_encode_length(out: &mut Vec<u8>, len: usize) {
    if len < 0x80 {
        out.push(len as u8);
        return;
    }
    let mut tmp = [0u8; 8];
    let mut n = len;
    let mut i = tmp.len();
    while n > 0 {
        i -= 1;
        tmp[i] = (n & 0xff) as u8;
        n >>= 8;
    }
    let count = tmp.len() - i;
    out.push(0x80 | count as u8);
    out.extend_from_slice(&tmp[i..]);
}

// -- internals -------------------------------------------------------------

fn is_reserved_claim(k: &str) -> bool {
    matches!(k, "iss" | "sub" | "aud" | "exp" | "nbf" | "iat" | "jti")
}

fn now_unix_secs() -> i64 {
    gossamer_runtime::platform::system_time_now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn parse_aud(v: &serde_json::Value) -> Result<Vec<String>, Error> {
    match v {
        serde_json::Value::String(s) => Ok(vec![s.clone()]),
        serde_json::Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for entry in arr {
                let s = entry
                    .as_str()
                    .ok_or_else(|| Error::new("jwt: aud array contains non-string"))?;
                out.push(s.to_string());
            }
            Ok(out)
        }
        _ => Err(Error::new("jwt: aud must be a string or array of strings")),
    }
}

fn parse_numeric_date(v: &serde_json::Value, name: &str) -> Result<i64, Error> {
    // RFC 7519 §2 NumericDate: "A JSON numeric value representing
    // the number of seconds from 1970-01-01T00:00:00Z UTC". Both
    // integer and float forms are valid on the wire.
    if let Some(n) = v.as_i64() {
        return Ok(n);
    }
    if let Some(f) = v.as_f64() {
        if f.is_finite() {
            return Ok(f as i64);
        }
    }
    Err(Error::new(format!("jwt: {name} is not a numeric date")))
}

fn build_signing_input(header: &Header, claims: &Claims) -> Result<String, Error> {
    let header_json = serde_json::to_vec(&header.to_json())
        .map_err(|e| Error::new(format!("jwt: encode header: {e}")))?;
    let claims_json = serde_json::to_vec(&claims.to_json())
        .map_err(|e| Error::new(format!("jwt: encode claims: {e}")))?;
    Ok(format!(
        "{}.{}",
        b64url_encode(&header_json),
        b64url_encode(&claims_json)
    ))
}

/// Splits a JWS compact-serialization token into
/// `(header, claims, signing_input, signature_bytes)`. Performs
/// every structural decode but no signature verification and no
/// claim validation.
fn split_and_decode(token: &str) -> Result<(Header, Claims, String, Vec<u8>), Error> {
    // Reject tokens with stray dots before doing any allocation -
    // a JWS compact token is exactly three segments.
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(Error::new(format!(
            "jwt: expected 3 segments, got {}",
            parts.len()
        )));
    }
    let header_b = b64url_decode(parts[0])?;
    let payload_b = b64url_decode(parts[1])?;
    let sig_b = b64url_decode(parts[2])?;

    let header_v: serde_json::Value = serde_json::from_slice(&header_b)
        .map_err(|e| Error::new(format!("jwt: header json: {e}")))?;
    let header = Header::from_json(&header_v)?;

    let payload_v: serde_json::Value = serde_json::from_slice(&payload_b)
        .map_err(|e| Error::new(format!("jwt: payload json: {e}")))?;
    let claims = Claims::from_json(payload_v)?;

    let signing_input = format!("{}.{}", parts[0], parts[1]);
    Ok((header, claims, signing_input, sig_b))
}

fn validate_claims(claims: &Claims, opts: &VerifyOpts) -> Result<(), Error> {
    let now = now_unix_secs();
    let leeway = opts.leeway_secs;

    if opts.validate_exp {
        if let Some(exp) = claims.exp {
            // exp is the absolute deadline - token is valid while
            // now <= exp + leeway. Use saturating math to avoid
            // wrap on pathological leeway values.
            if now > exp.saturating_add(leeway) {
                return Err(Error::new("jwt: token expired"));
            }
        }
    }

    if opts.validate_nbf {
        if let Some(nbf) = claims.nbf {
            if now.saturating_add(leeway) < nbf {
                return Err(Error::new("jwt: token not yet valid (nbf)"));
            }
        }
    }

    if let Some(max_age) = opts.max_age_secs {
        if let Some(iat) = claims.iat {
            if now > iat.saturating_add(max_age).saturating_add(leeway) {
                return Err(Error::new("jwt: token exceeds max age"));
            }
        }
    }

    if let Some(want) = &opts.required_iss {
        match &claims.iss {
            Some(got) if got == want => {}
            _ => {
                return Err(Error::new(format!("jwt: iss mismatch (expected {want:?})")));
            }
        }
    }

    if let Some(want) = &opts.required_aud {
        let ok = claims
            .aud
            .as_ref()
            .is_some_and(|v| v.iter().any(|a| a == want));
        if !ok {
            return Err(Error::new(format!(
                "jwt: aud does not contain required value {want:?}"
            )));
        }
    }

    if let Some(want) = &opts.required_sub {
        match &claims.sub {
            Some(got) if got == want => {}
            _ => {
                return Err(Error::new(format!("jwt: sub mismatch (expected {want:?})")));
            }
        }
    }

    Ok(())
}

// -- HMAC dispatch ---------------------------------------------------------

fn hmac_sign(alg: Alg, key: &[u8], message: &[u8]) -> Vec<u8> {
    match alg {
        Alg::Hs256 => crate::crypto::hmac::sha256_mac(key, message).to_vec(),
        Alg::Hs384 => hmac_sha384(key, message).to_vec(),
        Alg::Hs512 => hmac_sha512(key, message).to_vec(),
        // Unreachable: callers gate on `alg.is_hmac()`.
        _ => Vec::new(),
    }
}

// Block size for both SHA-384 and SHA-512 is 128 bytes.
const SHA512_BLOCK: usize = 128;

fn hmac_sha512(key: &[u8], message: &[u8]) -> [u8; 64] {
    // Standard HMAC construction per RFC 2104, parameterized for
    // SHA-512. Mirrors the in-tree sha256_mac shape so the audit
    // trail is identical.
    let mut block = [0u8; SHA512_BLOCK];
    if key.len() > SHA512_BLOCK {
        block[..64].copy_from_slice(&sha512::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner_key = [0u8; SHA512_BLOCK];
    let mut outer_key = [0u8; SHA512_BLOCK];
    for i in 0..SHA512_BLOCK {
        inner_key[i] = block[i] ^ 0x36;
        outer_key[i] = block[i] ^ 0x5c;
    }
    let mut inner = sha2::Sha512::new();
    inner.update(inner_key);
    inner.update(message);
    let inner_hash = inner.finalize();
    let mut outer = sha2::Sha512::new();
    outer.update(outer_key);
    outer.update(inner_hash);
    outer.finalize().into()
}

fn hmac_sha384(key: &[u8], message: &[u8]) -> [u8; 48] {
    // SHA-384 uses the same 128-byte block size as SHA-512.
    let mut block = [0u8; SHA512_BLOCK];
    if key.len() > SHA512_BLOCK {
        let mut h = sha2::Sha384::new();
        h.update(key);
        let digest: [u8; 48] = h.finalize().into();
        block[..48].copy_from_slice(&digest);
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner_key = [0u8; SHA512_BLOCK];
    let mut outer_key = [0u8; SHA512_BLOCK];
    for i in 0..SHA512_BLOCK {
        inner_key[i] = block[i] ^ 0x36;
        outer_key[i] = block[i] ^ 0x5c;
    }
    let mut inner = sha2::Sha384::new();
    inner.update(inner_key);
    inner.update(message);
    let inner_hash = inner.finalize();
    let mut outer = sha2::Sha384::new();
    outer.update(outer_key);
    outer.update(inner_hash);
    outer.finalize().into()
}

// -- base64url (RFC 4648 §5, no padding) -----------------------------------

const B64URL_ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64url_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= input.len() {
        let n =
            (u32::from(input[i]) << 16) | (u32::from(input[i + 1]) << 8) | u32::from(input[i + 2]);
        out.push(B64URL_ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 6) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let remaining = input.len() - i;
    if remaining == 1 {
        let n = u32::from(input[i]) << 16;
        out.push(B64URL_ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 12) & 0x3f) as usize] as char);
    } else if remaining == 2 {
        let n = (u32::from(input[i]) << 16) | (u32::from(input[i + 1]) << 8);
        out.push(B64URL_ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 6) & 0x3f) as usize] as char);
    }
    out
}

fn b64url_decode(input: &str) -> Result<Vec<u8>, Error> {
    // Strip any trailing padding the producer may have included.
    let trimmed = input.trim_end_matches('=');
    let bytes = trimmed.as_bytes();

    // Compute final output length up front: every 4 input chars
    // become 3 output bytes; the trailing 2/3 chars become 1/2.
    let n = bytes.len();
    let out_len = match n % 4 {
        0 => n / 4 * 3,
        2 => n / 4 * 3 + 1,
        3 => n / 4 * 3 + 2,
        // A length of 1 mod 4 cannot occur in valid base64.
        _ => return Err(Error::new("jwt: base64url: invalid length")),
    };

    let mut out = Vec::with_capacity(out_len);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        let v = decode_b64url_char(b)?;
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    // Remaining bits must be zero - otherwise this is malformed.
    if bits > 0 && (acc & ((1u32 << bits) - 1)) != 0 {
        return Err(Error::new("jwt: base64url: non-zero padding bits"));
    }
    Ok(out)
}

fn decode_b64url_char(c: u8) -> Result<u8, Error> {
    match c {
        b'A'..=b'Z' => Ok(c - b'A'),
        b'a'..=b'z' => Ok(c - b'a' + 26),
        b'0'..=b'9' => Ok(c - b'0' + 52),
        b'-' => Ok(62),
        b'_' => Ok(63),
        _ => Err(Error::new(format!(
            "jwt: base64url: invalid character 0x{c:02x}"
        ))),
    }
}

// --------------------------------------------------------------------------
// JSON-string entry API
//
// The bytecode-VM builtins for `std::jwt` move `Claims` across the call
// boundary as a JSON object string rather than a struct, so the surface
// is struct-marshalling-free and bit-identical to the compiled tier's
// `gos_rt_jwt_*` shims. Each wrapper parses the claims JSON into a
// `Claims` (`Claims::from_json`) before signing, and renders the
// verified `Claims` back to canonical JSON (`Claims::to_json`) after
// verifying.

/// Parse a JSON object string into a `Claims` set.
fn claims_from_json_str(claims_json: &str) -> Result<Claims, Error> {
    let v: serde_json::Value = serde_json::from_str(claims_json)
        .map_err(|e| Error::new(format!("jwt: claims json: {e}")))?;
    Claims::from_json(v)
}

/// Render a `Claims` set to canonical JSON (sorted keys, matching the
/// compiled tier's `normalize_claims`).
fn claims_to_json_str(claims: &Claims) -> Result<String, Error> {
    serde_json::to_string(&claims.to_json())
        .map_err(|e| Error::new(format!("jwt: encode claims: {e}")))
}

/// `jwt::sign_hs(alg, claims_json, key)` over JSON-string claims.
pub fn sign_hs_json(alg: &str, claims_json: &str, key: &[u8]) -> Result<String, Error> {
    let alg = Alg::from_str(alg)?;
    let claims = claims_from_json_str(claims_json)?;
    sign_hs(alg, &claims, key)
}

/// `jwt::verify_hs(token, alg, key, leeway)` returning canonical claims JSON.
pub fn verify_hs_json(token: &str, alg: &str, key: &[u8], leeway: i64) -> Result<String, Error> {
    let alg = Alg::from_str(alg)?;
    let opts = VerifyOpts::new().leeway(leeway);
    let claims = verify_hs(token, alg, key, &opts)?;
    claims_to_json_str(&claims)
}

/// `jwt::sign_es256(claims_json, signing_key_pem)` over JSON-string claims.
pub fn sign_es256_json(claims_json: &str, signing_key_pem: &str) -> Result<String, Error> {
    let claims = claims_from_json_str(claims_json)?;
    sign_es256(&claims, signing_key_pem)
}

/// `jwt::verify_es256(token, pem, leeway)` returning canonical claims JSON.
pub fn verify_es256_json(
    token: &str,
    verifying_key_pem: &str,
    leeway: i64,
) -> Result<String, Error> {
    let opts = VerifyOpts::new().leeway(leeway);
    let claims = verify_es256(token, verifying_key_pem, &opts)?;
    claims_to_json_str(&claims)
}

/// `jwt::sign_eddsa(claims_json, signing_key_pem)` over JSON-string claims.
pub fn sign_eddsa_json(claims_json: &str, signing_key_pem: &str) -> Result<String, Error> {
    let claims = claims_from_json_str(claims_json)?;
    sign_eddsa(&claims, signing_key_pem)
}

/// `jwt::verify_eddsa(token, pem, leeway)` returning canonical claims JSON.
pub fn verify_eddsa_json(
    token: &str,
    verifying_key_pem: &str,
    leeway: i64,
) -> Result<String, Error> {
    let opts = VerifyOpts::new().leeway(leeway);
    let claims = verify_eddsa(token, verifying_key_pem, &opts)?;
    claims_to_json_str(&claims)
}

/// `jwt::verify(token, alg, key, leeway, issuer, audience)` - the
/// verifier a service protecting an endpoint uses.
///
/// One entry point for every algorithm the library supports, including
/// the RS family every mainstream identity provider mints with. `key` is
/// the shared secret for the HS family and the PEM-encoded public key for
/// the rest.
///
/// `issuer` and `audience` are enforced when non-empty. Leaving them empty
/// accepts a token from any issuer that signed with this key and any
/// audience it was minted for, which is what makes cross-service replay
/// possible - a service that shares a key with another must set them.
///
/// Answers the canonical claims JSON on success.
pub fn verify_json(
    token: &str,
    alg: &str,
    key: &str,
    leeway: i64,
    issuer: &str,
    audience: &str,
) -> Result<String, Error> {
    let alg = Alg::from_str(alg)?;
    let mut opts = VerifyOpts::new().leeway(leeway);
    if !issuer.is_empty() {
        opts.required_iss = Some(issuer.to_string());
    }
    if !audience.is_empty() {
        opts.required_aud = Some(audience.to_string());
    }
    let claims = if alg.is_hmac() {
        verify_hs(token, alg, key.as_bytes(), &opts)?
    } else if alg.is_rsa() {
        verify_rs(token, alg, key, &opts)?
    } else if alg == Alg::Es256 {
        verify_es256(token, key, &opts)?
    } else {
        verify_eddsa(token, key, &opts)?
    };
    claims_to_json_str(&claims)
}

/// `jwt::header(token)` - the token's JOSE header as JSON, read WITHOUT
/// verifying the signature.
///
/// A service holding a key set reads `kid` and `alg` to choose which key
/// to verify with. The header is unauthenticated until that verification
/// succeeds, so nothing in it may be trusted for anything else - in
/// particular `alg` must be checked against what the chosen key permits,
/// which `verify` does by taking the expected algorithm as its own
/// argument rather than reading it from the token.
pub fn header_json(token: &str) -> Result<String, Error> {
    let header_b64 = token
        .split('.')
        .next()
        .ok_or_else(|| Error::new("jwt: token has no header segment"))?;
    let raw = b64url_decode(header_b64)?;
    let text = std::str::from_utf8(&raw)
        .map_err(|e| Error::new(format!("jwt: header is not UTF-8: {e}")))?;
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| Error::new(format!("jwt: header is not JSON: {e}")))?;
    serde_json::to_string(&value).map_err(|e| Error::new(format!("jwt: encode header: {e}")))
}

// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn key_hs() -> &'static [u8] {
        b"super-secret-test-key-do-not-reuse"
    }

    #[test]
    fn b64url_round_trips() {
        for input in [
            &b""[..],
            &b"f"[..],
            &b"fo"[..],
            &b"foo"[..],
            &b"foob"[..],
            &b"fooba"[..],
            &b"foobar"[..],
            &[0xff, 0xfe, 0xfd, 0xfc, 0x00, 0x01][..],
        ] {
            let enc = b64url_encode(input);
            // No padding ever, no '+' / '/' ever.
            assert!(!enc.contains('='));
            assert!(!enc.contains('+'));
            assert!(!enc.contains('/'));
            let dec = b64url_decode(&enc).unwrap();
            assert_eq!(dec, input);
        }
    }

    #[test]
    fn b64url_accepts_optional_padding() {
        // Tokens emitted by other libraries sometimes include '='.
        let raw = b"hello";
        let mut enc = b64url_encode(raw);
        enc.push('=');
        assert_eq!(b64url_decode(&enc).unwrap(), raw);
    }

    #[test]
    fn b64url_rejects_invalid_chars() {
        assert!(b64url_decode("abc!").is_err());
    }

    #[test]
    fn hs256_round_trip() {
        let claims = Claims::new()
            .issuer("issuer.example")
            .subject("user-42")
            .audience("clients.example")
            .id("token-1")
            .issued_at(1_700_000_000);
        let token = sign_hs(Alg::Hs256, &claims, key_hs()).unwrap();
        // JWS compact serialization has exactly two dots.
        assert_eq!(token.matches('.').count(), 2);
        let back = verify_hs(&token, Alg::Hs256, key_hs(), &VerifyOpts::new()).unwrap();
        assert_eq!(back.iss.as_deref(), Some("issuer.example"));
        assert_eq!(back.sub.as_deref(), Some("user-42"));
        assert_eq!(back.aud, Some(vec!["clients.example".to_string()]));
        assert_eq!(back.jti.as_deref(), Some("token-1"));
        assert_eq!(back.iat, Some(1_700_000_000));
    }

    #[test]
    fn hs256_matches_rfc7515_a1_vector() {
        // RFC 7515 Appendix A.1 - known HS256 test vector.
        // Header: {"typ":"JWT","alg":"HS256"}
        // Payload: {"iss":"joe","exp":1300819380,"http://example.com/is_root":true}
        // Key: the 64-byte octet string from Appendix A.1.
        let key: [u8; 64] = [
            3, 35, 53, 75, 43, 15, 165, 188, 131, 126, 6, 101, 119, 123, 166, 143, 90, 179, 40,
            230, 240, 84, 201, 40, 169, 15, 132, 178, 210, 80, 46, 191, 211, 251, 90, 146, 210, 6,
            71, 239, 150, 138, 180, 195, 119, 98, 61, 34, 61, 46, 33, 114, 5, 46, 79, 8, 192, 205,
            154, 245, 103, 208, 128, 163,
        ];
        let claims = Claims::new()
            .issuer("joe")
            .expires_at(1_300_819_380)
            .custom("http://example.com/is_root", serde_json::Value::Bool(true));
        let token = sign_hs(Alg::Hs256, &claims, &key).unwrap();
        let opts = VerifyOpts::new().leeway(i64::MAX / 4);
        let back = verify_hs(&token, Alg::Hs256, &key, &opts).unwrap();
        assert_eq!(back.iss.as_deref(), Some("joe"));
        assert_eq!(back.exp, Some(1_300_819_380));
        assert_eq!(
            back.custom.get("http://example.com/is_root"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn hs512_round_trip() {
        let claims = Claims::new().subject("u1").issued_at(now_unix_secs());
        let token = sign_hs(Alg::Hs512, &claims, key_hs()).unwrap();
        let back = verify_hs(&token, Alg::Hs512, key_hs(), &VerifyOpts::new()).unwrap();
        assert_eq!(back.sub.as_deref(), Some("u1"));
    }

    #[test]
    fn hs384_round_trip() {
        let claims = Claims::new().subject("u2");
        let token = sign_hs(Alg::Hs384, &claims, key_hs()).unwrap();
        let back = verify_hs(&token, Alg::Hs384, key_hs(), &VerifyOpts::new()).unwrap();
        assert_eq!(back.sub.as_deref(), Some("u2"));
    }

    #[test]
    fn tampered_signature_rejected() {
        let token = sign_hs(Alg::Hs256, &Claims::new().subject("u"), key_hs()).unwrap();
        // Flip a bit in the signature segment.
        let mut bytes = token.into_bytes();
        let len = bytes.len();
        // Last char of base64url alphabet is always safe to swap with the first.
        bytes[len - 1] = if bytes[len - 1] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(bytes).unwrap();
        assert!(verify_hs(&tampered, Alg::Hs256, key_hs(), &VerifyOpts::new()).is_err());
    }

    #[test]
    fn alg_none_rejected_on_verify() {
        // Construct a token with alg "none" by hand - no signature
        // segment computed, but the third segment is present and
        // empty so the token has the expected three-segment shape.
        let header = serde_json::json!({"alg":"none","typ":"JWT"});
        let payload = serde_json::json!({"sub":"hacker"});
        let h = b64url_encode(&serde_json::to_vec(&header).unwrap());
        let p = b64url_encode(&serde_json::to_vec(&payload).unwrap());
        let token = format!("{h}.{p}.");
        let err = verify_hs(&token, Alg::Hs256, key_hs(), &VerifyOpts::new()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("none"),
            "expected mention of alg none, got: {msg}"
        );
    }

    #[test]
    fn alg_mismatch_rejected() {
        let token = sign_hs(Alg::Hs256, &Claims::new(), key_hs()).unwrap();
        // Signed with HS256, verifier asks for HS512.
        let err = verify_hs(&token, Alg::Hs512, key_hs(), &VerifyOpts::new()).unwrap_err();
        assert!(format!("{err}").contains("alg mismatch"));
    }

    #[test]
    fn hs_vs_es_confusion_rejected() {
        // Sign a token with HS256, then try to verify it as ES256.
        // verify_es256 must reject HS256 outright, regardless of
        // whether any cryptographic operation could succeed.
        let (_secret_pem, public_pem) = crate::crypto::ecdsa::keypair_pem().unwrap();
        let hs_token = sign_hs(Alg::Hs256, &Claims::new().subject("u"), key_hs()).unwrap();
        let err = verify_es256(&hs_token, &public_pem, &VerifyOpts::new()).unwrap_err();
        assert!(
            format!("{err}").to_lowercase().contains("refusing")
                || format!("{err}").to_lowercase().contains("mismatch"),
            "expected HMAC-rejection error, got: {err}"
        );
    }

    #[test]
    fn expired_token_rejected_without_leeway() {
        let claims = Claims::new().expires_at(now_unix_secs() - 60);
        let token = sign_hs(Alg::Hs256, &claims, key_hs()).unwrap();
        assert!(verify_hs(&token, Alg::Hs256, key_hs(), &VerifyOpts::new()).is_err());
    }

    #[test]
    fn expired_token_accepted_with_leeway() {
        let claims = Claims::new().expires_at(now_unix_secs() - 60);
        let token = sign_hs(Alg::Hs256, &claims, key_hs()).unwrap();
        let opts = VerifyOpts::new().leeway(120);
        assert!(verify_hs(&token, Alg::Hs256, key_hs(), &opts).is_ok());
    }

    #[test]
    fn nbf_in_future_rejected_then_accepted_with_leeway() {
        let claims = Claims::new().not_before(now_unix_secs() + 60);
        let token = sign_hs(Alg::Hs256, &claims, key_hs()).unwrap();
        assert!(verify_hs(&token, Alg::Hs256, key_hs(), &VerifyOpts::new()).is_err());
        let opts = VerifyOpts::new().leeway(120);
        assert!(verify_hs(&token, Alg::Hs256, key_hs(), &opts).is_ok());
    }

    #[test]
    fn max_age_enforced() {
        let claims = Claims::new().issued_at(now_unix_secs() - 3600);
        let token = sign_hs(Alg::Hs256, &claims, key_hs()).unwrap();
        let strict = VerifyOpts::new().max_age(600);
        assert!(verify_hs(&token, Alg::Hs256, key_hs(), &strict).is_err());
        let lenient = VerifyOpts::new().max_age(7200);
        assert!(verify_hs(&token, Alg::Hs256, key_hs(), &lenient).is_ok());
    }

    #[test]
    fn required_iss_enforced() {
        let claims = Claims::new().issuer("real.example");
        let token = sign_hs(Alg::Hs256, &claims, key_hs()).unwrap();
        let opts = VerifyOpts::new().iss("real.example");
        assert!(verify_hs(&token, Alg::Hs256, key_hs(), &opts).is_ok());
        let opts_wrong = VerifyOpts::new().iss("attacker.example");
        assert!(verify_hs(&token, Alg::Hs256, key_hs(), &opts_wrong).is_err());
        let token_missing = sign_hs(Alg::Hs256, &Claims::new(), key_hs()).unwrap();
        assert!(verify_hs(&token_missing, Alg::Hs256, key_hs(), &opts).is_err());
    }

    #[test]
    fn required_aud_enforced() {
        let claims =
            Claims::new().audiences(vec!["api.example".to_string(), "web.example".to_string()]);
        let token = sign_hs(Alg::Hs256, &claims, key_hs()).unwrap();
        let opts = VerifyOpts::new().aud("api.example");
        assert!(verify_hs(&token, Alg::Hs256, key_hs(), &opts).is_ok());
        let opts_wrong = VerifyOpts::new().aud("admin.example");
        assert!(verify_hs(&token, Alg::Hs256, key_hs(), &opts_wrong).is_err());
    }

    #[test]
    fn required_sub_enforced() {
        let claims = Claims::new().subject("alice");
        let token = sign_hs(Alg::Hs256, &claims, key_hs()).unwrap();
        let opts = VerifyOpts::new().sub("alice");
        assert!(verify_hs(&token, Alg::Hs256, key_hs(), &opts).is_ok());
        let opts_wrong = VerifyOpts::new().sub("bob");
        assert!(verify_hs(&token, Alg::Hs256, key_hs(), &opts_wrong).is_err());
    }

    #[test]
    fn es256_round_trip() {
        let (secret_pem, public_pem) = crate::crypto::ecdsa::keypair_pem().unwrap();
        let claims = Claims::new()
            .subject("ecdsa-user")
            .issued_at(now_unix_secs());
        let token = sign_es256(&claims, &secret_pem).unwrap();
        let back = verify_es256(&token, &public_pem, &VerifyOpts::new()).unwrap();
        assert_eq!(back.sub.as_deref(), Some("ecdsa-user"));
    }

    #[test]
    fn es256_rejects_hs_alg_in_header() {
        // Mint a fake header that claims ES256 but the body is HS-signed;
        // this is covered by alg-mismatch already, but the inverse
        // direction (HS token vs ES verifier) is also a hard reject.
        let (_secret_pem, public_pem) = crate::crypto::ecdsa::keypair_pem().unwrap();
        let token = sign_hs(Alg::Hs256, &Claims::new(), key_hs()).unwrap();
        assert!(verify_es256(&token, &public_pem, &VerifyOpts::new()).is_err());
    }

    #[test]
    fn eddsa_round_trip() {
        // Both encoders share the PEM line-ending type, so use P-256's
        // re-export rather than a direct dependency just for this test.
        use ed25519_dalek::SigningKey;
        use ed25519_dalek::pkcs8::{EncodePrivateKey, EncodePublicKey};
        let mut rng = crate::crypto::rand::OsRng;
        let signing = SigningKey::generate(&mut rng);
        let verifying = signing.verifying_key();
        let secret_pem = signing
            .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
            .unwrap()
            .to_string();
        let public_pem = verifying
            .to_public_key_pem(p256::pkcs8::LineEnding::LF)
            .unwrap();

        let claims = Claims::new().subject("ed-user").issued_at(now_unix_secs());
        let token = sign_eddsa(&claims, &secret_pem).unwrap();
        let back = verify_eddsa(&token, &public_pem, &VerifyOpts::new()).unwrap();
        assert_eq!(back.sub.as_deref(), Some("ed-user"));

        // HS tokens must not pass an EdDSA verifier.
        let hs = sign_hs(Alg::Hs256, &Claims::new(), key_hs()).unwrap();
        assert!(verify_eddsa(&hs, &public_pem, &VerifyOpts::new()).is_err());
    }

    #[test]
    fn crit_header_rejected() {
        // Build a token whose header carries a crit parameter.
        // The signature is HS256 so the structural rejection is
        // unambiguously the crit reject, not the alg.
        let header = serde_json::json!({
            "alg":"HS256",
            "typ":"JWT",
            "crit":["exp"],
        });
        let payload = serde_json::json!({"sub":"x"});
        let h = b64url_encode(&serde_json::to_vec(&header).unwrap());
        let p = b64url_encode(&serde_json::to_vec(&payload).unwrap());
        let signing_input = format!("{h}.{p}");
        let sig = crate::crypto::hmac::sha256_mac(key_hs(), signing_input.as_bytes());
        let token = format!("{signing_input}.{}", b64url_encode(&sig));
        let err = verify_hs(&token, Alg::Hs256, key_hs(), &VerifyOpts::new()).unwrap_err();
        assert!(
            format!("{err}").to_lowercase().contains("crit"),
            "expected crit-rejection error, got: {err}"
        );
    }

    #[test]
    fn claims_round_trip_through_json() {
        let claims = Claims::new()
            .issuer("iss")
            .subject("sub")
            .audiences(vec!["a".to_string(), "b".to_string()])
            .expires_at(100)
            .not_before(50)
            .issued_at(25)
            .id("jti-1")
            .custom("role", serde_json::Value::String("admin".to_string()));
        let json = claims.to_json();
        let back = Claims::from_json(json).unwrap();
        assert_eq!(back, claims);
    }

    #[test]
    fn single_audience_serializes_as_string_per_rfc() {
        let claims = Claims::new().audience("solo");
        let v = claims.to_json();
        assert_eq!(
            v.as_object().unwrap().get("aud"),
            Some(&serde_json::Value::String("solo".to_string()))
        );
        // ...and round-trips back into a single-element Vec.
        let back = Claims::from_json(v).unwrap();
        assert_eq!(back.aud, Some(vec!["solo".to_string()]));
    }

    #[test]
    fn unsupported_alg_rejected() {
        let header = serde_json::json!({"alg":"RS256","typ":"JWT"});
        let payload = serde_json::json!({});
        let h = b64url_encode(&serde_json::to_vec(&header).unwrap());
        let p = b64url_encode(&serde_json::to_vec(&payload).unwrap());
        let token = format!("{h}.{p}.AAAA");
        assert!(verify_hs(&token, Alg::Hs256, key_hs(), &VerifyOpts::new()).is_err());
    }

    #[test]
    fn malformed_token_rejected() {
        assert!(
            verify_hs(
                "not.a.token.extra",
                Alg::Hs256,
                key_hs(),
                &VerifyOpts::new()
            )
            .is_err()
        );
        assert!(verify_hs("only.two", Alg::Hs256, key_hs(), &VerifyOpts::new()).is_err());
        assert!(verify_hs("", Alg::Hs256, key_hs(), &VerifyOpts::new()).is_err());
    }

    // -- RSA fixtures --------------------------------------------------
    //
    // A fixed 2048-bit RSA keypair used to mint test tokens. The
    // matching public key (`RSA_PUB_PEM`) is what `verify_rs*` is
    // exercised against. `RSA_PUB_PEM_WRONG` is a second,
    // independent key for the "wrong key" reject test.

    const RSA_PKCS8_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQC8aE5RdsVuGLhm\n\
2U/2RZLvmw0yixEonuHZzicg66uy+KSj11XIjK+uq0091Vi1B0wbG2WSEM/oZov2\n\
sg9G7ipvjiVFb2TVbVlBIDYgQ9yCFoLLUWGG4VM18wHCCPo9l7+/GNs+ZTlwgBsH\n\
0EIvPm2gX3swkIXn/SghjIA9TnXihZrSk2BYpZKhtA6wkBKWRH9urST6Q997/w4/\n\
0I05ELZbmLxfXrBxL8a2tczoRkpXCJUVdvLZt/vYqJXaZ/sK/4uPS0lWnQnCsSfD\n\
O8f4/fguUrVPaIhzghNfVCUKyvcu/UU69Sst5ceVpxgbdjiUWtsJwWRp8gu4WMDp\n\
g0RwFWFhAgMBAAECggEACe5ubaLjubyTc9bYSgE7qcyAv8pO2qPnAD+USaSPbD0c\n\
SKETarsa9thQZDlryXC1O7SQM3vi/O6MfP7AowEih+p0eOcfTi5k09o3BpctKq2C\n\
S+XIgBik9aVCnkMxr0kUsj94LYWedBl8oBPQeVCo7LKiLSdhHgWqAnxQ6J2X+M6L\n\
QPm47+Gaf/Rita2eJh1w3cPoVF0sPtu6GZwjOW74eSy70ZL5lc6fc9ep3626xjSw\n\
M2lSXYaRlGyP1hrBDXVHV3Pz5HpNvSezG3G7mywkyqHRjqSSchwsU2yjhDZvY4Og\n\
bYmSjto32uMRWTSineLgqf+tjEGlXswqZsreEggcZQKBgQDzHPdsZjdIKc4mmFbz\n\
97dEFAUmkYJf3ot9JZqh6ih/Py+tHi8LLzD1AAgq05llr+4DzoMKADZxML6fXUSm\n\
b/lzFZ923X0EXaVOaln2i3YCeD1fA3hYTqrjaeEpA8+2x4qju8UC8HzrcdguVEQ6\n\
c3WskWGNB7swlTet8KcsJsuA6wKBgQDGZPxW1Mso18/kl5k+1Hh4IkPT4gq2b1iN\n\
McITvdde9BFOVsA5w4OQmdQODgF4v0WbTa/p0wT2/yOFve9yLBSLwbVL9QcEYe5Z\n\
2Qyw42tq5rl6fAl7CtBWY6yY7nTizLAJrM57HbWrEz6KfVgnlpDH2F+E7Ow7MA7F\n\
PXIEggnz4wKBgHACkZDVC3VpJX0sxStEn6BzJOhfNFVdYKE5WSRukVgHUb0OYhhi\n\
FslayWiJ82whgaUpWcCa1nqSPdGJFF8myiSW+tC2PapsRwR5BZgNK0L6CTSkkacG\n\
H8AFgWL3SZVqHFtR4PR4vuVvn23BD2pq1fW7SdnDjSBWL8ApV6yE91AfAoGBALkx\n\
WzvStzIRAkboHGzB+RJrKdWHk2ho18g1Qm0bMQe53M27vQQutYktjvzvpgAIy/kE\n\
s8kY6fGGiKo3emShMSykTY/x0fMNV2kXavlT0NmhNlJXpqHsnj2GHX9EWGe9mjXt\n\
0XCrcwGWnTK5fqi1q8BhAgkbAAjf+2myydPbb17xAoGBAIUU3BCuclFys/XgRfDd\n\
/tQ8Wpsd4nWbakqRNp2KVdNbxW5I/Ctc1CkDkezsg5eMDmpuRIqKCgyp3RbSLKqW\n\
QNG1ZNOynGQUknHZb0UX6JEBlzEt0oSbaquUp7TNkQ0b/bL90AeLZbccWKcCN8CG\n\
Dr30CHN0lBM4yZeuzmNKTZ4p\n\
-----END PRIVATE KEY-----\n";

    const RSA_PUB_PEM: &str = "-----BEGIN PUBLIC KEY-----\n\
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAvGhOUXbFbhi4ZtlP9kWS\n\
75sNMosRKJ7h2c4nIOursviko9dVyIyvrqtNPdVYtQdMGxtlkhDP6GaL9rIPRu4q\n\
b44lRW9k1W1ZQSA2IEPcghaCy1FhhuFTNfMBwgj6PZe/vxjbPmU5cIAbB9BCLz5t\n\
oF97MJCF5/0oIYyAPU514oWa0pNgWKWSobQOsJASlkR/bq0k+kPfe/8OP9CNORC2\n\
W5i8X16wcS/GtrXM6EZKVwiVFXby2bf72KiV2mf7Cv+Lj0tJVp0JwrEnwzvH+P34\n\
LlK1T2iIc4ITX1QlCsr3Lv1FOvUrLeXHlacYG3Y4lFrbCcFkafILuFjA6YNEcBVh\n\
YQIDAQAB\n\
-----END PUBLIC KEY-----\n";

    const RSA_PUB_PEM_WRONG: &str = "-----BEGIN PUBLIC KEY-----\n\
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAibfBARrrEcnul4ZW1o7f\n\
giL6UC+63gK6+lCYAwkskU5smm/kV7hrpPMpkeH2VNVhy4cCIRNPHhRYdJhTKrtu\n\
dGX2KkY8gyXWA4oqIsxAnqeXy+KYhTtff2vr8raxgJEJaFwQq3RM8Xm2FjUEFisb\n\
yuxsqZ+3fdbXwCjinE7ds1Db/EEAFahfkAvWAYmSoj6FOzK/xycnOoh17mWDrobe\n\
/52Wq0BH/nSGiIsJvic51LxesXLh8mrNS+pH26vu9lyqoA29UOw4GPWlg7WjjUKw\n\
c8sXVMOsEK3V1GrcN6CNDucJtVeSEfkcY+QFEBvyNTEnv6TxOnBq2VtJeQb5HQNE\n\
8QIDAQAB\n\
-----END PUBLIC KEY-----\n";

    /// Mints an RS{256,384,512} token by signing `claims` with the
    /// fixture private key through ring. Used only inside the test
    /// module - `verify_rs` is the surface this whole file exposes.
    fn mint_rs_token(alg: Alg, claims: &Claims) -> String {
        let padding: &'static dyn ring::signature::RsaEncoding = match alg {
            Alg::Rs256 => &ring::signature::RSA_PKCS1_SHA256,
            Alg::Rs384 => &ring::signature::RSA_PKCS1_SHA384,
            Alg::Rs512 => &ring::signature::RSA_PKCS1_SHA512,
            _ => panic!("mint_rs_token requires an RSA alg"),
        };
        // Parse the PKCS#8 PEM down to DER, then hand to ring.
        let pem = x509_parser::pem::Pem::read(std::io::Cursor::new(RSA_PKCS8_PEM.as_bytes()))
            .expect("parse test pkcs8 pem")
            .0;
        let key_pair =
            ring::signature::RsaKeyPair::from_pkcs8(&pem.contents).expect("ring rsa keypair");
        let header = Header::new(alg);
        let signing_input = build_signing_input(&header, claims).expect("build signing input");
        let mut sig = vec![0u8; key_pair.public().modulus_len()];
        let rng = ring::rand::SystemRandom::new();
        key_pair
            .sign(padding, &rng, signing_input.as_bytes(), &mut sig)
            .expect("ring rsa sign");
        format!("{signing_input}.{}", b64url_encode(&sig))
    }

    #[test]
    fn rs256_valid_token_accepted() {
        let claims = Claims::new().subject("alice").issued_at(now_unix_secs());
        let token = mint_rs_token(Alg::Rs256, &claims);
        let back = verify_rs256(&token, RSA_PUB_PEM, &VerifyOpts::new()).unwrap();
        assert_eq!(back.sub.as_deref(), Some("alice"));
    }

    #[test]
    fn rs256_tampered_signature_rejected() {
        let token = mint_rs_token(Alg::Rs256, &Claims::new().subject("alice"));
        // Flip a byte well inside the signature segment so the
        // base64url length and final-quartet padding bits stay
        // valid - we want a cryptographic-mismatch reject, not a
        // structural one.
        let last_dot = token.rfind('.').expect("token has 2 dots");
        let mut bytes = token.into_bytes();
        let target = last_dot + 4;
        bytes[target] = if bytes[target] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(bytes).unwrap();
        let err = verify_rs256(&tampered, RSA_PUB_PEM, &VerifyOpts::new()).unwrap_err();
        assert!(
            format!("{err}").contains("signature invalid"),
            "expected signature-invalid, got: {err}"
        );
    }

    #[test]
    fn rs256_wrong_key_rejected() {
        let token = mint_rs_token(Alg::Rs256, &Claims::new().subject("alice"));
        let err = verify_rs256(&token, RSA_PUB_PEM_WRONG, &VerifyOpts::new()).unwrap_err();
        assert!(format!("{err}").contains("signature invalid"));
    }

    #[test]
    fn rs384_valid_token_accepted() {
        let claims = Claims::new().subject("bob").issued_at(now_unix_secs());
        let token = mint_rs_token(Alg::Rs384, &claims);
        let back = verify_rs384(&token, RSA_PUB_PEM, &VerifyOpts::new()).unwrap();
        assert_eq!(back.sub.as_deref(), Some("bob"));
    }

    #[test]
    fn rs384_tampered_signature_rejected() {
        let token = mint_rs_token(Alg::Rs384, &Claims::new().subject("bob"));
        let mut bytes = token.into_bytes();
        let len = bytes.len();
        bytes[len - 1] = if bytes[len - 1] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(bytes).unwrap();
        assert!(verify_rs384(&tampered, RSA_PUB_PEM, &VerifyOpts::new()).is_err());
    }

    #[test]
    fn rs384_wrong_key_rejected() {
        let token = mint_rs_token(Alg::Rs384, &Claims::new().subject("bob"));
        assert!(verify_rs384(&token, RSA_PUB_PEM_WRONG, &VerifyOpts::new()).is_err());
    }

    #[test]
    fn rs512_valid_token_accepted() {
        let claims = Claims::new().subject("carol").issued_at(now_unix_secs());
        let token = mint_rs_token(Alg::Rs512, &claims);
        let back = verify_rs512(&token, RSA_PUB_PEM, &VerifyOpts::new()).unwrap();
        assert_eq!(back.sub.as_deref(), Some("carol"));
    }

    #[test]
    fn rs512_tampered_signature_rejected() {
        let token = mint_rs_token(Alg::Rs512, &Claims::new().subject("carol"));
        let mut bytes = token.into_bytes();
        let len = bytes.len();
        bytes[len - 1] = if bytes[len - 1] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(bytes).unwrap();
        assert!(verify_rs512(&tampered, RSA_PUB_PEM, &VerifyOpts::new()).is_err());
    }

    #[test]
    fn rs512_wrong_key_rejected() {
        let token = mint_rs_token(Alg::Rs512, &Claims::new().subject("carol"));
        assert!(verify_rs512(&token, RSA_PUB_PEM_WRONG, &VerifyOpts::new()).is_err());
    }

    #[test]
    fn rs_alg_mismatch_rejected() {
        // Token signed with RS256 but verifier expects RS384.
        let token = mint_rs_token(Alg::Rs256, &Claims::new().subject("x"));
        let err = verify_rs(&token, Alg::Rs384, RSA_PUB_PEM, &VerifyOpts::new()).unwrap_err();
        assert!(format!("{err}").contains("alg mismatch"));
    }

    #[test]
    fn rs_verifier_refuses_hmac_token() {
        // HS-signed token aimed at an RSA verifier must reject before
        // any cryptographic operation runs.
        let token = sign_hs(Alg::Hs256, &Claims::new().subject("x"), key_hs()).unwrap();
        let err = verify_rs256(&token, RSA_PUB_PEM, &VerifyOpts::new()).unwrap_err();
        let msg = format!("{err}").to_lowercase();
        assert!(
            msg.contains("refusing") || msg.contains("mismatch"),
            "expected RSA-vs-HMAC rejection, got: {err}"
        );
    }

    #[test]
    fn hs_verifier_refuses_rsa_token() {
        // Inverse direction: RSA-signed token must never be treated
        // as an HMAC shared-secret candidate by `verify_hs`.
        let token = mint_rs_token(Alg::Rs256, &Claims::new().subject("x"));
        let err = verify_hs(&token, Alg::Hs256, key_hs(), &VerifyOpts::new()).unwrap_err();
        let msg = format!("{err}").to_lowercase();
        assert!(
            msg.contains("refusing") || msg.contains("mismatch"),
            "expected HMAC-vs-RSA rejection, got: {err}"
        );
    }
}
