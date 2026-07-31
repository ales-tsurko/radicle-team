//! Managed network proxy routing for sandboxed agents.

use std::{
    collections::HashMap,
    convert::Infallible,
    io,
    marker::PhantomData,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use axum::body::Body;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use hyper::{
    Method, Request, Response, StatusCode, Uri, body::Incoming, client::conn::http1 as client,
    header::HOST, server::conn::http1 as server, service::service_fn,
};
use hyper_util::rt::TokioIo;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;
use zerobox_network_proxy::{
    ConfigReloader, ConfigState, NO_PROXY_ENV_KEYS, NetworkProxy, NetworkProxyConfig,
    NetworkProxyConstraints, NetworkProxyState, build_config_state, normalize_host,
};

use crate::sandbox::{command::Error, policy::NetworkPolicy};

const DNS_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct ManagedNetwork {
    pub(super) proxy: NetworkProxy,
    runtime: ProxyRuntime,
}

impl ManagedNetwork {
    pub(super) async fn start(policy: NetworkPolicy) -> Result<Option<Self>, Error> {
        if !policy.is_restricted() {
            return Ok(None);
        }
        let route = Arc::new(Route::new(&policy)?);
        let http = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(anyhow::Error::from)?;
        let socks = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(anyhow::Error::from)?;
        let http_address = http.local_addr().map_err(anyhow::Error::from)?;
        let socks_address = socks.local_addr().map_err(anyhow::Error::from)?;
        let proxy = Self::metadata(http_address, socks_address).await?;
        let runtime = ProxyRuntime::start(http, socks, route);

        Ok(Some(Self { proxy, runtime }))
    }

    pub(super) fn apply_to_environment(&self, environment: &mut HashMap<String, String>) {
        self.proxy.apply_to_env(environment);
        for key in NO_PROXY_ENV_KEYS {
            let _previous = environment.insert((*key).to_owned(), String::new());
        }
    }

    pub(super) fn into_parts(self) -> (NetworkProxy, ProxyRuntime) {
        (self.proxy, self.runtime)
    }

    async fn metadata(http: SocketAddr, socks: SocketAddr) -> Result<NetworkProxy, Error> {
        let mut config = NetworkProxyConfig::default();
        config.network.enabled = true;
        config.network.allow_local_binding = true;
        config.network.allow_upstream_proxy = false;
        let state = build_config_state(config, NetworkProxyConstraints::default())?;
        let state = Arc::new(NetworkProxyState::with_reloader(
            state,
            Arc::new(StaticReloader),
        ));

        NetworkProxy::builder()
            .state(state)
            .http_addr(http)
            .socks_addr(socks)
            .managed_by_codex(false)
            .build()
            .await
            .map_err(Error::from)
    }
}

pub(super) struct ProxyRuntime {
    cancellation: CancellationToken,
    http: JoinHandle<()>,
    socks: JoinHandle<()>,
}

impl ProxyRuntime {
    fn start(http: TcpListener, socks: TcpListener, route: Arc<Route>) -> Self {
        let cancellation = CancellationToken::new();
        let http_task = tokio::spawn(
            ProxyListener::<HttpProxy>::new(http, route.clone()).run(cancellation.child_token()),
        );
        let socks_task = tokio::spawn(
            ProxyListener::<SocksProxy>::new(socks, route).run(cancellation.child_token()),
        );

        Self {
            cancellation,
            http: http_task,
            socks: socks_task,
        }
    }
}

impl Drop for ProxyRuntime {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.http.abort();
        self.socks.abort();
    }
}

struct ProxyListener<Protocol> {
    listener: TcpListener,
    route: Arc<Route>,
    protocol: PhantomData<Protocol>,
}

impl<Protocol> ProxyListener<Protocol>
where
    Protocol: ProxyProtocol,
{
    const fn new(listener: TcpListener, route: Arc<Route>) -> Self {
        Self {
            listener,
            route,
            protocol: PhantomData,
        }
    }

    async fn run(self, cancellation: CancellationToken) {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                () = cancellation.cancelled() => break,
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, _peer)) => {
                        connections.spawn(Protocol::serve(stream, self.route.clone()));
                    }
                    Err(_) => break,
                },
                Some(_result) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
        connections.abort_all();
    }
}

