//! The network boundary for every tool HighVi runs.
//!
//! yt-dlp and the discovery browser are started with this proxy as their only
//! route to the network. The proxy accepts only `CONNECT host:port` (HTTPS
//! tunnels), resolves the host itself, requires *every* resolved address to be
//! public, and connects to exactly the address it checked. Redirects, variant
//! playlists, segments, keys and iframes are each a new tunnel, so each is
//! checked, and DNS rebinding has no window between check and connect.
//! Plain-HTTP proxying is refused: HighVi fetches media over HTTPS only.

use crate::netpolicy::{is_public_ip, local_addresses};
use std::{
    io,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, lookup_host},
    sync::Semaphore,
    task::{JoinHandle, JoinSet},
    time::timeout,
};
use url::{Host, Url};

const MAX_REQUEST_HEAD_BYTES: usize = 8 * 1024;
const MAX_CONCURRENT_TUNNELS: usize = 64;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// What the proxy refused during one job. Only counts leave the proxy; no URL
/// or host name is stored, because those can carry tokens.
#[derive(Debug, Default)]
pub struct EgressStats {
    blocked_address: AtomicU64,
    blocked_plain_http: AtomicU64,
    unresolvable: AtomicU64,
    tunnels: AtomicU64,
}

/// A point-in-time copy of the refusal counters, so a caller can tell which
/// refusals happened during its own tool run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RefusalCounts {
    pub address: u64,
    pub plain_http: u64,
    pub unresolvable: u64,
}

impl RefusalCounts {
    pub fn since(self, earlier: Self) -> Self {
        Self {
            address: self.address.saturating_sub(earlier.address),
            plain_http: self.plain_http.saturating_sub(earlier.plain_http),
            unresolvable: self.unresolvable.saturating_sub(earlier.unresolvable),
        }
    }
}

impl EgressStats {
    pub fn counts(&self) -> RefusalCounts {
        RefusalCounts {
            address: self.blocked_address(),
            plain_http: self.blocked_plain_http(),
            unresolvable: self.unresolvable(),
        }
    }

    /// Requests refused because the destination is local, private or reserved.
    pub fn blocked_address(&self) -> u64 {
        self.blocked_address.load(Ordering::Relaxed)
    }

    /// Requests refused because they were plain HTTP.
    pub fn blocked_plain_http(&self) -> u64 {
        self.blocked_plain_http.load(Ordering::Relaxed)
    }

    /// Requests whose host name did not resolve.
    pub fn unresolvable(&self) -> u64 {
        self.unresolvable.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub fn tunnels(&self) -> u64 {
        self.tunnels.load(Ordering::Relaxed)
    }
}

/// How the proxy turns a host into addresses.
#[derive(Clone, Debug)]
pub(crate) enum Resolver {
    System,
    /// Test fixtures: fixed answers, and no real DNS. Production code cannot
    /// construct this variant.
    #[cfg(test)]
    Fixture(Arc<std::collections::HashMap<String, FixtureHost>>),
}

#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct FixtureHost {
    pub addresses: Vec<SocketAddr>,
    /// A trusted fixture may resolve to loopback (the local test server). An
    /// untrusted one goes through the public-address policy, which lets tests
    /// model DNS answers that point at private networks.
    pub trusted: bool,
}

/// A running proxy. Dropping it stops the listener and every open tunnel.
pub struct EgressProxy {
    address: SocketAddr,
    stats: Arc<EgressStats>,
    accept_task: JoinHandle<()>,
}

impl EgressProxy {
    pub(crate) async fn start_with(resolver: Resolver) -> io::Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let stats = Arc::new(EgressStats::default());
        let accept_task = tokio::spawn(accept_loop(listener, resolver, Arc::clone(&stats)));
        Ok(Self {
            address,
            stats,
            accept_task,
        })
    }

    /// Proxy URL for `yt-dlp --proxy` and `chrome --proxy-server`.
    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn stats(&self) -> &EgressStats {
        &self.stats
    }
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        // The accept task owns the JoinSet of tunnels, so aborting it aborts
        // every tunnel too.
        self.accept_task.abort();
    }
}

