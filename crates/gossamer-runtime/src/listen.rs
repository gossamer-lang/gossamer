//! TCP listeners with the accept queue the system allows.
//!
//! `std::net::TcpListener::bind` listens with a fixed backlog of 128. A burst
//! of clients larger than that overflows the queue, the kernel drops the
//! surplus SYNs, and each dropped client waits out a full retransmission
//! timeout (a second on Linux) before its connection is even seen.

use std::io;
#[cfg(not(target_arch = "wasm32"))]
use std::net::SocketAddr;
use std::net::{TcpListener, ToSocketAddrs};

/// Backlog asked of `listen(2)`. Every supported kernel caps the request at
/// its own configured limit (`net.core.somaxconn`, `kern.ipc.somaxconn`,
/// `SOMAXCONN`), so asking for the largest an `int` holds yields that limit.
#[cfg(not(target_arch = "wasm32"))]
const LISTEN_BACKLOG: i32 = i32::MAX;

/// Binds `addr` and listens with the longest accept queue the system allows,
/// trying each address it resolves to in turn as `TcpListener::bind` does.
#[cfg(not(target_arch = "wasm32"))]
pub fn bind_tcp(addr: impl ToSocketAddrs) -> io::Result<TcpListener> {
    let mut last = None;
    for candidate in addr.to_socket_addrs()? {
        match bind_one(candidate) {
            Ok(listener) => return Ok(listener),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "could not resolve to any addresses",
        )
    }))
}

/// Binds `addr` as `TcpListener::bind` does: a wasm32 target has no
/// socket layer to set a queue length on.
#[cfg(target_arch = "wasm32")]
pub fn bind_tcp(addr: impl ToSocketAddrs) -> io::Result<TcpListener> {
    TcpListener::bind(addr)
}

#[cfg(not(target_arch = "wasm32"))]
fn bind_one(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    // Unix only, matching `TcpListener::bind`: on Windows the option lets a
    // second process bind the same port.
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(LISTEN_BACKLOG)?;
    Ok(socket.into())
}

#[cfg(test)]
mod tests {
    use super::bind_tcp;

    #[test]
    #[cfg_attr(miri, ignore)] // opens sockets, which Miri does not model
    fn a_bound_listener_accepts_a_connection() {
        let listener = bind_tcp("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let client = std::net::TcpStream::connect(addr).expect("connect");
        let (_, peer) = listener.accept().expect("accept");
        assert_eq!(peer, client.local_addr().expect("client addr"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // opens sockets, which Miri does not model
    fn a_port_already_listening_is_refused() {
        let first = bind_tcp("127.0.0.1:0").expect("bind");
        let addr = first.local_addr().expect("addr");
        assert!(bind_tcp(addr).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[cfg_attr(miri, ignore)] // opens sockets, which Miri does not model
    fn more_clients_than_std_queues_connect_before_any_is_accepted() {
        let somaxconn: usize = std::fs::read_to_string("/proc/sys/net/core/somaxconn")
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(128);
        let clients = somaxconn.min(400) - 2;
        let listener = bind_tcp("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        // A loopback connect completes once the kernel queues it, so every
        // client within the queue connects with nothing accepted yet.
        let held: Vec<_> = (0..clients)
            .map(|i| {
                std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500))
                    .unwrap_or_else(|e| panic!("client {i} of {clients} was not queued: {e}"))
            })
            .collect();
        assert_eq!(held.len(), clients);
    }
}