#[async_trait::async_trait]
trait ProxyProtocol: Send + Sync + 'static {
    async fn serve(stream: TcpStream, route: Arc<Route>);
}

struct HttpProxy;

impl HttpProxy {
    async fn serve(stream: TcpStream, route: Arc<Route>) {
        let service = service_fn(move |request| {
            let route = route.clone();
            async move { Ok::<_, Infallible>(Self::forward(request, route).await) }
        });
        let _result = server::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades()
            .await;
    }

    async fn forward(mut request: Request<Incoming>, route: Arc<Route>) -> Response<Body> {
        let Some(target) = Target::from_request(&request) else {
            return Self::error(StatusCode::BAD_REQUEST);
        };
        let Ok(stream) = route.connect(&target.host, target.port).await else {
            return Self::error(StatusCode::FORBIDDEN);
        };
        if request.method() == Method::CONNECT {
            return Self::tunnel(&mut request, stream);
        }
        Self::send(request, stream).await
    }

    fn tunnel(request: &mut Request<Incoming>, mut stream: TcpStream) -> Response<Body> {
        let upgrade = hyper::upgrade::on(request);
        tokio::spawn(async move {
            if let Ok(client) = upgrade.await {
                let _result =
                    tokio::io::copy_bidirectional(&mut TokioIo::new(client), &mut stream).await;
            }
        });

        Response::new(Body::empty())
    }

    async fn send(mut request: Request<Incoming>, stream: TcpStream) -> Response<Body> {
        let path = request
            .uri()
            .path_and_query()
            .map_or("/", hyper::http::uri::PathAndQuery::as_str);
        let Ok(uri) = path.parse::<Uri>() else {
            return Self::error(StatusCode::BAD_REQUEST);
        };
        *request.uri_mut() = uri;
        request.headers_mut().remove("proxy-connection");
        let Ok((mut sender, connection)) = client::handshake(TokioIo::new(stream)).await else {
            return Self::error(StatusCode::BAD_GATEWAY);
        };
        tokio::spawn(async move {
            let _result = connection.with_upgrades().await;
        });

        match sender.send_request(request).await {
            Ok(response) => response.map(Body::new),
            Err(_) => Self::error(StatusCode::BAD_GATEWAY),
        }
    }

    fn error(status: StatusCode) -> Response<Body> {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = status;
        response
    }
}

#[async_trait::async_trait]
impl ProxyProtocol for HttpProxy {
    async fn serve(stream: TcpStream, route: Arc<Route>) {
        Self::serve(stream, route).await;
    }
}

struct SocksProxy;

impl SocksProxy {
    async fn serve(mut client: TcpStream, route: Arc<Route>) {
        if Self::negotiate(&mut client).await.is_err() {
            return;
        }
        let Ok(target) = Self::target(&mut client).await else {
            let _result = Self::reply(&mut client, 1).await;
            return;
        };
        let Ok(mut upstream) = route.connect(&target.host, target.port).await else {
            let _result = Self::reply(&mut client, 2).await;
            return;
        };
        if Self::reply(&mut client, 0).await.is_ok() {
            let _result = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        }
    }

    async fn negotiate(stream: &mut TcpStream) -> io::Result<()> {
        let methods = Self::methods(stream).await?;
        let method = Self::method(&methods);
        stream.write_all(&[5, method]).await?;
        Self::accepted_method(method)
    }

