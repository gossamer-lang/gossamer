# `std::crypto::ed25519`

Status: experimental

Ed25519 digital signatures.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## API details and source

The [implementation source](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) contains the complete declarations and implementation notes. The table below lists canonical Gossamer call signatures; every item name links directly to its implementation file.

| Item | Canonical signature or declaration | Description |
|---|---|---|
| [`keypair`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn keypair() -> Result<(Vec<u8>, Vec<u8>), errors::Error>` | Generates a fresh Ed25519 keypair from the host CSPRNG. |
| [`sign`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn sign(secret: Vec<u8>, message: Vec<u8>) -> Result<Vec<u8>, errors::Error>` | Signs a message with a 32-byte secret key. |
| [`verify`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn verify(public: Vec<u8>, message: Vec<u8>, signature: Vec<u8>) -> Result<(), errors::Error>` | Verifies a 64-byte signature against a 32-byte public key. |
