//! Шифрование секретных настроек: XChaCha20-Poly1305 с привязкой к строке.
//!
//! Шифротекст (формат из design, решение 8) состоит из версии формата (1 байт),
//! id ключа (8 байт — первые байты SHA-256 ключа), случайного nonce (24 байта) и
//! шифротекста с тегом Poly1305. AAD — версия и поля строки `settings` с
//! префиксами длины, поэтому перенос шифротекста в другую строку не
//! расшифровывается.
//!
//! Шифрование всегда идёт текущим ключом, расшифровка — текущим или прежним
//! (`TASKLERQ_SECRET_KEY_OLD`) по id из шифротекста. [`SecretError`] и `Debug`
//! не содержат ни ключа, ни открытого текста.

use std::fmt;

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{Key, KeyInit, XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::config::{Config, SecretKey, SecretString};

/// Текущая версия формата шифротекста.
pub const FORMAT_VERSION: u8 = 1;

/// Длина id ключа в шифротексте.
const KEY_ID_LEN: usize = 8;
/// Длина nonce XChaCha20-Poly1305.
const NONCE_LEN: usize = 24;
/// Длина тега Poly1305.
const TAG_LEN: usize = 16;
/// Длина заголовка: версия, id ключа, nonce.
const HEADER_LEN: usize = 1 + KEY_ID_LEN + NONCE_LEN;

/// Идентификатор ключа шифрования: первые байты SHA-256 ключа.
///
/// Лежит в шифротексте открыто: по нему выбирается ключ при расшифровке и
/// строки для перешифровки при ротации.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyId([u8; KEY_ID_LEN]);

impl KeyId {
    /// Идентификатор ключа.
    pub fn of(key: &SecretKey) -> Self {
        let digest = Sha256::digest(key.expose());
        let mut id = [0_u8; KEY_ID_LEN];
        id.copy_from_slice(&digest[..KEY_ID_LEN]);
        Self(id)
    }

    /// Байты идентификатора в том виде, в каком они лежат в шифротексте.
    pub const fn as_bytes(&self) -> &[u8; KEY_ID_LEN] {
        &self.0
    }
}

impl fmt::Debug for KeyId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("KeyId(")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        formatter.write_str(")")
    }
}

/// Уровень строки `settings`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingScope {
    /// Настройка всей инсталляции.
    Installation,
    /// Настройка одного household.
    Household,
}

impl SettingScope {
    /// Значение колонки `settings.scope`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Installation => "installation",
            Self::Household => "household",
        }
    }
}

/// Строка `settings`, к которой привязан шифротекст.
///
/// `household_id` есть только у уровня [`SettingScope::Household`], как и
/// требует CHECK таблицы.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretContext {
    scope: SettingScope,
    household_id: Option<Uuid>,
    key: String,
}

impl SecretContext {
    /// Контекст настройки всей инсталляции.
    pub fn installation(key: impl Into<String>) -> Self {
        Self {
            scope: SettingScope::Installation,
            household_id: None,
            key: key.into(),
        }
    }

    /// Контекст настройки одного household.
    pub fn household(household_id: Uuid, key: impl Into<String>) -> Self {
        Self {
            scope: SettingScope::Household,
            household_id: Some(household_id),
            key: key.into(),
        }
    }

    /// Уровень строки.
    pub const fn scope(&self) -> SettingScope {
        self.scope
    }

    /// Household строки; `None` для настройки инсталляции.
    pub const fn household_id(&self) -> Option<Uuid> {
        self.household_id
    }

    /// Имя настройки (`settings.key`).
    pub fn key(&self) -> &str {
        &self.key
    }

    /// AAD: версия формата и поля строки с префиксами длины.
    fn aad(&self) -> Vec<u8> {
        associated_data(
            FORMAT_VERSION,
            self.scope.as_str().as_bytes(),
            self.household_id
                .as_ref()
                .map(|id| id.as_bytes().as_slice()),
            self.key.as_bytes(),
        )
    }
}

