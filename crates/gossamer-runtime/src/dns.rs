//! A stub DNS resolver for the record types `net::lookup` does not cover:
//! MX, TXT, SRV, CNAME, and PTR. It asks the system's nameservers (or one the
//! program names) over UDP, retries over TCP when an answer is truncated, and
//! answers each record as flat text fields, so every tier reads the same
//! records.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::Duration;

/// A record type this resolver asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordType {
    /// Mail exchangers: preference, then host.
    Mx,
    /// Text records: one string per record, its character-strings joined.
    Txt,
    /// Service locations: priority, weight, port, then target.
    Srv,
    /// The canonical name an alias stands for.
    Cname,
    /// The names an address maps back to.
    Ptr,
}

impl RecordType {
    /// The record type a program names by its DNS type number.
    #[must_use]
    pub fn from_code(code: i64) -> Option<Self> {
        Some(match code {
            15 => Self::Mx,
            16 => Self::Txt,
            33 => Self::Srv,
            5 => Self::Cname,
            12 => Self::Ptr,
            _ => return None,
        })
    }

    fn code(self) -> u16 {
        match self {
            Self::Mx => 15,
            Self::Txt => 16,
            Self::Srv => 33,
            Self::Cname => 5,
            Self::Ptr => 12,
        }
    }
}

const TIMEOUT: Duration = Duration::from_secs(3);
const ATTEMPTS: usize = 2;

/// The records of `rtype` for `name`, each as its text fields in order (see
/// [`RecordType`]), from the nameserver `server` (`host:port`, port 53 when
/// left out) or else the system's. A PTR lookup takes an IP address.
///
/// # Errors
/// No nameserver configured, every nameserver failing to answer, a name that
/// does not exist, or an answer that does not parse.
pub fn lookup(
    name: &str,
    rtype: RecordType,
    server: Option<&str>,
) -> Result<Vec<Vec<String>>, String> {
    let qname = if rtype == RecordType::Ptr {
        reverse_name(name)?
    } else {
        name.trim_end_matches('.').to_string()
    };
    let servers = match server {
        Some(server) if !server.is_empty() => vec![server_address(server)?],
        _ => system_nameservers()?,
    };
    let query = encode_query(query_id(), &qname, rtype.code())?;
    let mut last_error = String::from("no nameserver answered");
    for server in &servers {
        for _ in 0..ATTEMPTS {
            match ask(*server, &query) {
                Ok(answer) => {
                    return decode_answer(&query, &answer, rtype)
                        .map_err(|e| format!("dns: {qname}: {e}"));
                }
                Err(error) => last_error = error,
            }
        }
    }
    Err(format!("dns: {qname}: {last_error}"))
}

/// `server`, a nameserver address with port 53 when it names none.
fn server_address(server: &str) -> Result<SocketAddr, String> {
    let with_port = if server.parse::<std::net::IpAddr>().is_ok() {
        if server.contains(':') {
            format!("[{server}]:53")
        } else {
            format!("{server}:53")
        }
    } else {
        server.to_string()
    };
    with_port
        .to_socket_addrs()
        .map_err(|e| format!("dns: nameserver {server}: {e}"))?
        .next()
        .ok_or_else(|| format!("dns: nameserver {server} names no address"))
}

/// The in-addr.arpa or ip6.arpa name of the address `ip`.
fn reverse_name(ip: &str) -> Result<String, String> {
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => {
            let [a, b, c, d] = v4.octets();
            Ok(format!("{d}.{c}.{b}.{a}.in-addr.arpa"))
        }
        Ok(std::net::IpAddr::V6(v6)) => {
            let mut name = String::new();
            for byte in v6.octets().iter().rev() {
                name.push_str(&format!("{:x}.{:x}.", byte & 0x0f, byte >> 4));
            }
            name.push_str("ip6.arpa");
            Ok(name)
        }
        Err(_) => Err(format!("dns: `{ip}` is not an IP address")),
    }
}

/// A query id: the low bits of the clock and a counter, so two queries in
/// flight carry different ids.
fn query_id() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static COUNTER: AtomicU16 = AtomicU16::new(0);
    let clock = crate::platform::system_time_now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let low = u16::try_from(clock & 0xffff).unwrap_or(0);
    low ^ COUNTER.fetch_add(0x9e37, Ordering::Relaxed)
}