async fn accept_loop(listener: TcpListener, resolver: Resolver, stats: Arc<EgressStats>) {
    let limit = Arc::new(Semaphore::new(MAX_CONCURRENT_TUNNELS));
    let mut tunnels = JoinSet::new();
    loop {
        while tunnels.try_join_next().is_some() {}
        let Ok(permit) = Arc::clone(&limit).acquire_owned().await else {
            return;
        };
        let Ok((client, _)) = listener.accept().await else {
            // e.g. out of descriptors: back off instead of spinning.
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let resolver = resolver.clone();
        let stats = Arc::clone(&stats);
        tunnels.spawn(async move {
            let _permit = permit;
            let _ = handle_client(client, &resolver, &stats).await;
        });
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Refusal {
    BadRequest,
    PlainHttp,
    Address,
    Unresolvable,
    Unreachable,
}

impl Refusal {
    fn status_line(&self) -> &'static str {
        match self {
            Self::BadRequest => "400 Bad Request",
            Self::PlainHttp | Self::Address => "403 Forbidden",
            Self::Unresolvable | Self::Unreachable => "502 Bad Gateway",
        }
    }
}

async fn handle_client(
    mut client: TcpStream,
    resolver: &Resolver,
    stats: &EgressStats,
) -> io::Result<()> {
    let Ok(head) = timeout(HEAD_TIMEOUT, read_request_head(&mut client)).await else {
        return Ok(());
    };
    let (head, leftover) = head?;

    let upstream = match parse_connect(&head) {
        Err(refusal) => Err(refusal),
        Ok((host, port)) => connect_checked(&host, port, resolver).await,
    };
    let mut upstream = match upstream {
        Ok(upstream) => upstream,
        Err(refusal) => {
            match refusal {
                Refusal::Address => stats.blocked_address.fetch_add(1, Ordering::Relaxed),
                Refusal::PlainHttp => stats.blocked_plain_http.fetch_add(1, Ordering::Relaxed),
                Refusal::Unresolvable => stats.unresolvable.fetch_add(1, Ordering::Relaxed),
                _ => 0,
            };
            let response = format!(
                "HTTP/1.1 {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                refusal.status_line()
            );
            let _ = client.write_all(response.as_bytes()).await;
            return Ok(());
        }
    };

    stats.tunnels.fetch_add(1, Ordering::Relaxed);
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    if !leftover.is_empty() {
        upstream.write_all(&leftover).await?;
    }
    let (client_read, client_write) = client.into_split();
    let (upstream_read, upstream_write) = upstream.into_split();
    let _ = tokio::join!(
        pump(client_read, upstream_write),
        pump(upstream_read, client_write)
    );
    Ok(())
}

/// Reads up to the end of the request head. Returns the head and any bytes
/// the client sent after it.
async fn read_request_head(client: &mut TcpStream) -> io::Result<(String, Vec<u8>)> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        let read = client.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let leftover = buffer.split_off(end + 4);
            let head = String::from_utf8(buffer).map_err(|_| io::ErrorKind::InvalidData)?;
            return Ok((head, leftover));
        }
        if buffer.len() > MAX_REQUEST_HEAD_BYTES {
            return Err(io::ErrorKind::InvalidData.into());
        }
    }
}

/// Parses `CONNECT authority HTTP/1.x` into a normalised host and port.
fn parse_connect(head: &str) -> Result<(Host<String>, u16), Refusal> {
    let request_line = head.lines().next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let (Some(method), Some(authority), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Refusal::BadRequest);
    };
    if !version.starts_with("HTTP/1.") {
        return Err(Refusal::BadRequest);
    }
    if method != "CONNECT" {
        // Absolute-form requests (`GET http://…`) are plain-HTTP proxying.
        return Err(if authority.starts_with("http://") {
            Refusal::PlainHttp
        } else {
            Refusal::BadRequest
        });
    }
    if authority.is_empty() || authority.contains(['/', '\\', '?', '#', '@']) {
        return Err(Refusal::BadRequest);
    }
    // The WHATWG parser normalises every host form (IDNA, decimal/hex/octal
    // IPv4, bracketed IPv6), so the address checked is the address dialled.
    let url = Url::parse(&format!("https://{authority}/")).map_err(|_| Refusal::BadRequest)?;
    let host = url.host().ok_or(Refusal::BadRequest)?.to_owned();
    let port = url.port_or_known_default().ok_or(Refusal::BadRequest)?;
    if port == 0 {
        return Err(Refusal::BadRequest);
    }
    Ok((host, port))
}

async fn connect_checked(
    host: &Host<String>,
    port: u16,
    resolver: &Resolver,
) -> Result<TcpStream, Refusal> {
    let addresses = resolve_checked(host, port, resolver).await?;
    for address in addresses {
        if let Ok(Ok(stream)) = timeout(CONNECT_TIMEOUT, TcpStream::connect(address)).await {
            let _ = stream.set_nodelay(true);
            return Ok(stream);
        }
    }
    Err(Refusal::Unreachable)
}

