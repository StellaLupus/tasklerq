//! Писатель журнала в память для подписчика `tracing`.

use std::sync::{Arc, Mutex};

/// Писатель журнала в память.
#[derive(Clone, Default)]
pub struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    /// Захваченный журнал целиком.
    pub fn contents(&self) -> String {
        let bytes = self.0.lock().expect("log buffer lock").clone();
        String::from_utf8(bytes).expect("logs are UTF-8")
    }
}

impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogBuffer {
    type Writer = Self;

    fn make_writer(&'writer self) -> Self::Writer {
        self.clone()
    }
}
