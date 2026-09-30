# `std::net`

Status: experimental

TCP/UDP networking primitives.

## Items

| Item | Signature | Description |
|---|---|---|
| `UnixListener` | `type UnixListener` | Unix-domain socket listener. |
| `UnixStream` | `type UnixStream` | Connected Unix-domain byte stream. |
| `TcpListener` | `type TcpListener` | Accepts incoming TCP connections. |
| `TcpStream` | `type TcpStream` | Bidirectional TCP byte stream; supports read/write, TLS upgrade, close, and read/write timeout setters. |
| `UdpSocket` | `type UdpSocket` | Bound UDP socket for datagram I/O. |
| `lookup` | `fn lookup(host: String) -> Result<Vec<String>, errors::Error>` | Resolves a hostname to its IP addresses. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