async fn resolve_checked(
    host: &Host<String>,
    port: u16,
    resolver: &Resolver,
) -> Result<Vec<SocketAddr>, Refusal> {
    let addresses: Vec<SocketAddr> = match (host, resolver) {
        (Host::Ipv4(address), _) => vec![SocketAddr::new((*address).into(), port)],
        (Host::Ipv6(address), _) => vec![SocketAddr::new((*address).into(), port)],
        (Host::Domain(domain), Resolver::System) => {
            match timeout(RESOLVE_TIMEOUT, lookup_host((domain.as_str(), port))).await {
                Ok(Ok(addresses)) => addresses.collect(),
                _ => return Err(Refusal::Unresolvable),
            }
        }
        #[cfg(test)]
        (Host::Domain(domain), Resolver::Fixture(hosts)) => match hosts.get(domain.as_str()) {
            Some(fixture) if fixture.trusted => return Ok(fixture.addresses.clone()),
            Some(fixture) => fixture.addresses.clone(),
            None => return Err(Refusal::Unresolvable),
        },
    };
    if addresses.is_empty() {
        return Err(Refusal::Unresolvable);
    }
    // One private answer taints the whole name: a client must not be able to
    // pick which of a mixed answer set it reaches.
    let local = local_addresses().ok_or(Refusal::Address)?;
    if addresses
        .iter()
        .any(|address| !allowed(address.ip(), &local))
    {
        return Err(Refusal::Address);
    }
    Ok(addresses)
}

/// Public, and not one of this machine's own addresses. Canonical forms are
/// compared, so `::ffff:a.b.c.d` counts as the IPv4 host it maps to.
fn allowed(ip: std::net::IpAddr, local: &[std::net::IpAddr]) -> bool {
    let ip = ip.to_canonical();
    is_public_ip(ip) && !local.iter().any(|own| own.to_canonical() == ip)
}

