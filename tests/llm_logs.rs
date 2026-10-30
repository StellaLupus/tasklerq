//! Проверка, что журнал LLM-разбора не содержит исходный текст.
//!
//! Тест живёт в отдельном файле: `tracing` кеширует интерес к callsite, и в
//! общем тестовом бинарнике параллельные тесты успевают отключить его до того,
//! как здесь появится подписчик. Один тест на процесс делает перехват
//! детерминированным.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use serde_json::Value;
use tasklerq::home_assistant::{AiTask, GenerateData, HaError};
use tasklerq::telegram::bot::Bot;
use tasklerq::telegram::capture_worker::{CaptureWorker, Pass};
use tokio::sync::Notify;

use support::telegram::{FakeTelegram, Harness};
use support::{db, next_telegram_id};

/// Home Assistant, который всегда отвечает некорректно.
struct FailingAi {
    calls: AtomicUsize,
}

#[async_trait]
impl AiTask for FailingAi {
    async fn check_entity(&self) -> Result<(), HaError> {
        Ok(())
    }

    async fn generate_data(
        &self,
        _request: &GenerateData<'_>,
        _timeout: StdDuration,
    ) -> Result<Value, HaError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(HaError::BadResponse)
    }
}

/// Писатель журнала в память.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter(self.0.clone())
    }
}

#[test]
fn llm_logs_do_not_contain_the_source_text() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let db = db().await;
            let id = next_telegram_id();
            let handler = Bot::new("tasklerqbot").with_llm_capture(true);
            let ai = Arc::new(FailingAi {
                calls: AtomicUsize::new(0),
            });
            let mut bot =
                Harness::with_handler(&db.pool, id, Arc::new(handler.clone()), FakeTelegram::new());
            bot.text("/start").await;
            bot.text("Семья").await;
            bot.press("Москва").await;
            bot.press("меню").await;

            let worker = CaptureWorker::new(
                db.pool.clone(),
                ai.clone(),
                handler,
                Arc::new(Notify::new()),
            )
            .only_chat(id);
            bot.text("секретная заметка про подарок").await;
            assert_eq!(worker.run_once(bot.now).await.expect("pass"), Pass::Dead);
            assert_eq!(ai.calls.load(Ordering::SeqCst), 1);
        });
    });

    let captured = logs.0.lock().expect("lock").clone();
    let captured = String::from_utf8(captured).expect("utf8");
    assert!(
        captured.contains("llm capture request failed"),
        "the failure must be logged:\n{captured}"
    );
    assert!(
        !captured.contains("секретная заметка") && !captured.contains("подарок"),
        "the source text must not reach the logs:\n{captured}"
    );
}
