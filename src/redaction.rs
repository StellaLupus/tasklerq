//! Удаление секретов из сообщений об ошибках и служебного вывода.
//!
//! Основная защита — [`crate::config::SecretString`] и типизированные ошибки
//! клиентов. Этот модуль нужен на внешних границах, куда попадает уже готовый
//! текст: сообщение об ошибке старта/CLI и произвольные `reqwest::Error`.
//! Глобальный фильтр журналирования не ставится: точечных правок достаточно.

use url::Url;

/// Замена секретного фрагмента.
pub const REDACTED: &str = "<redacted>";

/// Переменные окружения, значения которых считаются секретами целиком.
const SECRET_ENV: [&str; 3] = ["TELEGRAM_BOT_TOKEN", "TELEGRAM_WEBHOOK_SECRET", "HA_TOKEN"];

/// Убрать учётные данные из одного URL, сохранив остальные части.
pub fn redact_url(url: &str) -> String {
    let Ok(mut parsed) = Url::parse(url) else {
        return url.to_owned();
    };
    if parsed.username().is_empty() && parsed.password().is_none() {
        return url.to_owned();
    }

    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.to_string()
}

/// Убрать учётные данные из URL, встретившихся в произвольном тексте.
///
/// Ошибки `reqwest` и `sqlx` иногда содержат строку подключения целиком, а
/// `anyhow` печатает всю цепочку источников. Функция находит фрагменты вида
/// `scheme://...`, разбирает их как URL и переписывает только те, где есть
/// логин или пароль.
pub fn redact_url_credentials(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;

    while let Some(relative) = text[cursor..].find("://") {
        let scheme_at = cursor + relative;
        let start = url_start(text, scheme_at);
        let original_end = url_end(text, scheme_at + 3);
        let mut end = original_end;

        loop {
            let candidate = &text[start..end];
            if let Ok(parsed) = Url::parse(candidate) {
                output.push_str(&text[cursor..start]);
                if parsed.username().is_empty() && parsed.password().is_none() {
                    output.push_str(candidate);
                } else {
                    output.push_str(&redact_url(candidate));
                }
                output.push_str(&text[end..original_end]);
                cursor = original_end;
                break;
            }

            let Some(last) = text[..end].chars().next_back() else {
                break;
            };
            if !is_trailing_punctuation(last) {
                // Разбор не удался, и это не завершающая пунктуация: не трогаем
                // фрагмент, чтобы не испортить полезный текст.
                output.push_str(&text[cursor..scheme_at + 3]);
                cursor = scheme_at + 3;
                break;
            }
            end -= last.len_utf8();
        }
    }

    output.push_str(&text[cursor..]);
    output
}

/// Заменить известные секретные значения на [`REDACTED`].
pub fn replace_known_secrets(text: &str, secrets: &[String]) -> String {
    let mut output = text.to_owned();
    for secret in secrets {
        if !secret.is_empty() {
            output = output.replace(secret, REDACTED);
        }
    }
    output
}

/// Секреты из переменных окружения процесса.
///
/// Нужны на границе `main`, где ошибка уже потеряла типы конфигурации. Пароль
/// базы извлекается из `DATABASE_URL`, потому что `sqlx` может напечатать
/// строку подключения в цепочке источников.
pub fn environment_secrets() -> Vec<String> {
    let mut secrets: Vec<String> = SECRET_ENV
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .filter(|value| !value.is_empty())
        .collect();

    if let Ok(raw) = std::env::var("DATABASE_URL")
        && let Ok(url) = Url::parse(&raw)
        && let Some(password) = url.password()
        && !password.is_empty()
    {
        secrets.push(password.to_owned());
    }

    secrets
}

/// Полная очистка текста на внешней границе: известные секреты и учётные
/// данные в URL.
pub fn sanitize(text: &str) -> String {
    let secrets = environment_secrets();
    redact_url_credentials(&replace_known_secrets(text, &secrets))
}

fn url_start(text: &str, scheme_at: usize) -> usize {
    let mut start = scheme_at;
    for (offset, ch) in text[..scheme_at].char_indices().rev() {
        if ch.is_whitespace() || is_url_start_boundary(ch) {
            break;
        }
        start = offset;
    }
    start
}

fn url_end(text: &str, from: usize) -> usize {
    text[from..]
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace() || is_url_end_boundary(*ch))
        .map_or(text.len(), |(offset, _)| from + offset)
}

fn is_url_start_boundary(ch: char) -> bool {
    matches!(
        ch,
        '"' | '\'' | '`' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '=' | ':'
    )
}

/// Конец URL не обрывается на `[`/`]`: это часть IPv6-адреса.
fn is_url_end_boundary(ch: char) -> bool {
    matches!(ch, '"' | '\'' | '`' | '<' | '>')
}

fn is_trailing_punctuation(ch: char) -> bool {
    matches!(ch, ')' | ']' | '}' | '.' | ',' | ';' | ':' | '!' | '?')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_credentials_are_removed_but_host_and_path_stay() {
        assert_eq!(
            redact_url("postgres://tasklerq:S3cretPassw0rd@db:5432/tasklerq?sslmode=disable"),
            "postgres://db:5432/tasklerq?sslmode=disable"
        );
        assert_eq!(
            redact_url("https://db.example.com/health"),
            "https://db.example.com/health"
        );
    }

    #[test]
    fn credentials_are_removed_from_text_and_trailing_punctuation_is_kept() {
        let text = "connect failed for (postgres://user:S3cretPassw0rd@db:5432/tasklerq), retry";
        assert_eq!(
            redact_url_credentials(text),
            "connect failed for (postgres://db:5432/tasklerq), retry"
        );

        let text = "url: https://user:S3cretPassw0rd@example.test/path.";
        assert_eq!(
            redact_url_credentials(text),
            "url: https://example.test/path."
        );

        let text = "url:https://user:S3cretPassw0rd@example.test/path";
        assert_eq!(
            redact_url_credentials(text),
            "url:https://example.test/path"
        );
    }

    #[test]
    fn urls_without_credentials_and_plain_text_are_unchanged() {
        let text = "GET https://example.test/health failed; host db:5432";
        assert_eq!(redact_url_credentials(text), text);
        assert_eq!(redact_url_credentials("no urls here"), "no urls here");
    }

    #[test]
    fn known_secrets_are_replaced_everywhere() {
        let text = "token=abc123 password=abc123";
        let secrets = vec!["abc123".to_owned()];
        assert_eq!(
            replace_known_secrets(text, &secrets),
            "token=<redacted> password=<redacted>"
        );
    }
}
