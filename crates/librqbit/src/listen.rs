use std::{
    net::{Ipv6Addr, SocketAddr},
    num::NonZeroUsize,
    sync::Arc,
};

use anyhow::Context;
use dontfrag::UdpSocketExt;
use librqbit_dualstack_sockets::{BindOpts, TcpListener, UdpSocket};
use librqbit_utp::{BindDevice, UtpSocketUdp};
use tokio::io::AsyncWrite;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{stream_connect::ConnectionKind, vectored_traits::AsyncReadVectored};

/// librqbit-utp defaults for the options below, used to size the receive buffer
/// the same way `UtpSocketUdp::new_udp_with_opts` does.
const UTP_DEFAULT_MAX_LIVE_VSOCKS: usize = 128;
const UTP_DEFAULT_VSOCK_RX_BUFSIZE: usize = 1024 * 1024;

/// The uTP socket, set up like `UtpSocketUdp::new_udp_with_opts` does.
///
/// The difference is Windows: there a UDP socket reports an ICMP "port unreachable"
/// for an earlier datagram as WSAECONNRESET (10054) from the next `recv_from`, and an
/// ICMP "TTL expired" as WSAENETRESET (10052). librqbit-utp stops its dispatcher on any
/// receive error, so the first peer without an open UDP port killed uTP for the whole
/// session within seconds of the start. Such reports carry nothing for a socket shared by
/// all peers, so they are switched off.
fn bind_utp_socket(
    addr: SocketAddr,
    opts: librqbit_utp::SocketOpts,
    device: Option<&BindDevice>,
) -> anyhow::Result<Arc<UtpSocketUdp>> {
    let socket = UdpSocket::bind_udp(
        addr,
        BindOpts {
            request_dualstack: true,
            reuseport: false,
            device,
        },
    )
    .context("error binding UDP socket for uTP")?;

    set_dont_fragment(socket.socket(), addr);
    set_receive_buffer(socket.socket(), utp_receive_buffer_size(&opts));
    #[cfg(windows)]
    ignore_icmp_errors(socket.socket());

    Ok(UtpSocketUdp::new_with_opts(
        socket,
        Default::default(),
        opts,
    )?)
}

fn set_dont_fragment(socket: &tokio::net::UdpSocket, addr: SocketAddr) {
    if addr.is_ipv4() {
        if let Err(e) = socket.set_dontfrag_v4(true) {
            warn!("error setting IPV4_DONTFRAG: {e:#}");
        }
    } else if let Err(e) = socket.set_dontfrag_v6(true) {
        debug!("error setting IPV6_DONTFRAG: {e:#}");
    }
}

/// Large enough to fit the RX windows of all streams in case the socket isn't drained fast enough.
fn utp_receive_buffer_size(opts: &librqbit_utp::SocketOpts) -> usize {
    let streams = opts
        .max_live_vsocks
        .map_or(UTP_DEFAULT_MAX_LIVE_VSOCKS, NonZeroUsize::get);
    let per_stream = opts
        .vsock_rx_bufsize_bytes
        .map_or(UTP_DEFAULT_VSOCK_RX_BUFSIZE, NonZeroUsize::get);

    // Some heuristic overhead for uTP and ACK packets.
    streams * per_stream * 10 / 8
}

fn set_receive_buffer(socket: &tokio::net::UdpSocket, size: usize) {
    let socket = socket2::SockRef::from(socket);

    match socket.set_recv_buffer_size(size) {
        Ok(()) => debug!(
            requested = size,
            current = ?socket.recv_buffer_size().ok(),
            "set UDP receive buffer size for uTP"
        ),
        Err(e) => warn!(
            requested = size,
            "error setting UDP receive buffer size: {e:#}"
        ),
    }
}

#[cfg(windows)]
fn ignore_icmp_errors(socket: &tokio::net::UdpSocket) {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{
        SIO_UDP_CONNRESET, SIO_UDP_NETRESET, SOCKET, WSAGetLastError, WSAIoctl,
    };

    let raw = SOCKET(socket.as_raw_socket() as usize);
    let disabled: u32 = 0;

    for (code, name) in [
        (SIO_UDP_CONNRESET, "SIO_UDP_CONNRESET"),
        (SIO_UDP_NETRESET, "SIO_UDP_NETRESET"),
    ] {
        let mut returned = 0u32;
        // SAFETY: `raw` is a live socket owned by `socket`; the input buffer is a 4-byte BOOL
        // that outlives the synchronous call, no output buffer and no overlapped I/O.
        let result = unsafe {
            WSAIoctl(
                raw,
                code,
                Some(std::ptr::from_ref(&disabled).cast()),
                size_of::<u32>() as u32,
                None,
                0,
                &mut returned,
                None,
                None,
            )
        };

        if result != 0 {
            let error = unsafe { WSAGetLastError() };
            warn!(
                ioctl = name,
                error = error.0,
                "error disabling UDP ICMP error reports"
            );
        }
    }
}

pub(crate) struct ListenResult {
    pub tcp_socket: Option<TcpListener>,
    pub utp_socket: Option<Arc<UtpSocketUdp>>,
    pub enable_upnp_port_forwarding: bool,
    pub addr: SocketAddr,
    pub announce_port: Option<u16>,
    pub max_pending_incoming_handshake_checks: usize,
}

#[derive(Debug, Clone, Copy)]
pub enum ListenerMode {
    TcpOnly,
    UtpOnly,
    TcpAndUtp,
}