/// Собрать AAD из версии формата и полей строки.
///
/// Версия — фиксированный первый байт, префикс длины ей не нужен. Каждый
/// компонент полей предваряется длиной `u32` big-endian: без префиксов границы
/// полей терялись бы и разные строки могли бы дать одинаковый AAD. Отсутствующий
/// `household_id` кодируется длиной 0, UUID — длиной 16, поэтому эти случаи
/// различимы всегда.
fn associated_data(version: u8, scope: &[u8], household_id: Option<&[u8]>, key: &[u8]) -> Vec<u8> {
    let mut aad = Vec::new();
    aad.push(version);
    push_component(&mut aad, scope);
    push_component(&mut aad, household_id.unwrap_or_default());
    push_component(&mut aad, key);
    aad
}

/// Дописать компонент AAD: длину `u32` big-endian и сами байты.
fn push_component(buffer: &mut Vec<u8>, value: &[u8]) {
    let length = u32::try_from(value.len()).expect("AAD component longer than u32::MAX bytes");
    buffer.extend_from_slice(&length.to_be_bytes());
    buffer.extend_from_slice(value);
}

/// Id ключа из заголовка шифротекста без расшифровки.
///
/// `None` — буфер короче поля id. По этому id при ротации выбираются строки,
/// зашифрованные прежним ключом.
pub fn key_id(blob: &[u8]) -> Option<KeyId> {
    let header = blob.get(1..1 + KEY_ID_LEN)?;
    let mut id = [0_u8; KEY_ID_LEN];
    id.copy_from_slice(header);
    Some(KeyId(id))
}

/// Ошибка шифрования или расшифровки секрета.
///
/// Не содержит ни открытого текста, ни ключа.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SecretError {
    /// Ключ шифрования не задан: секрет нельзя ни зашифровать, ни прочитать.
    #[error("secret key is not configured")]
    KeyMissing,
    /// Шифротекст не расшифровывается текущим или прежним ключом.
    #[error("secret cannot be decrypted")]
    Undecryptable,
}

/// Шифровальщик секретов: текущий ключ и необязательный прежний.
#[derive(Clone, Debug)]
pub struct SecretCipher {
    current: Option<SecretKey>,
    old: Option<SecretKey>,
}

impl SecretCipher {
    /// Шифровальщик из ключей конфигурации; любой из них может отсутствовать.
    pub fn new(current: Option<SecretKey>, old: Option<SecretKey>) -> Self {
        Self { current, old }
    }

    /// Шифровальщик из [`Config`].
    pub fn from_config(config: &Config) -> Self {
        Self::new(config.secret_key.clone(), config.secret_key_old.clone())
    }

    /// Id текущего ключа; `None` — текущего ключа нет.
    pub fn current_key_id(&self) -> Option<KeyId> {
        self.current.as_ref().map(KeyId::of)
    }