/// Copies one direction of a tunnel until EOF, an error, or `IDLE_TIMEOUT`
/// without data.
async fn pump<R, W>(mut from: R, mut to: W) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0_u8; 32 * 1024];
    loop {
        let read = timeout(IDLE_TIMEOUT, from.read(&mut buffer))
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
        if read == 0 {
            let _ = to.shutdown().await;
            return Ok(());
        }
        timeout(IDLE_TIMEOUT, to.write_all(&buffer[..read]))
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn host(raw: &str) -> Host<String> {
        Host::parse(raw).unwrap()
    }

    #[test]
    fn only_connect_with_a_bare_authority_is_accepted() {
        assert_eq!(
            parse_connect("CONNECT cdn.example:443 HTTP/1.1\r\nHost: x\r\n\r\n").unwrap(),
            (host("cdn.example"), 443)
        );
        assert_eq!(
            parse_connect("CONNECT [2606:4700::1]:8443 HTTP/1.1\r\n\r\n").unwrap(),
            (Host::<String>::Ipv6("2606:4700::1".parse().unwrap()), 8443)
        );
        assert_eq!(
            parse_connect("GET http://cdn.example/a.ts HTTP/1.1\r\n\r\n").unwrap_err(),
            Refusal::PlainHttp
        );
        for bad in [
            "GET / HTTP/1.1\r\n\r\n",
            "CONNECT user@cdn.example:443 HTTP/1.1\r\n\r\n",
            "CONNECT cdn.example:443/x HTTP/1.1\r\n\r\n",
            "CONNECT cdn.example:0 HTTP/1.1\r\n\r\n",
            "CONNECT cdn.example:99999 HTTP/1.1\r\n\r\n",
            "CONNECT  HTTP/1.1\r\n\r\n",
            "CONNECT cdn.example:443 HTTP/2\r\n\r\n",
            "CONNECT cdn.example:443 HTTP/1.1 extra\r\n\r\n",
            "connect cdn.example:443 HTTP/1.1\r\n\r\n",
        ] {
            assert_eq!(
                parse_connect(bad).unwrap_err(),
                Refusal::BadRequest,
                "{bad}"
            );
        }
    }

    #[test]
    fn obfuscated_ipv4_forms_normalise_to_the_dialled_address() {
        for raw in ["2130706433", "0x7f.1", "0177.0.0.1", "127.1"] {
            let (parsed, _) =
                parse_connect(&format!("CONNECT {raw}:443 HTTP/1.1\r\n\r\n")).unwrap();
            assert_eq!(
                parsed,
                Host::<String>::Ipv4("127.0.0.1".parse().unwrap()),
                "{raw}"
            );
        }
    }

    #[test]
    fn this_hosts_own_public_addresses_are_refused_in_every_form() {
        let local = ["8.8.8.8".parse().unwrap(), "2606:4700::1".parse().unwrap()];
        for own in ["8.8.8.8", "::ffff:8.8.8.8", "2606:4700::1"] {
            assert!(!allowed(own.parse().unwrap(), &local), "{own}");
        }
        assert!(allowed("1.1.1.1".parse().unwrap(), &local));
    }

    #[tokio::test]
    async fn literal_and_resolved_private_destinations_are_refused() {
        let resolver = Resolver::Fixture(Arc::new(HashMap::from([
            (
                "rebind.test".to_string(),
                FixtureHost {
                    addresses: vec!["127.0.0.1:443".parse().unwrap()],
                    trusted: false,
                },
            ),
            (
                "mixed.test".to_string(),
                FixtureHost {
                    addresses: vec![
                        "93.184.215.14:443".parse().unwrap(),
                        "10.0.0.1:443".parse().unwrap(),
                    ],
                    trusted: false,
                },
            ),
        ])));
        for target in [
            "127.0.0.1",
            "[::1]",
            "[::ffff:7f00:1]",
            "169.254.169.254",
            "rebind.test",
            "mixed.test",
        ] {
            assert_eq!(
                resolve_checked(&host_of(target), 443, &resolver)
                    .await
                    .unwrap_err(),
                Refusal::Address,
                "{target}"
            );
        }
    }

    fn host_of(authority: &str) -> Host<String> {
        parse_connect(&format!("CONNECT {authority}:443 HTTP/1.1\r\n\r\n"))
            .unwrap()
            .0
    }

    async fn send(proxy: &EgressProxy, request: &str) -> String {
        let mut stream = TcpStream::connect(proxy.address).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        let mut chunk = [0_u8; 256];
        while !response.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            response.extend_from_slice(&chunk[..read]);
        }
        String::from_utf8(response).unwrap()
    }

    #[tokio::test]
    async fn proxy_tunnels_to_trusted_fixtures_and_counts_refusals() {
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_address = echo.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = echo.accept().await.unwrap();
            let mut buffer = [0_u8; 5];
            socket.read_exact(&mut buffer).await.unwrap();
            socket.write_all(&buffer).await.unwrap();
        });
        let resolver = Resolver::Fixture(Arc::new(HashMap::from([(
            "media.test".to_string(),
            FixtureHost {
                addresses: vec![echo_address],
                trusted: true,
            },
        )])));
        let proxy = EgressProxy::start_with(resolver).await.unwrap();

        let mut stream = TcpStream::connect(proxy.address).await.unwrap();
        stream
            .write_all(b"CONNECT media.test:443 HTTP/1.1\r\n\r\nhello")
            .await
            .unwrap();
        let mut response = vec![0_u8; 39 + 5];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(
            &response,
            b"HTTP/1.1 200 Connection Established\r\n\r\nhello"
        );

        // The fixture server itself, addressed directly, is loopback.
        let direct = send(&proxy, &format!("CONNECT {echo_address} HTTP/1.1\r\n\r\n")).await;
        assert!(direct.starts_with("HTTP/1.1 403"), "{direct}");
        let plain = send(&proxy, "GET http://media.test/a.ts HTTP/1.1\r\n\r\n").await;
        assert!(plain.starts_with("HTTP/1.1 403"), "{plain}");
        let unknown = send(&proxy, "CONNECT nowhere.test:443 HTTP/1.1\r\n\r\n").await;
        assert!(unknown.starts_with("HTTP/1.1 502"), "{unknown}");

        assert_eq!(proxy.stats().tunnels(), 1);
        assert_eq!(proxy.stats().blocked_address(), 1);
        assert_eq!(proxy.stats().blocked_plain_http(), 1);
        assert_eq!(proxy.stats().unresolvable(), 1);
    }

    #[tokio::test]
    async fn oversized_request_head_is_dropped() {
        let proxy = EgressProxy::start_with(Resolver::Fixture(Arc::default()))
            .await
            .unwrap();
        let mut stream = TcpStream::connect(proxy.address).await.unwrap();
        let _ = stream
            .write_all(
                format!(
                    "CONNECT a:443 HTTP/1.1\r\nX: {}\r\n\r\n",
                    "a".repeat(20_000)
                )
                .as_bytes(),
            )
            .await;
        let mut buffer = Vec::new();
        let _ = stream.read_to_end(&mut buffer).await;
        assert!(buffer.is_empty());
        assert_eq!(proxy.stats().tunnels(), 0);
    }
}
