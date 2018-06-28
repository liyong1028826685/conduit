use futures::Future;
use tokio_connect;

use std::{fmt, io};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

use http;

use connection;
use convert::TryFrom;
use dns;
use telemetry::sensor;
use transport::tls;

#[derive(Debug, Clone)]
pub struct Connect {
    addr: SocketAddr,
    tls: tls::ConditionalConnectionConfig<tls::ClientConfig>,
    tls_sensor: Option<sensor::tls::Connect>,
}

#[derive(Clone, Debug)]
pub struct HostAndPort {
    pub host: Host,
    pub port: u16,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DnsNameAndPort {
    pub host: dns::Name,
    pub port: u16,
}


#[derive(Clone, Debug)]
pub enum Host {
    DnsName(dns::Name),
    Ip(IpAddr),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HostAndPortError {
    /// The host is not a valid DNS name or IP address.
    InvalidHost,

    /// The port is missing.
    MissingPort,
}

#[derive(Debug, Clone)]
pub struct LookupAddressAndConnect {
    host_and_port: HostAndPort,
    dns_resolver: dns::Resolver,
    tls: tls::ConditionalConnectionConfig<tls::ClientConfig>,
}

// ===== impl HostAndPort =====

impl HostAndPort {
    pub fn normalize(a: &http::uri::Authority, default_port: Option<u16>)
        -> Result<Self, HostAndPortError>
    {
        let host = IpAddr::from_str(a.host())
            .map(Host::Ip)
            .or_else(|_|
                dns::Name::try_from(a.host().as_bytes())
                    .map(Host::DnsName)
                    .map_err(|_| HostAndPortError::InvalidHost))?;
        let port = a.port()
            .or(default_port)
            .ok_or_else(|| HostAndPortError::MissingPort)?;
        Ok(HostAndPort {
            host,
            port
        })
    }
}

impl<'a> From<&'a HostAndPort> for http::uri::Authority {
    fn from(a: &HostAndPort) -> Self {
        let s = match a.host {
            Host::DnsName(ref n) => format!("{}:{}", n, a.port),
            Host::Ip(ref ip) => format!("{}:{}", ip, a.port),
        };
        http::uri::Authority::from_str(&s).unwrap()
    }
}

impl fmt::Display for HostAndPort {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.host {
            Host::DnsName(ref dns) => {
                write!(f, "{}:{}", dns, self.port)
            }
            Host::Ip(ref ip) => {
                write!(f, "{}:{}", ip, self.port)
            }
        }
    }
}

// ===== impl Connect =====

impl Connect {
    /// Returns a `Connect` to `addr`.
    pub fn new(
        addr: SocketAddr,
        tls: tls::ConditionalConnectionConfig<tls::ClientConfig>,
    ) -> Self {
        Self {
            addr,
            tls,
            tls_sensor: None,
        }
    }

    pub fn with_tls_sensor(
        self,
        sensor: sensor::tls::Connect,
    ) -> Self {
        Connect {
            tls_sensor: Some(sensor),
            ..self
        }
    }
}

impl tokio_connect::Connect for Connect {
    type Connected = connection::Connection;
    type Error = io::Error;
    type Future = connection::Connecting;

    fn connect(&self) -> Self::Future {
        connection::connect(
            &self.addr,
            self.tls.clone(),
            self.tls_sensor.clone(),
        )
    }
}

// ===== impl LookupAddressAndConnect =====

impl LookupAddressAndConnect {
    pub fn new(
        host_and_port: HostAndPort,
        dns_resolver: dns::Resolver,
        tls: tls::ConditionalConnectionConfig<tls::ClientConfig>,
    ) -> Self {
        Self {
            host_and_port,
            dns_resolver,
            tls,
        }
    }
}

impl tokio_connect::Connect for LookupAddressAndConnect {
    type Connected = connection::Connection;
    type Error = io::Error;
    type Future = Box<Future<Item = connection::Connection, Error = io::Error> + Send>;

    fn connect(&self) -> Self::Future {
        let port = self.host_and_port.port;
        let host = self.host_and_port.host.clone();
        let tls = self.tls.clone();
        let c = self.dns_resolver
            .resolve_one_ip(&self.host_and_port.host)
            .map_err(|_| {
                io::Error::new(io::ErrorKind::NotFound, "DNS resolution failed")
            })
            .and_then(move |ip_addr: IpAddr| {
                info!("DNS resolved {:?} to {}", host, ip_addr);
                let addr = SocketAddr::from((ip_addr, port));
                trace!("connect {}", addr);
                connection::connect(
                    &addr,
                    tls,
                    None, // no TLS sensor since we don't have a context.
                )
            });
        Box::new(c)
    }
}
