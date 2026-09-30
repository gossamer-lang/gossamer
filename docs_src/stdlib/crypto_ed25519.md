# `std::crypto::ed25519`

Status: experimental

Ed25519 digital signatures.

## Items

| Item | Signature | Description |
|---|---|---|
| `keypair` | `fn keypair() -> Result<(Vec<u8>, Vec<u8>), errors::Error>` | Generates a fresh Ed25519 keypair from the host CSPRNG. |
| `sign` | `fn sign(secret: Vec<u8>, message: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | Signs a message with a 32-byte secret key. |
| `verify` | `fn verify(public: Vec<u8>, message: Vec<u8>, signature: Vec<u8>) -> Result<(), errors::Error>` | Verifies a 64-byte signature against a 32-byte public key. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