pub const DEFAULT_MAX_PENDING_INCOMING_HANDSHAKE_CHECKS: usize = 256;

impl ListenerMode {
    pub fn tcp_enabled(&self) -> bool {
        match self {
            ListenerMode::TcpOnly => true,
            ListenerMode::UtpOnly => false,
            ListenerMode::TcpAndUtp => true,
        }
    }

    pub fn utp_enabled(&self) -> bool {
        match self {
            ListenerMode::TcpOnly => false,
            ListenerMode::UtpOnly => true,
            ListenerMode::TcpAndUtp => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ListenerOptions {
    pub mode: ListenerMode,
    pub listen_addr: SocketAddr,
    pub enable_upnp_port_forwarding: bool,
    pub utp_opts: Option<librqbit_utp::SocketOpts>,
    pub announce_port: Option<u16>,
    pub ipv4_only: bool,
    pub max_pending_incoming_handshake_checks: usize,
}

impl Default for ListenerOptions {
    fn default() -> Self {
        Self {
            // TODO: once uTP is stable upgrade default to both
            mode: ListenerMode::TcpOnly,
            listen_addr: (Ipv6Addr::UNSPECIFIED, 0).into(),
            enable_upnp_port_forwarding: false,
            utp_opts: None,
            announce_port: None,
            ipv4_only: false,
            max_pending_incoming_handshake_checks: DEFAULT_MAX_PENDING_INCOMING_HANDSHAKE_CHECKS,
        }
    }
}

impl ListenerOptions {
    pub(crate) async fn start(
        mut self,
        parent_span: Option<tracing::Id>,
        cancellation_token: CancellationToken,
        bind_device: Option<&BindDevice>,
    ) -> anyhow::Result<ListenResult> {
        let mut utp_opts = self.utp_opts.take().unwrap_or_default();
        utp_opts.cancellation_token = cancellation_token.clone();
        utp_opts.parent_span = parent_span;
        utp_opts.dont_wait_for_lastack = true;

        let mut listen_addr = if self.ipv4_only {
            if self.listen_addr.is_ipv6() && self.listen_addr.ip().is_unspecified() {
                // Force to IPv4 unspecified if IPv6 unspecified was requested but we are v4 only
                SocketAddr::from(([0, 0, 0, 0], self.listen_addr.port()))
            } else {
                self.listen_addr
            }
        } else {
            self.listen_addr
        };

        let tcp_socket = if self.mode.tcp_enabled() {
            let listener = TcpListener::bind_tcp(
                listen_addr,
                BindOpts {
                    request_dualstack: !self.ipv4_only,
                    reuseport: false,
                    device: bind_device,
                },
            )
            .context("error starting TCP listener")?;
            listen_addr = listener.bind_addr();
            info!(
                "Listening on TCP {:?} for incoming peer connections",
                listen_addr
            );
            Some(listener)
        } else {
            None
        };

        let utp_socket = if self.mode.utp_enabled() {
            let bind_result = bind_utp_socket(listen_addr, utp_opts, bind_device);
            match bind_result {
                Ok(sock) => {
                    listen_addr = sock.bind_addr();
                    info!(
                        "Listening on UDP {:?} for incoming uTP peer connections",
                        listen_addr
                    );
                    Some(sock)
                }
                Err(e) if tcp_socket.is_some() => {
                    // If we listen over TCP, it's not a fatal error if we can't listen over uTP.
                    tracing::error!("Error listening on UDP {listen_addr:?}: {e:#}");
                    None
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        } else {
            None
        };

        let announce_port = if let Some(p) = self.announce_port {
            Some(p)
        } else if listen_addr.ip().is_loopback() {
            None
        } else {
            Some(listen_addr.port())
        };
        Ok(ListenResult {
            tcp_socket,
            utp_socket,
            announce_port,
            addr: listen_addr,
            enable_upnp_port_forwarding: self.enable_upnp_port_forwarding,
            max_pending_incoming_handshake_checks: self.max_pending_incoming_handshake_checks,
        })
    }
}

pub(crate) trait Accept {
    const KIND: ConnectionKind;

    async fn accept(
        &self,
    ) -> anyhow::Result<(
        SocketAddr,
        (
            impl AsyncReadVectored + Send + 'static,
            impl AsyncWrite + Unpin + Send + 'static,
        ),
    )>;
}

impl Accept for TcpListener {
    const KIND: ConnectionKind = ConnectionKind::Tcp;
    async fn accept(
        &self,
    ) -> anyhow::Result<(
        SocketAddr,
        (
            impl AsyncReadVectored + Send + 'static,
            impl AsyncWrite + Send + 'static,
        ),
    )> {
        let (stream, addr) = self.accept().await.context("error accepting TCP")?;
        let (read, write) = stream.into_split();
        Ok((addr, (read, write)))
    }
}

impl Accept for Arc<UtpSocketUdp> {
    const KIND: ConnectionKind = ConnectionKind::Utp;
    async fn accept(
        &self,
    ) -> anyhow::Result<(
        SocketAddr,
        (
            impl AsyncReadVectored + Send + 'static,
            impl AsyncWrite + Unpin + Send + 'static,
        ),
    )> {
        let stream = self.accept().await.context("error accepting uTP")?;
        let addr = stream.remote_addr();
        let (read, write) = stream.split();
        Ok((addr, (read, write)))
    }
}
