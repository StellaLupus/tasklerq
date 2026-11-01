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

/// Слишком короткие значения не заменяются глобально: иначе секрет вроде
/// `abc` испортил бы обычные слова. Это осознанный компромисс: короткий
/// пароль может остаться в диагностическом тексте.
const MIN_SECRET_CHARS: usize = 6;

/// Убрать учётные данные из одного URL, сохранив остальные части.
///
/// При ошибке разбора URL очистка не отменяется: userinfo вырезается
/// лексически, иначе пароль остался бы в тексте (fail-closed).
pub fn redact_url(url: &str) -> String {
    let Ok(mut parsed) = Url::parse(url) else {
        return strip_userinfo_lexically(url);
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
/// логин или пароль. Если разбор не удался, userinfo вырезается лексически.
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
            if Url::parse(candidate).is_ok() {
                output.push_str(&text[cursor..start]);
                output.push_str(&redact_url(candidate));
                output.push_str(&text[end..original_end]);
                cursor = original_end;
                break;
            }

            match text[..end].chars().next_back() {
                Some(last) if is_trailing_punctuation(last) => end -= last.len_utf8(),
                // Разбор не удался и это не завершающая пунктуация:
                // вырезаем userinfo лексически, не оставляя пароль в тексте.
                _ => {
                    output.push_str(&text[cursor..start]);
                    output.push_str(&strip_userinfo_lexically(candidate));
                    output.push_str(&text[end..original_end]);
                    cursor = original_end;
                    break;
                }
            }
        }
    }

    output.push_str(&text[cursor..]);
    output
}

/// Вырезать всё между `://` и последним `@` до первого `/`, `?` или `#`.
fn strip_userinfo_lexically(url: &str) -> String {
    let Some(scheme_at) = url.find("://") else {
        return url.to_owned();
    };
    let authority_start = scheme_at + 3;
    let authority_end = url[authority_start..]
        .char_indices()
        .find(|(_, ch)| matches!(ch, '/' | '?' | '#'))
        .map_or(url.len(), |(offset, _)| authority_start + offset);
    let authority = &url[authority_start..authority_end];
    let Some(at) = authority.rfind('@') else {
        return url.to_owned();
    };

    let mut output = String::with_capacity(url.len());
    output.push_str(&url[..authority_start]);
    output.push_str(&authority[at + 1..]);
    output.push_str(&url[authority_end..]);
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
    let mut vars: Vec<(&str, String)> = SECRET_ENV
        .iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| (*name, value)))
        .collect();
    if let Ok(raw) = std::env::var("DATABASE_URL") {
        vars.push(("DATABASE_URL", raw));
    }
    collect_secrets(vars.iter().map(|(name, value)| (*name, value.as_str())))
}

/// Чистая часть [`environment_secrets`]: известные секреты из пар `(имя, значение)`.
fn collect_secrets<'a>(vars: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<String> {
    let mut secrets = Vec::new();
    let mut database_url = None;

    for (name, value) in vars {
        if name == "DATABASE_URL" {
            database_url = Some(value);
        } else if SECRET_ENV.contains(&name) {
            push_secret(&mut secrets, value);
        }
    }

    if let Some(raw) = database_url
        && let Ok(url) = Url::parse(raw)
        && let Some(password) = url.password()
    {
        push_secret(&mut secrets, password);
        // Пароль мог прийти percent-encoded; в тексте ошибки он может быть как
        // закодированным, так и раскрытым.
        if let Some(decoded) = percent_decode(password) {
            push_secret(&mut secrets, &decoded);
        }
    }

    secrets
}

fn push_secret(secrets: &mut Vec<String>, value: &str) {
    if value.chars().count() < MIN_SECRET_CHARS || secrets.iter().any(|known| known == value) {
        return;
    }
    secrets.push(value.to_owned());
}

/// Декодировать `%XX` без внешнего крейта; `None`, если замен нет.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut changed = false;

    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            output.push(high * 16 + low);
            index += 3;
            changed = true;
            continue;
        }
        output.push(bytes[index]);
        index += 1;
    }

    if !changed {
        return None;
    }
    String::from_utf8(output).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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

    #[test]
    fn malformed_urls_are_redacted_lexically() {
        assert_eq!(redact_url("http://user:S3cretPassw0rd@[::1"), "http://[::1");
        assert_eq!(
            redact_url_credentials("connect http://user:S3cretPassw0rd@[::1 failed"),
            "connect http://[::1 failed"
        );
        assert_eq!(
            redact_url_credentials("(http://user:S3cretPassw0rd@[::1)"),
            "(http://[::1)"
        );
    }

    #[test]
    fn environment_secrets_include_url_password_and_skip_short_values() {
        let secrets = collect_secrets([
            (
                "DATABASE_URL",
                "postgres://tasklerq:p%40ssword@db:5432/tasklerq",
            ),
            ("TELEGRAM_BOT_TOKEN", "123456:secret-token-value"),
            ("HA_TOKEN", "abc"),
            ("TELEGRAM_WEBHOOK_SECRET", ""),
        ]);

        assert!(secrets.iter().any(|secret| secret == "p%40ssword"));
        assert!(
            secrets.iter().any(|secret| secret == "p@ssword"),
            "the percent-decoded password must also be collected: {secrets:?}"
        );
        assert!(
            secrets
                .iter()
                .any(|secret| secret == "123456:secret-token-value")
        );
        assert!(
            !secrets.iter().any(|secret| secret == "abc"),
            "short values must not be replaced globally"
        );

        let text = "db p%40ssword, decoded p@ssword, token 123456:secret-token-value";
        let redacted = replace_known_secrets(text, &secrets);
        assert!(!redacted.contains("p%40ssword"), "{redacted}");
        assert!(!redacted.contains("p@ssword"), "{redacted}");
        assert!(
            !redacted.contains("123456:secret-token-value"),
            "{redacted}"
        );
    }
}
