# `std::net::ip`

Status: experimental

String-level IPv4 / IPv6 parsing and classification helpers.

## Items

| Item | Signature | Description |
|---|---|---|
| `parse` | `fn parse(addr: String) -> Result<net::ip::Addr, errors::Error>` | Parses an IP string, returning its canonical form or None. |
| `is_valid` | `fn is_valid(addr: String) -> bool` | Reports whether the string is a valid v4 or v6 IP. |
| `is_v4` | `fn is_v4(addr: String) -> bool` | Reports whether the string is a valid v4 IP. |
| `is_v6` | `fn is_v6(addr: String) -> bool` | Reports whether the string is a valid v6 IP. |
| `to_string` | `fn to_string(addr: net::ip::Addr) -> String` | Canonical lowercase string form of the IP. |
| `is_loopback` | `fn is_loopback(addr: String) -> bool` | Reports whether the IP is a loopback address. |
| `is_private` | `fn is_private(addr: String) -> bool` | Reports whether the IP is in a private range. |
| `is_multicast` | `fn is_multicast(addr: String) -> bool` | Reports whether the IP is a multicast address. |
| `is_unspecified` | `fn is_unspecified(addr: String) -> bool` | Reports whether the IP is the unspecified address. |
| `octets` | `fn octets(addr: net::ip::Addr) -> Vec<u8>` | Byte octets of the IP as a Vec. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
