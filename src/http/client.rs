//! Кто клиент запроса: адрес, схема и хост.
//!
//! `X-Forwarded-*` принимаются только от доверенных пиров. Публичный listener
//! доверяет loopback, RFC1918 и ULA и читает только `X-Forwarded-For`; консоль
//! доверяет явному списку сетей и читает ещё `X-Forwarded-Proto` и
//! `X-Forwarded-Host`.

use std::net::{IpAddr, SocketAddr};

use axum::http::{HeaderMap, header};

use crate::config::IpNet;

/// Заголовок с адресом клиента за reverse proxy.
const FORWARDED_FOR_HEADER: &str = "x-forwarded-for";
/// Заголовок со схемой запроса за reverse proxy.
const FORWARDED_PROTO_HEADER: &str = "x-forwarded-proto";
/// Заголовок с хостом запроса за reverse proxy.
const FORWARDED_HOST_HEADER: &str = "x-forwarded-host";

/// Схема, по которой клиент обратился к сервису.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    /// Без TLS, в том числе прямое соединение.
    Http,
    /// TLS, в том числе завершённый доверенным прокси.
    Https,
}

impl Scheme {
    /// Строковое значение схемы.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

/// Клиент запроса: адрес, схема и хост (для origin и флага `Secure`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Client {
    /// IP клиента.
    pub ip: IpAddr,
    /// Схема запроса.
    pub scheme: Scheme,
    /// Хост из `Host` или `X-Forwarded-Host`; `None`, если заголовка нет.
    pub host: Option<String>,
}

/// Набор пиров, которым разрешено задавать `X-Forwarded-*`.
#[derive(Clone, Copy, Debug)]
pub enum TrustedProxies<'a> {
    /// Публичный listener: loopback, RFC1918 и ULA; только `X-Forwarded-For`.
    PrivateNetworks,
    /// Консоль: явный список сетей; `X-Forwarded-For`, `-Proto` и `-Host`.
    Explicit(&'a [IpNet]),
}

impl TrustedProxies<'_> {
    /// Доверенный ли пир.
    fn accepts(self, peer: IpAddr) -> bool {
        match self {
            Self::PrivateNetworks => is_private_network(peer),
            Self::Explicit(networks) => networks.iter().any(|network| network.contains(peer)),
        }
    }
}

impl Client {
    /// Определить клиента по адресу соединения и заголовкам.
    ///
    /// IPv4-mapped адрес пира (`::ffff:127.0.0.1` на dual-stack-сокете)
    /// приводится к IPv4 до проверки доверия и до возврата.
    pub fn resolve(peer: SocketAddr, headers: &HeaderMap, trusted: TrustedProxies<'_>) -> Self {
        let peer_ip = peer.ip().to_canonical();
        let trusted_peer = trusted.accepts(peer_ip);
        let client_ip = if trusted_peer {
            forwarded_ip(headers).unwrap_or(peer_ip)
        } else {
            peer_ip
        };

        match trusted {
            // Публичный listener сохраняет прежнее правило: только адрес
            // клиента, схема соединения и `Host`.
            TrustedProxies::PrivateNetworks => Self {
                ip: client_ip,
                scheme: Scheme::Http,
                host: host_header(headers),
            },
            TrustedProxies::Explicit(_) if trusted_peer => Self {
                ip: client_ip,
                scheme: forwarded_scheme(headers).unwrap_or(Scheme::Http),
                host: forwarded_host(headers).or_else(|| host_header(headers)),
            },
            TrustedProxies::Explicit(_) => Self {
                ip: client_ip,
                scheme: Scheme::Http,
                host: host_header(headers),
            },
        }
    }
}

/// IP клиента из `X-Forwarded-For`; берётся последнее непустое значение.
fn forwarded_ip(headers: &HeaderMap) -> Option<IpAddr> {
    last_header_value(headers, FORWARDED_FOR_HEADER)?
        .parse()
        .ok()
}

