//! Роутер консоли администратора.
//!
//! Консоль живёт на отдельном listener: публичный [`crate::http::router`] не
//! содержит её маршрутов. Все ответы консоли, включая редиректы и ошибки,
//! получают запрет кеширования и встраивания во фреймы одним слоем на роутере;
//! исключение — встроенные ресурсы с хешем содержимого в имени (см.
//! [`security_headers`]).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, header};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::capture::slot_status::LlmSlotHealth;
use crate::config::{Config, IpNet};
use crate::domain::models::{Actor, ActorType};
use crate::http::client::{Client, TrustedProxies};
use crate::llm::LlmSettings;
use crate::rate_limit::{RateDecision, RateLimiter};
use crate::security_audit::{Audit, LimitKey, RateLimitEvent};
use crate::telegram::runtime::TelegramSupervisor;

use super::checks::CheckState;
use super::wizard::{WIZARD_PATH, WIZARD_SKIP_PATH, WizardSettings};
use super::{assets, households, origin, pages};

/// Сколько неудач допускается в окне лимита консоли.
pub const FAILURE_LIMIT: u32 = 5;
/// Окно лимита консоли, секунды.
pub const FAILURE_WINDOW_SECONDS: i64 = 900;

/// Состояние консоли: база, доверенные прокси, журнал и лимиты.
#[derive(Clone)]
pub struct AdminState {
    /// Пул соединений PostgreSQL.
    pub pool: PgPool,
    /// Сети, от которых принимаются `X-Forwarded-*`.
    trusted_proxies: Vec<IpNet>,
    /// Тот же журнал событий, что у публичного роутера: порог отказов
    /// аутентификации общий.
    pub audit: Audit,
    /// Бакеты неудач по IP клиента. Ключ нормализуется
    /// [`ip_key`](crate::rate_limit::ip_key): IPv6 сводится к /64, как у лимита
    /// webhook, чтобы смена младших 64 бит не обходила лимит. Журнал при этом
    /// хранит полный адрес клиента. `Arc`, чтобы клоны состояния делили
    /// счётчики: новые лимитеры при копировании не создаются.
    login_limiter: Arc<RateLimiter<IpAddr>>,
    setup_limiter: Arc<RateLimiter<IpAddr>>,
    current_password_limiter: Arc<RateLimiter<IpAddr>>,
    recovery_limiter: Arc<RateLimiter<IpAddr>>,
    /// Режим размещения, признак Telegram и параметры LLM для мастера.
    wizard: WizardSettings,
    /// Тот же экземпляр применения настроек LLM, что у рантайма: страница
    /// меняет настройки без перезапуска, а мастер берёт оттуда же окружение и
    /// ключи шифрования.
    llm_settings: LlmSettings,
    /// Статус проверки слотов LLM; по умолчанию поверх того же снимка, что у
    /// `llm_settings`, а боевой путь получает общий экземпляр с readiness.
    llm_slots: Arc<LlmSlotHealth>,
    /// Допустимый возраст необработанного обновления Telegram для предупреждений
    /// readiness на главной; `None` — очередь не проверяется.
    queue_lag_limit: Option<chrono::Duration>,
    /// Окружение и служебный адрес Bot API для страницы Telegram. Живое
    /// состояние поколения берётся у супервизора, а не из отдельной копии.
    telegram_config: Option<Config>,
    /// Ход проверки слота; одна проверка на процесс.
    checks: CheckState,
    /// Супервизор поколений Telegram: переключение настроек без перезапуска.
    telegram_supervisor: Option<Arc<TelegramSupervisor>>,
}

impl AdminState {
    /// Создать состояние консоли.
    ///
    /// Журнал передаётся снаружи: неудачные входы в консоль должны попадать в
    /// общий порог отказов аутентификации публичного роутера. Настройки LLM
    /// обязательны: без них страница писала бы в базу, но не в снимок рантайма.
    pub fn new(
        pool: PgPool,
        trusted_proxies: Vec<IpNet>,
        audit: Audit,
        llm_settings: LlmSettings,
    ) -> Self {
        let llm_slots = Arc::new(LlmSlotHealth::new(llm_settings.handle().clone()));
        Self {
            pool,
            trusted_proxies,
            audit,
            login_limiter: failure_limiter(),
            setup_limiter: failure_limiter(),
            current_password_limiter: failure_limiter(),
            recovery_limiter: failure_limiter(),
            wizard: WizardSettings::default(),
            llm_settings,
            llm_slots,
            queue_lag_limit: None,
            telegram_config: None,
            checks: CheckState::new(),
            telegram_supervisor: None,
        }
    }

