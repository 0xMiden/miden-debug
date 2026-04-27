use std::{
    borrow::Cow,
    collections::VecDeque,
    sync::{Arc, LazyLock, Mutex, Once},
};

use compact_str::CompactString;
use log::{Level, LevelFilter, Log};

static LOGGER: LazyLock<DebugLogger> = LazyLock::new(DebugLogger::default);
static LOGGER_INSTALLED: Once = Once::new();
const MAX_CAPTURED_LOGS: usize = 1000;

#[derive(Default)]
struct DebugLoggerImpl {
    inner: Option<Box<dyn Log>>,
    captured: VecDeque<LogEntry>,
}

#[derive(Clone)]
pub struct LogEntry {
    pub level: Level,
    #[allow(unused)]
    pub target: CompactString,
    #[allow(unused)]
    pub file: Option<Cow<'static, str>>,
    #[allow(unused)]
    pub line: Option<u32>,
    pub message: String,
}

#[derive(Default, Clone)]
pub struct DebugLogger(Arc<Mutex<DebugLoggerImpl>>);

impl Log for DebugLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        let guard = self.0.lock().unwrap();
        guard.inner.as_ref().is_some_and(|inner| inner.enabled(metadata))
    }

    fn log(&self, record: &log::Record) {
        let mut guard = self.0.lock().unwrap();
        if !guard.inner.as_ref().is_some_and(|inner| inner.enabled(record.metadata())) {
            return;
        }

        let target = CompactString::new(record.target());
        let file = record
            .file_static()
            .map(Cow::Borrowed)
            .or_else(|| record.file().map(|f| f.to_string()).map(Cow::Owned));
        let entry = LogEntry {
            target,
            level: record.level(),
            file,
            line: record.line(),
            message: format!("{}", record.args()),
        };
        guard.captured.push_back(entry);
        if guard.captured.len() > MAX_CAPTURED_LOGS {
            guard.captured.pop_front();
        }
        if let Some(inner) = guard.inner.as_ref() {
            inner.log(record);
        }
    }

    fn flush(&self) {}
}

impl DebugLogger {
    pub fn install_with_max_level(inner: Box<dyn Log>, max_level: LevelFilter) {
        let logger = &*LOGGER;
        logger.set_inner(inner);
        LOGGER_INSTALLED.call_once(|| {
            log::set_logger(logger).unwrap_or_else(|err| panic!("failed to install logger: {err}"));
            log::set_max_level(max_level);
        });
    }

    pub fn get() -> &'static Self {
        &LOGGER
    }

    pub fn take_captured(&self) -> VecDeque<LogEntry> {
        let mut guard = self.0.lock().unwrap();
        core::mem::take(&mut guard.captured)
    }

    pub fn peek_captured(&self) -> VecDeque<LogEntry> {
        self.0.lock().unwrap().captured.clone()
    }

    fn set_inner(&self, logger: Box<dyn Log>) {
        drop(self.0.lock().unwrap().inner.replace(logger));
    }

    // Tests share a global logger, so one test may observe logs emitted by another test.
    pub fn init_for_tests() {
        let mut builder = env_logger::Builder::from_env("MIDENC_TRACE");
        builder.format_indent(Some(2));
        builder.format_timestamp(None);
        Self::install_with_max_level(Box::new(builder.build()), LevelFilter::Trace);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::DebugLogger;

    static NEXT_LOG_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn test_logger_captures_logs() {
        DebugLogger::init_for_tests();

        let before = DebugLogger::get().peek_captured().len();
        let id = NEXT_LOG_ID.fetch_add(1, Ordering::Relaxed);
        let expected = format!("logger test message {id}");
        log::info!("{expected}");

        let captured = DebugLogger::get().peek_captured();
        assert!(captured.iter().skip(before).any(|entry| entry.message == expected));
    }
}
