# `std::net::netip`

Status: experimental

Typed IP-address parsing, classification, and addr:port helpers (Go's net/netip shape).

## Items

| Item | Signature | Description |
|---|---|---|
| `is_valid` | `fn is_valid(addr: String) -> bool` | Return true iff the string parses as a v4 or v6 IP. |
| `is_v4` | `fn is_v4(addr: String) -> bool` | Return true iff the string parses as a v4 IP. |
| `is_v6` | `fn is_v6(addr: String) -> bool` | Return true iff the string parses as a v6 IP. |
| `is_loopback` | `fn is_loopback(addr: String) -> bool` | Return true iff the string parses as a loopback IP (127.0.0.1 / ::1). |
| `is_unspecified` | `fn is_unspecified(addr: String) -> bool` | Return true iff the string parses as the unspecified IP (0.0.0.0 / ::). |
| `is_multicast` | `fn is_multicast(addr: String) -> bool` | Return true iff the string parses as a multicast IP. |
| `is_private` | `fn is_private(addr: String) -> bool` | Return true iff the IP is RFC1918 (v4) or ULA fc00::/7 (v6). |
| `normalize` | `fn normalize(addr: String) -> Result<String, errors::Error>` | Canonical lowercase form of the IP, or empty string on parse failure. |
| `host_of` | `fn host_of(addr_port: String) -> String` | Host portion of an addr:port string, or empty on parse failure. |
| `port_of` | `fn port_of(addr_port: String) -> i64` | Port portion of an addr:port string, or -1 on parse failure. |
| `join_addr_port` | `fn join_addr_port(addr: String, port: i64) -> String` | Compose an addr:port string from host and port, or empty on failure. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