    /// Задать супервизор поколений Telegram.
    ///
    /// Консоль переключает бота без перезапуска тем же супервизором, что
    /// запустил текущее поколение. Живое состояние поколения читается у него
    /// же: отдельной копии слота в консоли нет.
    pub fn with_telegram_supervisor(mut self, supervisor: Arc<TelegramSupervisor>) -> Self {
        self.telegram_supervisor = Some(supervisor);
        self
    }

    /// Задать окружение для страницы Telegram.
    ///
    /// Нужны `TELEGRAM_API_BASE_URL`, `PUBLIC_BASE_URL`, `ENV` и признак группы
    /// из окружения; настройки читаются из того же пула и ключа, что у LLM.
    pub fn with_telegram_config(mut self, config: Config) -> Self {
        self.telegram_config = Some(config);
        self
    }

    /// Окружение страницы Telegram, если консоль собрана рантаймом.
    pub fn telegram_config(&self) -> Option<&Config> {
        self.telegram_config.as_ref()
    }

    /// Супервизор поколений Telegram, если консоль собрана рантаймом.
    pub fn telegram_supervisor(&self) -> Option<&Arc<TelegramSupervisor>> {
        self.telegram_supervisor.as_ref()
    }

    /// Есть живое поколение Telegram; у консоли без рантайма — нет.
    pub fn telegram_active(&self) -> bool {
        self.telegram_supervisor
            .as_ref()
            .is_some_and(|supervisor| supervisor.ingress().is_active())
    }

    /// Задать тот же экземпляр проверки слотов, что у readiness.
    pub fn with_llm_slots(mut self, llm_slots: Arc<LlmSlotHealth>) -> Self {
        self.llm_slots = llm_slots;
        self
    }

    /// Задать порог задержки очереди Telegram, как у readiness.
    pub fn with_queue_lag_limit(mut self, seconds: u32) -> Self {
        self.queue_lag_limit = Some(chrono::Duration::seconds(i64::from(seconds)));
        self
    }

    /// Применение настроек LLM без перезапуска.
    pub fn llm_settings(&self) -> &LlmSettings {
        &self.llm_settings
    }

    /// Статус проверки слотов LLM для главной.
    pub fn llm_slots(&self) -> &LlmSlotHealth {
        &self.llm_slots
    }

    /// Порог задержки очереди Telegram для readiness на главной.
    pub fn queue_lag_limit(&self) -> Option<chrono::Duration> {
        self.queue_lag_limit
    }

    /// Состояние проверки слота.
    pub fn checks(&self) -> &CheckState {
        &self.checks
    }

    /// Задать параметры мастера: режим и Telegram.
    ///
    /// Окружение LLM и ключи шифрования не дублируются: их читает
    /// [`Self::llm_settings`].
    pub fn with_wizard(mut self, wizard: WizardSettings) -> Self {
        self.wizard = wizard;
        self
    }

    /// Параметры чтения снимка мастера.
    pub fn wizard(&self) -> &WizardSettings {
        &self.wizard
    }

    /// Клиент по адресу соединения и заголовкам запроса.
    ///
    /// Доверие задаётся только явным списком `ADMIN_TRUSTED_PROXIES`; пустой
    /// список не доверяет никому. Схема клиента решает флаг `Secure` у cookie.
    pub fn client(&self, peer: SocketAddr, headers: &HeaderMap) -> Client {
        Client::resolve(
            peer,
            headers,
            TrustedProxies::Explicit(&self.trusted_proxies),
        )
    }

    /// Бакет неудачных входов.
    pub fn login_limiter(&self) -> Arc<RateLimiter<IpAddr>> {
        Arc::clone(&self.login_limiter)
    }

    /// Бакет неудачных setup-кодов.
    pub fn setup_limiter(&self) -> Arc<RateLimiter<IpAddr>> {
        Arc::clone(&self.setup_limiter)
    }

    /// Бакет неверного текущего пароля.
    pub fn current_password_limiter(&self) -> Arc<RateLimiter<IpAddr>> {
        Arc::clone(&self.current_password_limiter)
    }

    /// Бакет восстановления доступа.
    pub fn recovery_limiter(&self) -> Arc<RateLimiter<IpAddr>> {
        Arc::clone(&self.recovery_limiter)
    }

    /// Записать в журнал превышение лимита консоли.
    ///
    /// Ключ — IP клиента без секретов. Повторы подавляет сам [`Audit`].
    pub async fn audit_rate_limited(
        &self,
        limit: &'static str,
        ip: IpAddr,
        decision: RateDecision,
        now: DateTime<Utc>,
    ) {
        self.audit
            .rate_limited(
                RateLimitEvent {
                    limit,
                    key: LimitKey::Ip(ip),
                    retry_after: decision.retry_after.unwrap_or(0),
                    household_id: None,
                    actor: Actor::system(ActorType::System),
                },
                now,
            )
            .await;
    }
}