/// A recursion-desired query for `name` and `qtype`, class IN.
fn encode_query(id: u16, name: &str, qtype: u16) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(32 + name.len());
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in name.split('.').filter(|l| !l.is_empty()) {
        let len = u8::try_from(label.len())
            .ok()
            .filter(|len| *len <= 63)
            .ok_or_else(|| format!("dns: label `{label}` is longer than 63 bytes"))?;
        out.push(len);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    Ok(out)
}

/// The answer `server` gives to `query`, over UDP, then over TCP when the
/// UDP answer is truncated.
fn ask(server: SocketAddr, query: &[u8]) -> Result<Vec<u8>, String> {
    let bind = if server.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind).map_err(|e| e.to_string())?;
    socket
        .set_read_timeout(Some(TIMEOUT))
        .map_err(|e| e.to_string())?;
    socket.connect(server).map_err(|e| e.to_string())?;
    socket.send(query).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 4096];
    loop {
        let n = socket.recv(&mut buf).map_err(|e| e.to_string())?;
        let answer = &buf[..n];
        // An answer to another query (a late reply to an earlier attempt) is
        // not this one's.
        if answer.len() < 12 || answer[..2] != query[..2] {
            continue;
        }
        if answer[2] & 0x02 == 0 {
            return Ok(answer.to_vec());
        }
        return ask_tcp(server, query);
    }
}

fn ask_tcp(server: SocketAddr, query: &[u8]) -> Result<Vec<u8>, String> {
    let mut stream = TcpStream::connect_timeout(&server, TIMEOUT).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .map_err(|e| e.to_string())?;
    let len = u16::try_from(query.len()).map_err(|_| "dns: query too long".to_string())?;
    stream
        .write_all(&len.to_be_bytes())
        .map_err(|e| e.to_string())?;
    stream.write_all(query).map_err(|e| e.to_string())?;
    let mut prefix = [0u8; 2];
    stream.read_exact(&mut prefix).map_err(|e| e.to_string())?;
    let mut answer = vec![0u8; usize::from(u16::from_be_bytes(prefix))];
    stream.read_exact(&mut answer).map_err(|e| e.to_string())?;
    Ok(answer)
}

/// The domain name at `at` in `msg`, following compression pointers, and the
/// offset just past the name where it was written.
fn read_name(msg: &[u8], mut at: usize) -> Result<(String, usize), String> {
    let bad = || "malformed name".to_string();
    let mut labels: Vec<String> = Vec::new();
    let mut end = None;
    let mut jumps = 0;
    loop {
        let len = *msg.get(at).ok_or_else(bad)?;
        match len & 0xc0 {
            0x00 if len == 0 => {
                end.get_or_insert(at + 1);
                break;
            }
            0x00 => {
                let label = msg.get(at + 1..at + 1 + usize::from(len)).ok_or_else(bad)?;
                labels.push(String::from_utf8_lossy(label).into_owned());
                at += 1 + usize::from(len);
            }
            0xc0 => {
                let low = *msg.get(at + 1).ok_or_else(bad)?;
                end.get_or_insert(at + 2);
                jumps += 1;
                if jumps > 64 {
                    return Err(bad());
                }
                at = (usize::from(len & 0x3f) << 8) | usize::from(low);
            }
            _ => return Err(bad()),
        }
    }
    Ok((labels.join("."), end.unwrap_or(at)))
}