/// Схема из `X-Forwarded-Proto`; неизвестное значение не меняет схему
/// соединения.
fn forwarded_scheme(headers: &HeaderMap) -> Option<Scheme> {
    let value = last_header_value(headers, FORWARDED_PROTO_HEADER)?;
    if value.eq_ignore_ascii_case("https") {
        Some(Scheme::Https)
    } else if value.eq_ignore_ascii_case("http") {
        Some(Scheme::Http)
    } else {
        None
    }
}

/// Хост из `X-Forwarded-Host`.
fn forwarded_host(headers: &HeaderMap) -> Option<String> {
    last_header_value(headers, FORWARDED_HOST_HEADER)
}

/// Хост из заголовка `Host`.
fn host_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Последнее непустое значение заголовка. Прокси дописывает свой адрес в конец
/// списка, поэтому именно последнее значение относится к ближайшему узлу.
fn last_header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .rfind(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Адрес доверенного reverse proxy публичного listener: loopback, RFC1918 или
/// IPv6 ULA.
fn is_private_network(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private(),
        IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local(),
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use axum::http::{HeaderMap, HeaderName, HeaderValue};

    use crate::config::IpNet;

    use super::{Client, Scheme, TrustedProxies};

    fn peer(octets: [u8; 4]) -> SocketAddr {
        SocketAddr::from((octets, 4_040))
    }

    fn headers(values: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in values {
            headers.append(
                HeaderName::from_bytes(name.as_bytes()).expect("header name should be valid"),
                HeaderValue::from_str(value).expect("header value should be valid"),
            );
        }
        headers
    }

    fn public_client(peer: SocketAddr, forwarded: Option<&str>) -> Client {
        let mut headers = HeaderMap::new();
        if let Some(forwarded) = forwarded {
            headers.append(
                "x-forwarded-for",
                HeaderValue::from_str(forwarded).expect("forwarded value"),
            );
        }
        Client::resolve(peer, &headers, TrustedProxies::PrivateNetworks)
    }

    fn console_client(peer: SocketAddr, headers: &HeaderMap, networks: &[IpNet]) -> Client {
        Client::resolve(peer, headers, TrustedProxies::Explicit(networks))
    }

    #[test]
    fn the_last_forwarded_address_is_used() {
        let headers = headers(&[("x-forwarded-for", "203.0.113.1, 198.51.100.7")]);
        let client = public_client(peer([127, 0, 0, 1]), None);
        assert_eq!(client.ip, IpAddr::V4(Ipv4Addr::LOCALHOST));

        let client = Client::resolve(
            peer([127, 0, 0, 1]),
            &headers,
            TrustedProxies::PrivateNetworks,
        );
        assert_eq!(client.ip, "198.51.100.7".parse::<IpAddr>().expect("ip"));
    }

    #[test]
    fn a_trusted_peer_uses_the_forwarded_address() {
        let ip: IpAddr = "198.51.100.7".parse().expect("ip");
        assert_eq!(
            public_client(peer([127, 0, 0, 1]), Some("198.51.100.7")).ip,
            ip
        );
        // Docker bridge: reverse proxy на хосте виден как шлюз 172.x.0.1.
        assert_eq!(
            public_client(peer([172, 18, 0, 1]), Some("198.51.100.7")).ip,
            ip
        );
        assert_eq!(
            public_client(peer([127, 0, 0, 1]), None).ip,
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        assert_eq!(
            public_client(peer([127, 0, 0, 1]), Some("not-an-ip")).ip,
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
    }

    #[test]
    fn a_public_peer_ignores_the_forwarded_header() {
        assert_eq!(
            public_client(peer([203, 0, 113, 9]), Some("198.51.100.7")).ip,
            IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9))
        );
    }

    #[test]
    fn an_ipv4_mapped_loopback_peer_is_a_trusted_proxy() {
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:4040".parse().expect("mapped peer");
        assert_eq!(
            public_client(mapped, Some("198.51.100.7")).ip,
            "198.51.100.7".parse::<IpAddr>().expect("ip")
        );
        // Без заголовка пир приводится к каноническому IPv4.
        assert_eq!(
            public_client(mapped, None).ip,
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
    }

    #[test]
    fn the_public_listener_ignores_forwarded_scheme_and_host() {
        let headers = headers(&[
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "console.example"),
            ("host", "public.example:8080"),
        ]);
        let client = Client::resolve(
            peer([127, 0, 0, 1]),
            &headers,
            TrustedProxies::PrivateNetworks,
        );
        assert_eq!(client.scheme, Scheme::Http);
        assert_eq!(client.host.as_deref(), Some("public.example:8080"));
    }

    #[test]
    fn an_empty_console_proxy_list_ignores_all_forwarded_headers() {
        let headers = headers(&[
            ("x-forwarded-for", "198.51.100.7"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "console.example"),
            ("host", "internal.example:8081"),
        ]);
        let client = console_client(peer([127, 0, 0, 1]), &headers, &[]);
        assert_eq!(client.ip, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(client.scheme, Scheme::Http);
        assert_eq!(client.host.as_deref(), Some("internal.example:8081"));
    }

    #[test]
    fn a_listed_console_peer_sets_the_client_scheme_and_host() {
        let networks = [IpNet::parse("10.0.0.0/8").expect("network")];
        let headers = headers(&[
            ("x-forwarded-for", "198.51.100.7"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "console.example"),
            ("host", "internal.example:8081"),
        ]);
        let client = console_client(peer([10, 1, 2, 3]), &headers, &networks);
        assert_eq!(client.ip, "198.51.100.7".parse::<IpAddr>().expect("ip"));
        assert_eq!(client.scheme, Scheme::Https);
        assert_eq!(client.host.as_deref(), Some("console.example"));
    }

    #[test]
    fn an_unlisted_console_peer_ignores_all_forwarded_headers() {
        let networks = [IpNet::parse("10.0.0.0/8").expect("network")];
        let headers = headers(&[
            ("x-forwarded-for", "198.51.100.7"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "console.example"),
            ("host", "internal.example:8081"),
        ]);
        let client = console_client(peer([203, 0, 113, 9]), &headers, &networks);
        assert_eq!(client.ip, IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9)));
        assert_eq!(client.scheme, Scheme::Http);
        assert_eq!(client.host.as_deref(), Some("internal.example:8081"));
    }

    #[test]
    fn an_ipv4_mapped_console_peer_is_trusted_when_listed() {
        let networks = [IpNet::parse("10.0.0.0/8").expect("network")];
        let mapped: SocketAddr = "[::ffff:10.0.0.1]:4040".parse().expect("mapped peer");
        let headers = headers(&[("x-forwarded-for", "198.51.100.7")]);
        let client = console_client(mapped, &headers, &networks);
        assert_eq!(client.ip, "198.51.100.7".parse::<IpAddr>().expect("ip"));

        let client = console_client(mapped, &HeaderMap::new(), &networks);
        assert_eq!(client.ip, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
    }

    #[test]
    fn an_invalid_console_forwarded_scheme_keeps_the_connection_scheme() {
        let networks = [IpNet::parse("127.0.0.1").expect("network")];
        let headers = headers(&[("x-forwarded-proto", "ftp")]);
        let client = console_client(peer([127, 0, 0, 1]), &headers, &networks);
        assert_eq!(client.scheme, Scheme::Http);
    }

    #[test]
    fn a_trusted_console_peer_without_forwarded_host_uses_the_host_header() {
        let networks = [IpNet::parse("127.0.0.1").expect("network")];
        let headers = headers(&[("host", "internal.example:8081")]);
        let client = console_client(peer([127, 0, 0, 1]), &headers, &networks);
        assert_eq!(client.host.as_deref(), Some("internal.example:8081"));
    }
}
