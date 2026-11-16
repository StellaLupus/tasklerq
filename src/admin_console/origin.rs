//! Сверка `Origin`/`Referer` изменяющих запросов консоли с origin запроса.
//!
//! Origin запроса — тройка (схема, хост, порт): схема из [`Client::scheme`],
//! хост и порт — разбор [`Client::host`] (значение `Host` или
//! `X-Forwarded-Host` от доверенного прокси). Порт по умолчанию — 80 для
//! `http` и 443 для `https`. Значение хоста разбирается, а не сравнивается
//! строкой: мусор, `user@host` и путь делают origin запроса неизвестным, и
//! запрос отклоняется.
//!
//! Источник — заголовок `Origin`; при его отсутствии `Referer`, у которого
//! берутся только схема, хост и порт. `Origin: null`, неразбираемое значение и
//! отсутствие обоих заголовков отклоняются. Если `Origin` есть, но не
//! совпадает, на `Referer` не откатываемся: иначе чужой сайт мог бы прислать
//! неверный `Origin` и верный `Referer`.
//!
//! Cookie консоли привязана к хосту, а не к порту, и `SameSite` не разделяет
//! соседние порты, поэтому проверяются **все** не-GET-запросы, включая вход,
//! setup и восстановление.

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use url::Url;

use crate::http::client::{Client, Scheme};

/// Origin запроса или источника: схема, хост и порт.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    scheme: Scheme,
    /// Хост в каноническом виде: домен в нижнем регистре, IPv6 в скобках.
    host: String,
    port: u16,
}

impl Origin {
    /// Совпадают ли origin'ы: схема и порт точно, хост без учёта регистра.
    fn matches(&self, other: &Self) -> bool {
        self.scheme == other.scheme
            && self.port == other.port
            && self.host.eq_ignore_ascii_case(&other.host)
    }
}

/// Совпадает ли источник изменяющего запроса с origin запроса.
///
/// `false` — запрос нужно отклонить: origin запроса неизвестен, источника нет
/// или они различаются.
pub fn allows(client: &Client, headers: &HeaderMap) -> bool {
    let Some(request) = request_origin(client) else {
        return false;
    };
    let Some(source) = source_origin(headers) else {
        return false;
    };
    request.matches(&source)
}

/// Origin запроса по клиенту: схема и разобранный `Host`.
///
/// `None`, если хоста нет или его нельзя разобрать на хост и порт.
pub fn request_origin(client: &Client) -> Option<Origin> {
    let authority = client.host.as_deref()?;
    authority_origin(client.scheme, authority)
}

/// Origin источника: `Origin`, а при его отсутствии — `Referer`.
///
/// Наличие `Origin` отменяет откат на `Referer`, даже если значение
/// неразбираемо.
pub fn source_origin(headers: &HeaderMap) -> Option<Origin> {
    if let Some(value) = headers.get(header::ORIGIN) {
        return origin_header(value.to_str().ok()?);
    }
    let value = headers.get(header::REFERER)?.to_str().ok()?;
    from_url(&Url::parse(value.trim()).ok()?)
}

/// Ответ на несовпадение: короткий текст без подробностей причины.
pub fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, "Forbidden").into_response()
}

/// Разобрать `Host`/`X-Forwarded-Host` как authority и собрать origin.
///
/// Значение обязано быть только хостом и необязательным портом: userinfo,
/// путь, query и fragment — уже не authority, и origin считается неизвестным.
fn authority_origin(scheme: Scheme, authority: &str) -> Option<Origin> {
    let url = Url::parse(&format!("{}://{}", scheme.as_str(), authority)).ok()?;
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    from_url(&url)
}

/// Разобрать значение `Origin`.
///
/// Заголовок обязан содержать только origin: `null`, путь, query, fragment и
/// userinfo отклоняются. Браузер присылает сериализованный origin без пути.
fn origin_header(value: &str) -> Option<Origin> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("null") {
        return None;
    }
    let url = Url::parse(value).ok()?;
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    from_url(&url)
}