fn read_u16(msg: &[u8], at: usize) -> Result<u16, String> {
    msg.get(at..at + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .ok_or_else(|| "truncated answer".to_string())
}

/// The records of `rtype` in `answer`, the reply to `query`.
fn decode_answer(
    query: &[u8],
    answer: &[u8],
    rtype: RecordType,
) -> Result<Vec<Vec<String>>, String> {
    if answer.len() < 12 || answer[..2] != query[..2] || answer[2] & 0x80 == 0 {
        return Err("the reply does not answer the query".to_string());
    }
    match answer[3] & 0x0f {
        0 => {}
        3 => return Err("no such host".to_string()),
        2 => return Err("the nameserver failed (SERVFAIL)".to_string()),
        5 => return Err("the nameserver refused the query".to_string()),
        code => return Err(format!("the nameserver answered error {code}")),
    }
    let questions = read_u16(answer, 4)?;
    let answers = read_u16(answer, 6)?;
    let mut at = 12;
    for _ in 0..questions {
        at = read_name(answer, at)?.1 + 4;
    }
    let mut records = Vec::new();
    for _ in 0..answers {
        let (_, after) = read_name(answer, at)?;
        let kind = read_u16(answer, after)?;
        let len = usize::from(read_u16(answer, after + 8)?);
        let data = after + 10;
        if answer.len() < data + len {
            return Err("truncated answer".to_string());
        }
        at = data + len;
        if kind != rtype.code() {
            continue;
        }
        let name_at = |offset: usize| read_name(answer, data + offset).map(|(name, _)| name);
        records.push(match rtype {
            RecordType::Mx => vec![read_u16(answer, data)?.to_string(), name_at(2)?],
            RecordType::Srv => vec![
                read_u16(answer, data)?.to_string(),
                read_u16(answer, data + 2)?.to_string(),
                read_u16(answer, data + 4)?.to_string(),
                name_at(6)?,
            ],
            RecordType::Cname | RecordType::Ptr => vec![name_at(0)?],
            RecordType::Txt => {
                let mut text = Vec::new();
                let mut part = data;
                while part < data + len {
                    let n = usize::from(answer[part]);
                    let bytes = answer
                        .get(part + 1..part + 1 + n)
                        .ok_or_else(|| "truncated text record".to_string())?;
                    text.extend_from_slice(bytes);
                    part += 1 + n;
                }
                vec![String::from_utf8_lossy(&text).into_owned()]
            }
        });
    }
    Ok(records)
}

/// The nameservers `/etc/resolv.conf` lists.
#[cfg(unix)]
fn system_nameservers() -> Result<Vec<SocketAddr>, String> {
    let conf = std::fs::read_to_string("/etc/resolv.conf")
        .map_err(|e| format!("dns: /etc/resolv.conf: {e}"))?;
    let servers: Vec<SocketAddr> = conf
        .lines()
        .filter_map(|line| line.trim().strip_prefix("nameserver"))
        .filter_map(|rest| {
            // A link-local IPv6 server carries its zone after `%`, which
            // `IpAddr` does not read; such a server is left out.
            let ip = rest.trim().parse::<std::net::IpAddr>().ok()?;
            Some(SocketAddr::new(ip, 53))
        })
        .collect();
    if servers.is_empty() {
        return Err("dns: /etc/resolv.conf lists no nameserver".to_string());
    }
    Ok(servers)
}

/// The DNS servers of the adapters that are up.
#[cfg(windows)]
fn system_nameservers() -> Result<Vec<SocketAddr>, String> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_MULTICAST, GAA_FLAG_SKIP_UNICAST,
        GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
    };

    const ERROR_BUFFER_OVERFLOW: u32 = 111;
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_UNICAST;
    let mut size: u32 = 16 * 1024;
    let mut buffer: Vec<u64> = Vec::new();
    for _ in 0..3 {
        buffer = vec![0u64; (size as usize).div_ceil(8)];
        // SAFETY: `buffer` holds `size` writable bytes, 8-aligned as the
        // adapter records need, and `size` is updated in place.
        let status = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_UNSPEC),
                flags,
                std::ptr::null(),
                buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>(),
                &raw mut size,
            )
        };
        match status {
            0 => break,
            ERROR_BUFFER_OVERFLOW => {}
            code => return Err(format!("dns: GetAdaptersAddresses failed ({code})")),
        }
    }
    let mut servers = Vec::new();
    let mut adapter = buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    while !adapter.is_null() {
        // SAFETY: `adapter` walks the linked list the call wrote into `buffer`.
        let record = unsafe { &*adapter };
        if record.OperStatus == IfOperStatusUp {
            let mut server = record.FirstDnsServerAddress;
            while !server.is_null() {
                // SAFETY: `server` walks the adapter's DNS server list.
                let entry = unsafe { &*server };
                let sockaddr = entry.Address.lpSockaddr;
                if !sockaddr.is_null() {
                    // SAFETY: `sockaddr` points at a socket address of the
                    // family its first field names.
                    let family = unsafe { (*sockaddr).sa_family };
                    if family == AF_INET {
                        // SAFETY: an AF_INET address is a SOCKADDR_IN, read
                        // whatever the alignment of the storage holding it.
                        let v4 = unsafe { sockaddr.cast::<SOCKADDR_IN>().read_unaligned() };
                        // SAFETY: every bit pattern of the union is a u32.
                        let raw = unsafe { v4.sin_addr.S_un.S_addr };
                        let ip = std::net::Ipv4Addr::from(u32::from_be(raw));
                        servers.push(SocketAddr::new(ip.into(), 53));
                    } else if family == AF_INET6 {
                        // SAFETY: an AF_INET6 address is a SOCKADDR_IN6, read
                        // whatever the alignment of the storage holding it.
                        let v6 = unsafe { sockaddr.cast::<SOCKADDR_IN6>().read_unaligned() };
                        // SAFETY: every bit pattern of the union is bytes.
                        let bytes = unsafe { v6.sin6_addr.u.Byte };
                        let ip = std::net::Ipv6Addr::from(bytes);
                        // A site-local resolver placeholder (fec0::/10) is
                        // what Windows lists for an adapter with none.
                        if ip.segments()[0] & 0xffc0 != 0xfec0 {
                            servers.push(SocketAddr::new(ip.into(), 53));
                        }
                    }
                }
                server = entry.Next;
            }
        }
        adapter = record.Next;
    }
    if servers.is_empty() {
        return Err("dns: no adapter lists a DNS server".to_string());
    }
    Ok(servers)
}