    async fn methods(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
        let mut header = [0_u8; 2];
        stream.read_exact(&mut header).await?;
        if header[0] != 5 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported SOCKS version",
            ));
        }
        let mut methods = vec![0_u8; usize::from(header[1])];
        stream.read_exact(&mut methods).await?;

        Ok(methods)
    }

    fn method(methods: &[u8]) -> u8 {
        if methods.contains(&0) { 0 } else { 0xff }
    }

    fn accepted_method(method: u8) -> io::Result<()> {
        if method == 0 {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SOCKS authentication required",
            ))
        }
    }

    async fn target(stream: &mut TcpStream) -> io::Result<Target> {
        let kind = Self::request(stream).await?;
        let host = Self::host(stream, kind).await?;
        let port = Self::port(stream).await?;

        Ok(Target { host, port })
    }

    async fn request(stream: &mut TcpStream) -> io::Result<SocksAddressKind> {
        let mut header = [0_u8; 4];
        stream.read_exact(&mut header).await?;
        let [version, command, reserved, kind] = header;
        if [version, command, reserved] != [5, 1, 0] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported SOCKS request",
            ));
        }

        SocksAddressKind::try_from(kind)
    }

    async fn host(stream: &mut TcpStream, kind: SocksAddressKind) -> io::Result<String> {
        match kind {
            SocksAddressKind::Ipv4 => Self::ipv4(stream).await,
            SocksAddressKind::Domain => Self::domain(stream).await,
            SocksAddressKind::Ipv6 => Self::ipv6(stream).await,
        }
    }

    async fn port(stream: &mut TcpStream) -> io::Result<u16> {
        let mut port = [0_u8; 2];
        stream.read_exact(&mut port).await?;
        Ok(u16::from_be_bytes(port))
    }

    async fn ipv4(stream: &mut TcpStream) -> io::Result<String> {
        let mut address = [0_u8; 4];
        stream.read_exact(&mut address).await?;
        Ok(Ipv4Addr::from(address).to_string())
    }

    async fn ipv6(stream: &mut TcpStream) -> io::Result<String> {
        let mut address = [0_u8; 16];
        stream.read_exact(&mut address).await?;
        Ok(Ipv6Addr::from(address).to_string())
    }

    async fn domain(stream: &mut TcpStream) -> io::Result<String> {
        let length = stream.read_u8().await?;
        let mut host = vec![0_u8; usize::from(length)];
        stream.read_exact(&mut host).await?;
        String::from_utf8(host).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    async fn reply(stream: &mut TcpStream, status: u8) -> io::Result<()> {
        stream.write_all(&[5, status, 0, 1, 0, 0, 0, 0, 0, 0]).await
    }
}

enum SocksAddressKind {
    Ipv4,
    Domain,
    Ipv6,
}

impl TryFrom<u8> for SocksAddressKind {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Ipv4),
            3 => Ok(Self::Domain),
            4 => Ok(Self::Ipv6),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported SOCKS address",
            )),
        }
    }
}

#[async_trait::async_trait]
impl ProxyProtocol for SocksProxy {
    async fn serve(stream: TcpStream, route: Arc<Route>) {
        Self::serve(stream, route).await;
    }
}

struct Target {
    host: String,
    port: u16,
}

impl Target {
    fn from_request(request: &Request<Incoming>) -> Option<Self> {
        let authority = request.uri().authority().cloned().or_else(|| {
            request
                .headers()
                .get(HOST)
                .and_then(|host| host.to_str().ok())
                .and_then(|host| host.parse().ok())
        })?;
        let port = authority.port_u16().unwrap_or_else(|| {
            if request.uri().scheme_str() == Some("https") {
                443
            } else {
                80
            }
        });

        Some(Self {
            host: normalize_host(authority.host()),
            port,
        })
    }
}

struct Route {
    domains: GlobSet,
    mcp_host: String,
    mcp_port: u16,
}

impl Route {
    fn new(policy: &NetworkPolicy) -> Result<Self, Error> {
        let mut builder = GlobSetBuilder::new();
        for pattern in policy.domains() {
            for pattern in Self::expand_pattern(pattern) {
                builder.add(
                    GlobBuilder::new(&pattern)
                        .case_insensitive(true)
                        .build()
                        .map_err(anyhow::Error::from)?,
                );
            }
        }
        let (mcp_host, mcp_port) = policy.mcp_address();

        Ok(Self {
            domains: builder.build().map_err(anyhow::Error::from)?,
            mcp_host: normalize_host(mcp_host),
            mcp_port,
        })
    }

