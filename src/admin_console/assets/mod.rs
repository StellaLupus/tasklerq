//! Встроенные ресурсы консоли: CSS и htmx.
//!
//! Файлы лежат рядом с модулем и попадают в бинарник через `include_bytes!`,
//! поэтому консоль работает в LAN без интернета (design 0013, решение 12).
//! В имени файла — короткий хеш содержимого, поэтому ответ можно кешировать
//! навсегда, а неизвестное имя даёт `404`. Ресурсы отдаются без сессии:
//! страница входа тоже должна быть со стилями.

use std::sync::OnceLock;

use axum::body::Body;
use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

/// Префикс путей ресурсов консоли.
pub const PREFIX: &str = "/admin/assets/";

/// `Cache-Control` ресурса с хешем в имени: содержимое не изменится.
pub const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";

/// CSS консоли: формы, сообщения и навигация.
pub const CONSOLE_CSS: &[u8] = include_bytes!("console.css");

/// htmx 2.0.4 без изменений; версия, SRI и лицензия — в README рядом.
pub const HTMX_JS: &[u8] = include_bytes!("htmx.min.js");

/// Сколько байт SHA-256 попадает в имя файла: 8 байт — 16 hex-символов.
const HASH_BYTES: usize = 8;

/// Ресурс, отдаваемый консолью.
pub struct Asset {
    /// Имя в `/admin/assets/`: `<префикс>-<хеш>.<расширение>`.
    pub name: String,
    /// Полный путь для ссылок из шаблонов.
    pub path: String,
    /// `Content-Type` файла.
    pub content_type: &'static str,
    /// Содержимое файла.
    pub body: &'static [u8],
}

impl Asset {
    /// Собрать ресурс: хеш считается по встроенному содержимому один раз.
    fn new(prefix: &str, ext: &str, content_type: &'static str, body: &'static [u8]) -> Self {
        let digest = Sha256::digest(body);
        let hash: String = digest[..HASH_BYTES]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let name = format!("{prefix}-{hash}.{ext}");

        Self {
            path: format!("{PREFIX}{name}"),
            name,
            content_type,
            body,
        }
    }
}

/// Оба ресурса собираются один раз: хеш считается от встроенных байтов.
struct Assets {
    css: Asset,
    htmx: Asset,
}

static ASSETS: OnceLock<Assets> = OnceLock::new();

fn assets() -> &'static Assets {
    ASSETS.get_or_init(|| Assets {
        css: Asset::new("console", "css", "text/css; charset=utf-8", CONSOLE_CSS),
        htmx: Asset::new("htmx", "js", "text/javascript; charset=utf-8", HTMX_JS),
    })
}

/// Путь CSS с хешем содержимого.
pub fn console_css_path() -> &'static str {
    &assets().css.path
}

/// Путь htmx с хешем содержимого.
pub fn htmx_path() -> &'static str {
    &assets().htmx.path
}

/// Найти ресурс по имени из адреса; неизвестное имя — `None`.
pub fn find(name: &str) -> Option<&'static Asset> {
    let assets = assets();
    [&assets.css, &assets.htmx]
        .into_iter()
        .find(|asset| asset.name == name)
}

/// `GET /admin/assets/{file}`: отдать ресурс по имени с хешем содержимого.
pub async fn serve(Path(name): Path<String>) -> Response {
    let Some(asset) = find(&name) else {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    };

    let mut response = Response::new(Body::from(asset.body));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(IMMUTABLE_CACHE),
    );
    response
}