// Platform: a target with neither resolv.conf nor adapter records names no
// nameserver; a lookup there passes `server:`.
#[cfg(not(any(unix, windows)))]
fn system_nameservers() -> Result<Vec<SocketAddr>, String> {
    Err("dns: this platform names no nameserver; pass `server:`".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reply to `query` carrying `records` (type, rdata), each named by a
    /// pointer to the question.
    fn reply(query: &[u8], records: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut out = query.to_vec();
        out[2] = 0x81;
        out[3] = 0x80;
        out[6..8].copy_from_slice(&u16::try_from(records.len()).unwrap().to_be_bytes());
        for (kind, data) in records {
            out.extend_from_slice(&[0xc0, 0x0c]);
            out.extend_from_slice(&kind.to_be_bytes());
            out.extend_from_slice(&[0, 1, 0, 0, 0, 60]);
            out.extend_from_slice(&u16::try_from(data.len()).unwrap().to_be_bytes());
            out.extend_from_slice(data);
        }
        out
    }

    #[test]
    fn mx_and_txt_records_decode_with_compressed_names() {
        let query = encode_query(7, "example.test", 15).unwrap();
        let mut mx = vec![0, 10, 4];
        mx.extend_from_slice(b"mail");
        mx.extend_from_slice(&[0xc0, 0x0c]);
        let answer = reply(&query, &[(15, mx)]);
        let records = decode_answer(&query, &answer, RecordType::Mx).unwrap();
        assert_eq!(
            records,
            vec![vec!["10".to_string(), "mail.example.test".to_string()]]
        );

        let query = encode_query(8, "example.test", 16).unwrap();
        let answer = reply(&query, &[(16, b"\x05hello\x06 world".to_vec())]);
        let records = decode_answer(&query, &answer, RecordType::Txt).unwrap();
        assert_eq!(records, vec![vec!["hello world".to_string()]]);
    }

    #[test]
    fn a_missing_name_and_a_pointer_loop_are_errors() {
        let query = encode_query(9, "nope.test", 15).unwrap();
        let mut answer = reply(&query, &[]);
        answer[3] = 0x83;
        assert_eq!(
            decode_answer(&query, &answer, RecordType::Mx).unwrap_err(),
            "no such host"
        );
        assert!(read_name(&[0xc0, 0x00], 0).is_err());
    }

    #[test]
    fn reverse_names_cover_both_families() {
        assert_eq!(reverse_name("192.0.2.5").unwrap(), "5.2.0.192.in-addr.arpa");
        assert!(reverse_name("2001:db8::1").unwrap().starts_with("1.0.0.0."));
        assert!(reverse_name("not-an-ip").is_err());
    }
}