    async fn connect(&self, host: &str, port: u16) -> io::Result<TcpStream> {
        let addresses = self.resolve(host, port).await?;
        let mut error = None;
        for address in addresses {
            match TcpStream::connect(address).await {
                Ok(stream) => return Ok(stream),
                Err(source) => error = Some(source),
            }
        }

        Err(error.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::AddrNotAvailable, "target did not resolve")
        }))
    }

    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        let host = normalize_host(host);
        if host == self.mcp_host && port == self.mcp_port {
            return self.mcp_address();
        }
        if !self.domains.is_match(&host) {
            return Err(Self::denied());
        }
        let lookup = tokio::net::lookup_host((host.as_str(), port));
        let addresses = tokio::time::timeout(DNS_TIMEOUT, lookup)
            .await
            .map_err(|_elapsed| Self::denied())?
            .map_err(|_source| Self::denied())?
            .collect::<Vec<_>>();
        if addresses.is_empty()
            || addresses
                .iter()
                .any(|address| !Self::is_global(address.ip()))
        {
            return Err(Self::denied());
        }

        Ok(addresses)
    }

    fn mcp_address(&self) -> io::Result<Vec<SocketAddr>> {
        self.mcp_host
            .parse::<IpAddr>()
            .map(|host| vec![SocketAddr::new(host, self.mcp_port)])
            .map_err(|_address| Self::denied())
    }

    fn expand_pattern(pattern: &str) -> Vec<String> {
        let pattern = pattern.trim();
        if pattern == "*" {
            return vec![pattern.to_owned()];
        }
        if let Some(domain) = pattern.strip_prefix("**.") {
            let domain = normalize_host(domain);
            return vec![domain.clone(), format!("?*.{domain}")];
        }
        if let Some(domain) = pattern.strip_prefix("*.") {
            return vec![format!("?*.{}", normalize_host(domain))];
        }

        vec![normalize_host(pattern)]
    }

    fn is_global(ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(ip) => Self::is_global_v4(ip),
            IpAddr::V6(ip) => Self::is_global_v6(ip),
        }
    }

    fn is_global_v4(ip: Ipv4Addr) -> bool {
        let [a, b, c, d] = ip.octets();
        let shared = a == 100 && b & 0xc0 == 0x40;
        let protocol = a == 192 && b == 0 && c == 0 && d != 9 && d != 10;
        let documentation =
            (a, b, c) == (192, 0, 2) || (a, b, c) == (198, 51, 100) || (a, b, c) == (203, 0, 113);
        let benchmarking = a == 198 && b & 0xfe == 18;
        let reserved = a & 0xf0 == 0xf0;

        a != 0
            && !ip.is_private()
            && !shared
            && !ip.is_loopback()
            && !ip.is_link_local()
            && !protocol
            && !documentation
            && !benchmarking
            && !reserved
            && !ip.is_multicast()
            && !ip.is_broadcast()
    }

    fn is_global_v6(ip: Ipv6Addr) -> bool {
        let segments = ip.segments();
        matches!(segments[0], 0x2000..=0x3fff)
            && !ip.is_unspecified()
            && !ip.is_loopback()
            && !ip.is_multicast()
            && !matches!(segments, [0, 0, 0, 0, 0, 0xffff, _, _])
            && !matches!(segments, [0x64, 0xff9b, 1, _, _, _, _, _])
            && !matches!(segments, [0x100, 0, 0, 0, _, _, _, _])
            && !Self::is_special_ietf_v6(segments, ip)
            && !matches!(segments, [0x2002, _, _, _, _, _, _, _])
            && !matches!(segments, [0x2001, 0xdb8, ..] | [0x3fff, 0..=0x0fff, ..])
            && !matches!(segments, [0x5f00, ..])
            && !ip.is_unique_local()
            && !ip.is_unicast_link_local()
    }

    fn is_special_ietf_v6(segments: [u16; 8], ip: Ipv6Addr) -> bool {
        if !matches!(segments, [0x2001, value, _, _, _, _, _, _] if value < 0x200) {
            return false;
        }
        let address = u128::from_be_bytes(ip.octets());
        let anycast = matches!(
            address,
            0x2001_0001_0000_0000_0000_0000_0000_0001 | 0x2001_0001_0000_0000_0000_0000_0000_0002
        );
        let assigned = matches!(segments, [0x2001, 3, _, _, _, _, _, _])
            || matches!(segments, [0x2001, 4, 0x112, _, _, _, _, _])
            || matches!(segments, [0x2001, 0x20..=0x3f, _, _, _, _, _, _]);

        !anycast && !assigned
    }

    fn denied() -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "network target is not allowlisted",
        )
    }
}