/// Бакет неудач консоли: 5 за 15 минут на клиента.
fn failure_limiter() -> Arc<RateLimiter<IpAddr>> {
    Arc::new(RateLimiter::with_window(
        FAILURE_LIMIT,
        FAILURE_WINDOW_SECONDS,
    ))
}

/// Собрать роутер консоли.
///
/// Сверка `Origin` стоит отдельным слоем, а не вызовом в каждом обработчике:
/// новые формы получают защиту автоматически. `security_headers` добавлен
/// последним, поэтому покрывает и `403` слоя сверки.
pub fn build_router(state: AdminState) -> Router {
    Router::new()
        .route("/admin", get(pages::index))
        .route(
            "/admin/households",
            get(households::list).post(households::create),
        )
        .route("/admin/households/{id}", get(households::detail))
        .route(WIZARD_PATH, get(pages::wizard_page))
        .route(WIZARD_SKIP_PATH, post(pages::wizard_skip))
        .route("/admin/wizard/household", post(pages::wizard_household))
        .route("/admin/wizard/token", post(pages::wizard_token))
        .route(
            pages::LLM_PATH,
            get(pages::llm_page).post(pages::llm_submit),
        )
        .route(
            pages::TELEGRAM_PATH,
            get(pages::telegram_page).post(pages::telegram_submit),
        )
        .route(
            pages::TELEGRAM_SECRET_PATH,
            post(pages::telegram_reissue_secret),
        )
        .route(pages::OWNER_PATH, post(pages::owner_submit))
        .route(pages::TELEGRAM_LINK_PATH, post(pages::telegram_link_issue))
        .route(
            pages::TELEGRAM_LINK_CONFIRM_PATH,
            post(pages::telegram_link_confirm),
        )
        .route(
            pages::TELEGRAM_LINK_REJECT_PATH,
            post(pages::telegram_link_reject),
        )
        .route(
            pages::TELEGRAM_LINK_UNLINK_PATH,
            post(pages::telegram_link_unlink),
        )
        .route(pages::LLM_CHECK_PATH, get(pages::llm_check_page))
        .route(
            pages::LLM_CHECK_FRAGMENT_PATH,
            get(pages::llm_check_fragment),
        )
        .route("/admin/llm/check/{slot}", post(pages::llm_check_start))
        .route(
            "/admin/setup",
            get(pages::setup_page).post(pages::setup_submit),
        )
        .route(
            "/admin/login",
            get(pages::login_page).post(pages::login_submit),
        )
        .route(
            "/admin/recover/{code}",
            get(pages::recover_page).post(pages::recover_submit),
        )
        .route(
            "/admin/password",
            get(pages::password_page).post(pages::password_submit),
        )
        .route("/admin/logout", post(pages::logout))
        .route("/admin/assets/{file}", get(assets::serve))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_trusted_origin,
        ))
        .layer(axum::middleware::from_fn(security_headers))
        .with_state(state)
}

/// Отклонить не-GET-запрос без `Origin`/`Referer` своего же origin.
///
/// Проверка стоит до обработчика: ни сессия, ни код не используются, ни одна
/// строка не пишется. Причина уходит в журнал без значений cookie и заголовков.
async fn require_trusted_origin(
    State(state): State<AdminState>,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method();
    if method == Method::GET || method == Method::HEAD {
        return next.run(request).await;
    }

    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| *peer);
    let allowed = peer.is_some_and(|peer| {
        let client = state.client(peer, request.headers());
        origin::allows(&client, request.headers())
    });

    if !allowed {
        // Шаблон маршрута, а не сырой путь: в `/admin/recover/{code}` лежит
        // действующий одноразовый код, и он не должен попадать в журнал.
        let route = request
            .extensions()
            .get::<MatchedPath>()
            .map(|path| path.as_str().to_owned());
        tracing::info!(
            method = %request.method(),
            route = route.as_deref().unwrap_or("<unmatched>"),
            "admin console request rejected: origin does not match"
        );
        return origin::forbidden();
    }

    next.run(request).await
}

/// Заголовки всех ответов консоли.
///
/// `no-store` не даёт прокси и браузеру сохранить страницу с формой или
/// cookie; запрет фреймов закрывает clickjacking. Один слой на роутере
/// покрывает и HTML, и `303`, и ошибки обработчиков.
///
/// `no-store` ставится только тем ответам, где обработчик не задал свой
/// `Cache-Control`: так встроенные ресурсы с хешем содержимого в имени
/// сохраняют долгий срок и `immutable`, а их `404` и все страницы получают
/// `no-store`. Остальные заголовки безопасности общие.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    if !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}