/// Схема, хост и порт URL; порт — явный или по умолчанию для схемы.
fn from_url(url: &Url) -> Option<Origin> {
    let scheme = match url.scheme() {
        "http" => Scheme::Http,
        "https" => Scheme::Https,
        _ => return None,
    };
    let host = url.host_str()?.to_owned();
    let port = url.port_or_known_default()?;
    Some(Origin { scheme, host, port })
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderName, HeaderValue};

    use crate::http::client::{Client, Scheme};

    use super::{allows, request_origin};

    fn client(scheme: Scheme, host: &str) -> Client {
        Client {
            ip: "127.0.0.1".parse().expect("ip"),
            scheme,
            host: Some(host.to_owned()),
        }
    }

    fn headers(values: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in values {
            headers.append(
                HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                HeaderValue::from_str(value).expect("header value"),
            );
        }
        headers
    }

    #[test]
    fn default_ports_belong_to_the_scheme() {
        // http://nas и http://nas:80 — один origin.
        assert!(allows(
            &client(Scheme::Http, "nas"),
            &headers(&[("origin", "http://nas:80")])
        ));
        assert!(allows(
            &client(Scheme::Http, "nas:80"),
            &headers(&[("origin", "http://nas")])
        ));
        // https://nas и https://nas:443 — один origin.
        assert!(allows(
            &client(Scheme::Https, "nas"),
            &headers(&[("origin", "https://nas:443")])
        ));
        assert!(allows(
            &client(Scheme::Https, "nas:443"),
            &headers(&[("origin", "https://nas")])
        ));
        // Явный порт другой схемы — уже другой origin.
        assert!(!allows(
            &client(Scheme::Http, "nas:443"),
            &headers(&[("origin", "https://nas:443")])
        ));
    }

    #[test]
    fn the_host_is_compared_without_case() {
        assert!(allows(
            &client(Scheme::Http, "NAS.Example:18081"),
            &headers(&[("origin", "http://nas.example:18081")])
        ));
        assert!(allows(
            &client(Scheme::Http, "nas.example:18081"),
            &headers(&[("origin", "http://NAS.EXAMPLE:18081")])
        ));
    }

    #[test]
    fn an_ipv6_host_is_compared_in_brackets() {
        assert!(allows(
            &client(Scheme::Http, "[fd00::1]:18081"),
            &headers(&[("origin", "http://[fd00::1]:18081")])
        ));
        // Другой порт и отсутствие скобок — не тот origin.
        assert!(!allows(
            &client(Scheme::Http, "[fd00::1]:18081"),
            &headers(&[("origin", "http://[fd00::1]:8123")])
        ));
        assert!(request_origin(&client(Scheme::Http, "fd00::1")).is_none());
    }

    #[test]
    fn the_referer_contributes_only_its_origin() {
        assert!(allows(
            &client(Scheme::Http, "nas:18081"),
            &headers(&[("referer", "http://nas:18081/admin/login?next=%2Fadmin#top")])
        ));
        assert!(!allows(
            &client(Scheme::Http, "nas:18081"),
            &headers(&[("referer", "http://nas:8123/admin/login")])
        ));
    }

    #[test]
    fn a_null_or_broken_origin_does_not_fall_back_to_the_referer() {
        for origin in ["null", "NULL", "not a url", "/relative", "http://nas/path"] {
            let headers = headers(&[
                ("origin", origin),
                ("referer", "http://nas:18081/admin/login"),
            ]);
            assert!(
                !allows(&client(Scheme::Http, "nas:18081"), &headers),
                "Origin {origin:?} must be refused even with a matching Referer"
            );
        }
    }

    #[test]
    fn a_missing_source_is_refused() {
        assert!(!allows(&client(Scheme::Http, "nas:18081"), &headers(&[])));
        // Пустые значения — тоже отсутствие источника.
        assert!(!allows(
            &client(Scheme::Http, "nas:18081"),
            &headers(&[("origin", "")])
        ));
        assert!(!allows(
            &client(Scheme::Http, "nas:18081"),
            &headers(&[("referer", "")])
        ));
    }

    #[test]
    fn garbage_in_the_request_host_makes_the_origin_unknown() {
        for host in [
            "",
            "user@nas:18081",
            "nas/path",
            "nas?query=1",
            "not a host",
        ] {
            assert!(
                request_origin(&client(Scheme::Http, host)).is_none(),
                "host {host:?} must not parse"
            );
            assert!(!allows(
                &client(Scheme::Http, host),
                &headers(&[("origin", "http://nas:18081")])
            ));
        }
        // `X-Forwarded-Host` от доверенного прокси разбирается так же.
        assert!(!allows(
            &client(Scheme::Https, "user@console.example"),
            &headers(&[("origin", "https://console.example")])
        ));
    }

    #[test]
    fn different_schemes_with_the_same_host_and_port_do_not_match() {
        assert!(!allows(
            &client(Scheme::Http, "nas:18081"),
            &headers(&[("origin", "https://nas:18081")])
        ));
        assert!(!allows(
            &client(Scheme::Https, "nas:18081"),
            &headers(&[("origin", "http://nas:18081")])
        ));
    }

    #[test]
    fn an_origin_with_a_userinfo_or_path_is_refused() {
        for origin in ["http://user@nas:18081", "http://nas:18081/path"] {
            assert!(!allows(
                &client(Scheme::Http, "nas:18081"),
                &headers(&[("origin", origin)])
            ));
        }
    }
}