struct StaticReloader;

#[async_trait::async_trait]
impl ConfigReloader for StaticReloader {
    fn source_label(&self) -> String {
        "radicle-team sandbox policy".to_owned()
    }

    async fn maybe_reload(&self) -> anyhow::Result<Option<ConfigState>> {
        Ok(None)
    }

    async fn reload_now(&self) -> anyhow::Result<ConfigState> {
        Err(anyhow::anyhow!(
            "immutable sandbox policy cannot be reloaded"
        ))
    }
}

impl std::fmt::Debug for ManagedNetwork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedNetwork")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ProxyRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProxyRuntime")
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, not(miri), any(target_os = "macos", target_os = "linux")))]
mod tests {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use crate::sandbox::{
        NetworkAccess,
        network::{ManagedNetwork, Route},
        policy::NetworkPolicy,
    };

    #[tokio::test]
    async fn restricted_proxy_reaches_only_the_mcp_port_on_loopback() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("MCP listener should bind");
        let address = listener
            .local_addr()
            .expect("MCP address should be available");
        let server = tokio::spawn(async move {
            let (mut stream, _peer) = listener.accept().await.expect("proxy should connect");
            let mut request = [0_u8; 1024];
            let _read = stream
                .read(&mut request)
                .await
                .expect("request should read");
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("response should write");
        });
        let policy = NetworkPolicy::new(
            NetworkAccess::Disabled,
            std::net::Ipv4Addr::LOCALHOST.to_string(),
            address.port(),
        );
        let managed = ManagedNetwork::start(policy)
            .await
            .expect("proxy should start")
            .expect("restricted networking should use a proxy");
        let mut client = tokio::net::TcpStream::connect(managed.proxy.http_addr())
            .await
            .expect("managed proxy should accept connections");
        let request = format!(
            "GET http://127.0.0.1:{}/mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: \
             close\r\n\r\n",
            address.port(),
            address.port()
        );
        client
            .write_all(request.as_bytes())
            .await
            .expect("proxy request should write");
        let mut response = [0_u8; 1024];
        let read = client
            .read(&mut response)
            .await
            .expect("proxy response should read");

        assert!(response[..read].starts_with(b"HTTP/1.1 204"));
        server.await.expect("MCP server should finish");
    }

    #[tokio::test]
    async fn proxy_route_rejects_other_loopback_ports() {
        let route = Route::new(&NetworkPolicy::new(
            NetworkAccess::Disabled,
            "127.0.0.1".to_owned(),
            1234,
        ))
        .expect("route should build");

        route.resolve("127.0.0.1", 5678).await.expect_err(
            "the proxy route must reject other loopback ports; this constrains proxied traffic \
             only, because the sandbox permits direct loopback connections that never reach the \
             proxy",
        );
    }

    #[test]
    fn route_rejects_every_non_global_example() {
        for address in [
            "100.64.0.1",
            "169.254.169.254",
            "::ffff:127.0.0.1",
            "2001:db8::1",
        ] {
            let address = address.parse().expect("address should parse");
            assert!(!Route::is_global(address));
        }
        assert!(Route::is_global(
            "8.8.8.8".parse().expect("address should parse")
        ));
        assert!(Route::is_global(
            "2606:4700:4700::1111"
                .parse()
                .expect("address should parse")
        ));
    }
}
