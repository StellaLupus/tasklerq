//! Входящие обновления: разбор в доменную форму.
//!
//! Из всего многообразия типов `teloxide-core` боту нужны два: текст в личном
//! чате и нажатие кнопки. Остальное игнорируется, а данные групп не
//! сохраняются.

use teloxide_core::types::{
    ChatKind, MaybeInaccessibleMessage, Message, Update, UpdateKind, User as TelegramUser,
};

/// Отправитель обновления.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sender {
    /// Telegram user id; в личном чате равен chat id.
    pub id: i64,
    /// Имя для отображения.
    pub display_name: String,
    /// Telegram username.
    pub username: Option<String>,
}

impl Sender {
    fn from_user(user: &TelegramUser) -> Self {
        let mut name = user.first_name.trim().to_owned();
        if let Some(last) = user.last_name.as_deref().map(str::trim)
            && !last.is_empty()
        {
            name.push(' ');
            name.push_str(last);
        }
        if name.is_empty() {
            name = user
                .username
                .clone()
                .unwrap_or_else(|| "Пользователь".to_owned());
        }
        Self {
            id: i64::try_from(user.id.0).unwrap_or(i64::MAX),
            display_name: name,
            username: user.username.clone(),
        }
    }
}

/// Обновление, понятное боту.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// Текстовое сообщение в личном чате.
    Text {
        /// Чат.
        chat_id: i64,
        /// Отправитель.
        sender: Sender,
        /// Идентификатор сообщения пользователя.
        message_id: i64,
        /// Текст.
        text: String,
    },
    /// Нажатие кнопки.
    Callback {
        /// Идентификатор callback-запроса для `answerCallbackQuery`.
        query_id: String,
        /// Отправитель.
        sender: Sender,
        /// Чат сообщения с кнопкой.
        chat_id: i64,
        /// Сообщение с кнопкой.
        message_id: i64,
        /// `callback_data`.
        data: String,
    },
}

/// Что делать с обновлением, поступившим на webhook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classified {
    /// Обработать.
    Handle(Incoming),
    /// Игнорировать без ошибки и без сохранения (группы, каналы, прочие типы).
    Ignore,
}

/// Классифицировать обновление.
pub fn classify(update: &Update) -> Classified {
    match &update.kind {
        UpdateKind::Message(message) => classify_message(message),
        UpdateKind::CallbackQuery(query) => {
            let Some(message) = &query.message else {
                return Classified::Ignore;
            };
            if !is_private(&message.chat().kind) {
                return Classified::Ignore;
            }
            let Some(data) = query.data.clone() else {
                return Classified::Ignore;
            };
            let message_id = match message {
                MaybeInaccessibleMessage::Regular(message) => i64::from(message.id.0),
                MaybeInaccessibleMessage::Inaccessible(_) => i64::from(message.id().0),
            };
            Classified::Handle(Incoming::Callback {
                query_id: query.id.0.clone(),
                sender: Sender::from_user(&query.from),
                chat_id: message.chat().id.0,
                message_id,
                data,
            })
        }
        _ => Classified::Ignore,
    }
}

fn classify_message(message: &Message) -> Classified {
    if !is_private(&message.chat.kind) {
        return Classified::Ignore;
    }
    let (Some(text), Some(from)) = (message.text(), message.from.as_ref()) else {
        return Classified::Ignore;
    };
    if from.is_bot {
        return Classified::Ignore;
    }
    Classified::Handle(Incoming::Text {
        chat_id: message.chat.id.0,
        sender: Sender::from_user(from),
        message_id: i64::from(message.id.0),
        text: text.to_owned(),
    })
}

fn is_private(kind: &ChatKind) -> bool {
    matches!(kind, ChatKind::Private(_))
}

/// Разобрать сырой JSON обновления.
///
/// `Update` из `teloxide-core` использует `#[serde(flatten)]`, который при
/// десериализации из `serde_json::Value` теряет целочисленные поля сообщения,
/// поэтому разбор идёт из строки.
pub fn parse(payload: &serde_json::Value) -> Result<Update, serde_json::Error> {
    serde_json::from_str(&payload.to_string())
}

/// Разобрать обновление из тела запроса.
pub fn parse_bytes(body: &[u8]) -> Result<Update, serde_json::Error> {
    serde_json::from_slice(body)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn private_text() -> serde_json::Value {
        json!({
            "update_id": 10,
            "message": {
                "message_id": 5,
                "date": 1_700_000_000,
                "chat": {"id": 42, "type": "private", "first_name": "Ann"},
                "from": {"id": 42, "is_bot": false, "first_name": "Ann", "last_name": "Lee"},
                "text": "/start"
            }
        })
    }

    #[test]
    fn private_text_is_handled() {
        let update = parse(&private_text()).expect("update");
        let Classified::Handle(Incoming::Text {
            chat_id,
            sender,
            text,
            ..
        }) = classify(&update)
        else {
            panic!("text expected");
        };
        assert_eq!(chat_id, 42);
        assert_eq!(sender.display_name, "Ann Lee");
        assert_eq!(text, "/start");
    }

    #[test]
    fn group_messages_are_ignored() {
        let mut payload = private_text();
        payload["message"]["chat"] = json!({"id": -100, "type": "supergroup", "title": "Семья"});
        let update = parse(&payload).expect("update");
        assert_eq!(classify(&update), Classified::Ignore);
    }

    #[test]
    fn callback_in_private_chat_is_handled() {
        let payload = json!({
            "update_id": 11,
            "callback_query": {
                "id": "cb1",
                "chat_instance": "x",
                "from": {"id": 42, "is_bot": false, "first_name": "Ann"},
                "data": "t:AAAAAAAAAAAAAAAA",
                "message": {
                    "message_id": 9,
                    "date": 1_700_000_000,
                    "chat": {"id": 42, "type": "private", "first_name": "Ann"},
                    "text": "экран"
                }
            }
        });
        let update = parse(&payload).expect("update");
        let Classified::Handle(Incoming::Callback {
            query_id,
            message_id,
            data,
            ..
        }) = classify(&update)
        else {
            panic!("callback expected");
        };
        assert_eq!((query_id.as_str(), message_id), ("cb1", 9));
        assert_eq!(data, "t:AAAAAAAAAAAAAAAA");
    }

    #[test]
    fn messages_from_bots_and_without_text_are_ignored() {
        let mut payload = private_text();
        payload["message"]["from"]["is_bot"] = json!(true);
        let update = parse(&payload).expect("update");
        assert_eq!(classify(&update), Classified::Ignore);

        let mut payload = private_text();
        payload["message"]
            .as_object_mut()
            .expect("object")
            .remove("text");
        let update = parse(&payload).expect("update");
        assert_eq!(classify(&update), Classified::Ignore);
    }
}
