//! Криптографически случайные секреты: API-токены и коды приглашений.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// `bytes` случайных байт от ОС в представлении base64url без заполнения.
///
/// Паника при недоступности системного генератора допустима: без него сервис
/// не может безопасно выпускать секреты.
pub fn random_base64url(bytes: usize) -> String {
    let mut buffer = vec![0_u8; bytes];
    getrandom::fill(&mut buffer).expect("operating system random generator is unavailable");
    URL_SAFE_NO_PAD.encode(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_requested_entropy_without_padding() {
        assert_eq!(random_base64url(12).len(), 16);
        assert_eq!(random_base64url(32).len(), 43);
        assert_ne!(random_base64url(32), random_base64url(32));
    }

    #[test]
    fn alphabet_is_url_safe() {
        let value = random_base64url(64);
        assert!(
            value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        );
    }
}
