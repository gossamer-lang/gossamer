# `std::crypto::ecdsa`

Status: experimental

ECDSA over the NIST P-256 curve.

## Items

| Item | Signature | Description |
|---|---|---|
| `keypair_pem` | `fn keypair_pem() -> Result<(String, String), errors::Error>` | Generates (secret_pem, public_pem) for a fresh P-256 keypair. |
| `sign_pem` | `fn sign_pem(secret_pem: String, message: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | Signs a message with a PKCS#8-PEM-encoded P-256 secret key. |
| `verify_pem` | `fn verify_pem(public_pem: String, message: Vec<u8>, signature: Vec<u8>) -> Result<(), errors::Error>` | Verifies a DER-encoded signature against an SPKI-PEM public key. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