    /// Зашифровать значение текущим ключом.
    ///
    /// Без текущего ключа возвращает [`SecretError::KeyMissing`]: запись
    /// секрета должна быть отклонена с объяснением.
    pub fn encrypt(
        &self,
        context: &SecretContext,
        plaintext: &SecretString,
    ) -> Result<Vec<u8>, SecretError> {
        let key = self.current.as_ref().ok_or(SecretError::KeyMissing)?;
        let key_id = KeyId::of(key);
        let aad = context.aad();
        let mut nonce = [0_u8; NONCE_LEN];
        getrandom::fill(&mut nonce).expect("operating system random generator is unavailable");

        let cipher = XChaCha20Poly1305::new(&Key::from(*key.expose()));
        let ciphertext = cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: plaintext.expose().as_bytes(),
                    aad: &aad,
                },
            )
            .expect("AEAD limits are far above any setting secret");

        let mut blob = Vec::with_capacity(HEADER_LEN + ciphertext.len());
        blob.push(FORMAT_VERSION);
        blob.extend_from_slice(key_id.as_bytes());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ciphertext);
        Ok(blob)
    }

    /// Расшифровать значение ключом, id которого лежит в шифротексте.
    ///
    /// Без текущего и прежнего ключа возвращает [`SecretError::KeyMissing`];
    /// неизвестная версия, короткий или изменённый буфер и чужой id —
    /// [`SecretError::Undecryptable`].
    pub fn decrypt(
        &self,
        context: &SecretContext,
        blob: &[u8],
    ) -> Result<SecretString, SecretError> {
        if self.current.is_none() && self.old.is_none() {
            return Err(SecretError::KeyMissing);
        }
        let blob_key_id = key_id(blob).ok_or(SecretError::Undecryptable)?;
        if blob[0] != FORMAT_VERSION {
            return Err(SecretError::Undecryptable);
        }
        if blob.len() < HEADER_LEN + TAG_LEN {
            return Err(SecretError::Undecryptable);
        }
        let key = self
            .key_with_id(blob_key_id)
            .ok_or(SecretError::Undecryptable)?;

        let mut nonce = [0_u8; NONCE_LEN];
        nonce.copy_from_slice(&blob[1 + KEY_ID_LEN..HEADER_LEN]);
        let ciphertext = &blob[HEADER_LEN..];

        let aad = context.aad();
        let cipher = XChaCha20Poly1305::new(&Key::from(*key.expose()));
        let plaintext = cipher
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| SecretError::Undecryptable)?;
        let plaintext = String::from_utf8(plaintext).map_err(|_| SecretError::Undecryptable)?;
        Ok(SecretString::new(plaintext))
    }

    /// Ключ с заданным id: текущий или прежний.
    fn key_with_id(&self, id: KeyId) -> Option<&SecretKey> {
        [self.current.as_ref(), self.old.as_ref()]
            .into_iter()
            .flatten()
            .find(|key| KeyId::of(key) == id)
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use uuid::Uuid;

    use super::*;

    const PLAINTEXT: &str = "SECRET-slot-token-1234";

    /// Ключ, все 32 байта которого равны `byte`.
    fn key(byte: u8) -> SecretKey {
        SecretKey::parse(&BASE64.encode([byte; 32])).expect("32 bytes decode into a key")
    }

    fn cipher(current: u8, old: Option<u8>) -> SecretCipher {
        SecretCipher::new(Some(key(current)), old.map(key))
    }

    fn context() -> SecretContext {
        SecretContext::installation("llm.primary.token")
    }

    fn encrypted(cipher: &SecretCipher, context: &SecretContext) -> Vec<u8> {
        cipher
            .encrypt(context, &SecretString::new(PLAINTEXT))
            .expect("encryption with a current key succeeds")
    }

    #[test]
    fn round_trip_returns_the_same_secret() {
        let cipher = cipher(1, None);
        let blob = encrypted(&cipher, &context());

        let decrypted = cipher.decrypt(&context(), &blob).expect("round trip");

        assert_eq!(decrypted.expose(), PLAINTEXT);
    }

    #[test]
    fn each_encryption_uses_a_fresh_nonce() {
        let cipher = cipher(1, None);
        let first = encrypted(&cipher, &context());
        let second = encrypted(&cipher, &context());

        assert_ne!(first, second);
        assert_ne!(
            first[1 + KEY_ID_LEN..HEADER_LEN],
            second[1 + KEY_ID_LEN..HEADER_LEN]
        );
    }

    #[test]
    fn another_key_cannot_decrypt() {
        let blob = encrypted(&cipher(1, None), &context());

        assert_eq!(
            cipher(2, None).decrypt(&context(), &blob),
            Err(SecretError::Undecryptable)
        );
    }

    #[test]
    fn rotation_reads_with_the_old_key_and_writes_with_the_new_one() {
        let blob = encrypted(&cipher(1, None), &context());

        let rotated = cipher(2, Some(1));
        let decrypted = rotated.decrypt(&context(), &blob).expect("old key reads");
        assert_eq!(decrypted.expose(), PLAINTEXT);

        let reencrypted = encrypted(&rotated, &context());
        assert_eq!(key_id(&reencrypted), Some(KeyId::of(&key(2))));
        assert_eq!(key_id(&blob), Some(KeyId::of(&key(1))));

        assert_eq!(
            cipher(2, None).decrypt(&context(), &blob),
            Err(SecretError::Undecryptable)
        );
    }

    #[test]
    fn a_missing_current_key_refuses_encryption_but_the_old_key_still_decrypts() {
        let blob = encrypted(&cipher(1, None), &context());
        let old_only = SecretCipher::new(None, Some(key(1)));

        assert_eq!(
            old_only.encrypt(&context(), &SecretString::new(PLAINTEXT)),
            Err(SecretError::KeyMissing)
        );
        assert_eq!(
            old_only
                .decrypt(&context(), &blob)
                .expect("old key reads")
                .expose(),
            PLAINTEXT
        );
    }

    #[test]
    fn without_any_key_encryption_and_decryption_report_the_missing_key() {
        let blob = encrypted(&cipher(1, None), &context());
        let empty = SecretCipher::new(None, None);

        assert_eq!(
            empty.encrypt(&context(), &SecretString::new(PLAINTEXT)),
            Err(SecretError::KeyMissing)
        );
        assert_eq!(
            empty.decrypt(&context(), &blob),
            Err(SecretError::KeyMissing)
        );
    }

    #[test]
    fn ciphertext_is_bound_to_its_row() {
        let cipher = cipher(1, None);
        let household = Uuid::from_u64_pair(1, 2);
        let other_household = Uuid::from_u64_pair(3, 4);
        let original = SecretContext::installation("llm.primary.token");
        let blob = encrypted(&cipher, &original);

        for moved in [
            SecretContext::installation("llm.fallback.token"),
            SecretContext::household(household, "llm.primary.token"),
        ] {
            assert_eq!(
                cipher.decrypt(&moved, &blob),
                Err(SecretError::Undecryptable),
                "ciphertext must not move to {moved:?}"
            );
        }

        let household_blob = encrypted(
            &cipher,
            &SecretContext::household(household, "llm.primary.token"),
        );
        for moved in [
            SecretContext::installation("llm.primary.token"),
            SecretContext::household(other_household, "llm.primary.token"),
        ] {
            assert_eq!(
                cipher.decrypt(&moved, &household_blob),
                Err(SecretError::Undecryptable),
                "ciphertext must not move to {moved:?}"
            );
        }
    }

    #[test]
    fn every_altered_byte_is_rejected() {
        let cipher = cipher(1, None);
        let blob = encrypted(&cipher, &context());

        for (index, byte) in blob.iter().enumerate() {
            let mut altered = blob.clone();
            altered[index] = byte ^ 0x01;

            assert_eq!(
                cipher.decrypt(&context(), &altered),
                Err(SecretError::Undecryptable),
                "byte {index} must be authenticated"
            );
        }
    }

    #[test]
    fn truncated_and_empty_buffers_are_rejected_without_panic() {
        let cipher = cipher(1, None);
        let blob = encrypted(&cipher, &context());

        for end in 0..blob.len() {
            assert_eq!(
                cipher.decrypt(&context(), &blob[..end]),
                Err(SecretError::Undecryptable),
                "buffer truncated to {end} bytes must be rejected"
            );
        }
        assert_eq!(
            cipher.decrypt(&context(), &[]),
            Err(SecretError::Undecryptable)
        );
    }

    #[test]
    fn aad_does_not_allow_ambiguous_concatenation() {
        // Без префиксов длины `"ab" + "c"` и `"a" + "bc"` совпали бы.
        assert_ne!(
            associated_data(FORMAT_VERSION, b"ab", None, b"c"),
            associated_data(FORMAT_VERSION, b"a", None, b"bc")
        );
        // Отсутствующий household — длина 0, а не пустая строка без границы.
        assert_ne!(
            associated_data(FORMAT_VERSION, b"installation", None, b"ab"),
            associated_data(
                FORMAT_VERSION,
                b"installation",
                Some(Uuid::nil().as_bytes().as_slice()),
                b"ab"
            )
        );
    }

    #[test]
    fn aad_binds_the_scope_even_with_the_same_household_and_key() {
        let household = Uuid::from_u64_pair(1, 2);
        assert_ne!(
            associated_data(
                FORMAT_VERSION,
                SettingScope::Installation.as_str().as_bytes(),
                Some(household.as_bytes().as_slice()),
                b"llm.primary.token"
            ),
            associated_data(
                FORMAT_VERSION,
                SettingScope::Household.as_str().as_bytes(),
                Some(household.as_bytes().as_slice()),
                b"llm.primary.token"
            ),
            "scope must be part of the AAD"
        );
    }

    #[test]
    fn context_aad_has_the_expected_byte_layout() {
        // Собрано независимо от `associated_data`: версия, затем каждое поле
        // с префиксом длины u32 big-endian (scope, household_id, key).
        let mut installation = vec![FORMAT_VERSION, 0, 0, 0, 12];
        installation.extend_from_slice(b"installation");
        installation.extend_from_slice(&[0, 0, 0, 0]);
        installation.extend_from_slice(&[0, 0, 0, 17]);
        installation.extend_from_slice(b"llm.primary.token");

        assert_eq!(
            SecretContext::installation("llm.primary.token").aad(),
            installation
        );

        let household_id = Uuid::from_u64_pair(0x0102_0304_0506_0708, 0x090a_0b0c_0d0e_0f10);
        let mut household = vec![FORMAT_VERSION, 0, 0, 0, 9];
        household.extend_from_slice(b"household");
        household.extend_from_slice(&[0, 0, 0, 16]);
        household.extend_from_slice(household_id.as_bytes());
        household.extend_from_slice(&[0, 0, 0, 17]);
        household.extend_from_slice(b"llm.primary.token");

        assert_eq!(
            SecretContext::household(household_id, "llm.primary.token").aad(),
            household
        );
    }

    #[test]
    fn key_id_is_read_from_the_header_without_decryption() {
        let current = cipher(1, None);
        let blob = encrypted(&current, &context());

        assert_eq!(key_id(&blob), Some(KeyId::of(&key(1))));
        assert_eq!(current.current_key_id(), Some(KeyId::of(&key(1))));
        assert_eq!(key_id(&blob[..KEY_ID_LEN]), None);
        assert_eq!(key_id(&[]), None);
    }

    #[test]
    fn debug_output_does_not_leak_keys_or_plaintext() {
        let current = key(7);
        let old = key(9);
        let cipher = SecretCipher::new(Some(current.clone()), Some(old.clone()));

        let debug = format!("{cipher:?}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(!debug.contains(PLAINTEXT), "{debug}");
        for key in [&current, &old] {
            assert!(
                !debug.contains(&format!("{:?}", key.expose())),
                "key bytes must not be printed: {debug}"
            );
            assert!(
                !debug.contains("[7, 7") && !debug.contains("[9, 9"),
                "key bytes must not be printed as a decimal list: {debug}"
            );
        }

        for error in [SecretError::KeyMissing, SecretError::Undecryptable] {
            let debug = format!("{error:?}");
            assert!(!debug.contains(PLAINTEXT), "{debug}");
            assert!(
                !debug.contains(&format!("{:?}", current.expose())),
                "key bytes must not be printed: {debug}"
            );
        }
    }
}
