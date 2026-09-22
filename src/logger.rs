use std::{
    borrow::Cow,
    boxed::Box,
    collections::VecDeque,
    string::{String, ToString},
    sync::{Arc, LazyLock, Mutex},
    vec::Vec,
};

use compact_str::CompactString;
use log::{Level, LevelFilter, Log};

static LOGGER: LazyLock<DebugLogger> = LazyLock::new(DebugLogger::default);

/// The maximum depth of the debug log.
///
/// When reached, older messages are dropped first
const HISTORY_SIZE: usize = 1000;

#[derive(Default)]
struct DebugLoggerImpl {
    inner: Option<Box<dyn Log>>,
    captured: VecDeque<LogEntry>,
}

#[derive(Clone)]
pub struct LogEntry {
    pub level: Level,
    pub target: CompactString,
    pub file: Option<Cow<'static, str>>,
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
        if guard.captured.len() > HISTORY_SIZE {
            guard.captured.pop_front();
        }
        if let Some(inner) = guard.inner.as_ref() {
            inner.log(record);
        }
    }

    fn flush(&self) {}
}

impl DebugLogger {
    /// Returns an error if the global logger was already initialized.
    pub fn install_with_max_level(
        inner: Box<dyn Log>,
        max_level: LevelFilter,
    ) -> Result<(), log::SetLoggerError> {
        let logger = &*LOGGER;
        log::set_logger(logger)?;
        // Update `inner` only if `set_logger` succeeded.
        logger.set_inner(inner);
        log::set_max_level(max_level);
        Ok(())
    }

    pub fn get() -> &'static Self {
        &LOGGER
    }

    pub fn take_captured(&self) -> VecDeque<LogEntry> {
        let mut guard = self.0.lock().unwrap();
        core::mem::take(&mut guard.captured)
    }

    /// Counts the number of log entries in the ring buffer which match `predicate`
    pub fn count_matching<F>(&self, mut predicate: F) -> usize
    where
        F: FnMut(&LogEntry) -> bool,
    {
        let guard = self.0.lock().unwrap();
        guard.captured.iter().filter(move |entry| predicate(entry)).count()
    }

    /// Returns true if any entry in the ring buffer matches `predicate`
    pub fn contains_matching<F>(&self, predicate: F) -> bool
    where
        F: FnMut(&LogEntry) -> bool,
    {
        let guard = self.0.lock().unwrap();
        guard.captured.iter().any(predicate)
    }

    /// Clones all of the log entries in the buffer which match `predicate`.
    pub fn select_matching<F>(&self, mut predicate: F) -> Vec<LogEntry>
    where
        F: FnMut(&LogEntry) -> bool,
    {
        let guard = self.0.lock().unwrap();
        guard
            .captured
            .iter()
            .filter_map(move |entry| {
                if predicate(entry) {
                    Some(entry.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn clone_captured(&self) -> VecDeque<LogEntry> {
        self.0.lock().unwrap().captured.clone()
    }

    fn set_inner(&self, logger: Box<dyn Log>) {
        drop(self.0.lock().unwrap().inner.replace(logger));
    }

    /// Returns an error if the global logger was already initialized.
    pub fn init_for_tests() -> Result<(), log::SetLoggerError> {
        use env_logger::Env;
        let env = Env::new().filter_or("MIDENC_TRACE", "info");
        let mut builder = env_logger::Builder::from_env(env);
        builder.format_indent(Some(2));
        builder.format_timestamp(None);
        Self::install_with_max_level(Box::new(builder.build()), LevelFilter::Trace)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Sink(Arc<AtomicUsize>);

    impl Log for Sink {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= Level::Info
        }

        fn log(&self, _record: &log::Record<'_>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn flush(&self) {}
    }

    #[test]
    fn logger_filters_forwards_bounds_and_queries_captured_records() {
        let logger = DebugLogger::default();
        let metadata = log::Metadata::builder().level(Level::Info).target("test").build();
        assert!(!logger.enabled(&metadata));
        logger
            .log(&log::Record::builder().args(format_args!("ignored")).level(Level::Info).build());
        assert!(logger.clone_captured().is_empty());
        let forwarded = Arc::new(AtomicUsize::new(0));
        logger.set_inner(Box::new(Sink(forwarded.clone())));
        assert!(logger.enabled(&metadata));
        for index in 0..HISTORY_SIZE + 2 {
            logger.log(
                &log::Record::builder()
                    .args(format_args!("entry {index}"))
                    .level(Level::Info)
                    .target("test")
                    .file_static(Some("source.rs"))
                    .line(Some(7))
                    .build(),
            );
        }
        logger
            .log(&log::Record::builder().args(format_args!("ignored")).level(Level::Debug).build());
        assert_eq!(forwarded.load(Ordering::Relaxed), HISTORY_SIZE + 2);
        let captured = logger.clone_captured();
        assert_eq!(captured.len(), HISTORY_SIZE);
        assert_eq!(captured[0].message, "entry 2");
        assert_eq!(captured[0].file.as_deref(), Some("source.rs"));
        assert_eq!(captured[0].line, Some(7));
        assert_eq!(logger.count_matching(|entry| entry.target == "test"), HISTORY_SIZE);
        assert!(logger.contains_matching(|entry| entry.message == "entry 1001"));
        assert_eq!(logger.select_matching(|entry| entry.message == "entry 1001").len(), 1);
        assert!(logger.select_matching(|entry| entry.level == Level::Error).is_empty());
        assert_eq!(logger.take_captured().len(), HISTORY_SIZE);
        assert!(logger.take_captured().is_empty());
        let file = String::from("owned.rs");
        logger.log(
            &log::Record::builder()
                .args(format_args!("owned file"))
                .level(Level::Info)
                .file(Some(&file))
                .build(),
        );
        assert_eq!(logger.take_captured()[0].file.as_deref(), Some("owned.rs"));
        logger.flush();
    }
}
