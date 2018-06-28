use std::{
    net::SocketAddr,
    time::{Duration, Instant},
    sync::Arc,
};

use h2;

use ctx;
use connection;

#[derive(Clone, Debug)]
pub enum Event {
    TransportOpen(Arc<ctx::transport::Ctx>),
    TransportClose(Arc<ctx::transport::Ctx>, TransportClose),

    StreamRequestOpen(Arc<ctx::http::Request>),
    StreamRequestFail(Arc<ctx::http::Request>, StreamRequestFail),
    StreamRequestEnd(Arc<ctx::http::Request>, StreamRequestEnd),

    StreamResponseOpen(Arc<ctx::http::Response>, StreamResponseOpen),
    StreamResponseFail(Arc<ctx::http::Response>, StreamResponseFail),
    StreamResponseEnd(Arc<ctx::http::Response>, StreamResponseEnd),

    TlsHandshakeFailed(Arc<ctx::transport::Ctx>, connection::HandshakeError),
    ControlTlsHandshakeFailed(ControlConnection, connection::HandshakeError),
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum ControlConnection {
    Accept {
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
    },
    Connect(SocketAddr),
}

#[derive(Clone, Debug)]
pub struct TransportClose {
    /// Indicates that the transport was closed without error.
    // TODO include details.
    pub clean: bool,

    pub duration: Duration,

    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct StreamRequestFail {
    pub request_open_at: Instant,
    pub request_fail_at: Instant,
    pub error: h2::Reason,
}

#[derive(Clone, Debug)]
pub struct StreamRequestEnd {
    pub request_open_at: Instant,
    pub request_end_at: Instant,
}

#[derive(Clone, Debug)]
pub struct StreamResponseOpen {
    pub request_open_at: Instant,
    pub response_open_at: Instant,
}

#[derive(Clone, Debug)]
pub struct StreamResponseFail {
    pub request_open_at: Instant,
    pub response_open_at: Instant,
    pub response_first_frame_at: Option<Instant>,
    pub response_fail_at: Instant,
    pub error: h2::Reason,
    pub bytes_sent: u64,
    pub frames_sent: u32,
}

#[derive(Clone, Debug)]
pub struct StreamResponseEnd {
    pub request_open_at: Instant,
    pub response_open_at: Instant,
    pub response_first_frame_at: Instant,
    pub response_end_at: Instant,
    pub grpc_status: Option<u32>,
    pub bytes_sent: u64,
    pub frames_sent: u32,
}

// ===== impl Event =====

impl Event {
    pub fn is_http(&self) -> bool {
        match *self {
            Event::StreamRequestOpen(_) |
            Event::StreamRequestFail(_, _) |
            Event::StreamRequestEnd(_, _) |
            Event::StreamResponseOpen(_, _) |
            Event::StreamResponseFail(_, _) |
            Event::StreamResponseEnd(_, _) => true,
            _ => false,
        }
    }

    pub fn is_transport(&self) -> bool {
        match *self {
            Event::TransportOpen(_) | Event::TransportClose(_, _) => true,
            _ => false,
        }
    }

    pub fn proxy(&self) -> &Arc<ctx::Proxy> {
        match *self {
            Event::TransportOpen(ref ctx) |
            Event::TransportClose(ref ctx, _) |
            Event::TlsHandshakeFailed(ref ctx, _) => ctx.proxy(),
            Event::StreamRequestOpen(ref req) |
            Event::StreamRequestFail(ref req, _) |
            Event::StreamRequestEnd(ref req, _) => &req.server.proxy,
            Event::StreamResponseOpen(ref rsp, _) |
            Event::StreamResponseFail(ref rsp, _) |
            Event::StreamResponseEnd(ref rsp, _) => &rsp.request.server.proxy,
        }
    }
}
