use log::{Level, Metadata, Record};
use nexus::log::{log as nexus_log, LogLevel};

const ADDON_LOG_CHANNEL: &str = "Vloxximity";

pub struct NexusLogger;

impl log::Log for NexusLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let level = match record.level() {
            Level::Error => LogLevel::Critical,
            Level::Warn => LogLevel::Warning,
            Level::Info => LogLevel::Info,
            Level::Debug => LogLevel::Debug,
            Level::Trace => LogLevel::Trace,
        };

        let target = log_channel(record);
        let mut msg = format!("{}", record.args());

        // Nexus converts the message to a CString internally and panics on an
        // interior null byte. Raw MumbleLink buffers (identity, name) are
        // fixed-size UTF-16 arrays padded with `\0`, so debug-formatted
        // messages can occasionally carry one through.
        if msg.contains('\0') {
            msg = msg.replace('\0', "");
        }

        // Send to Nexus logging system. Ignore any errors.
        let _ = nexus_log(level, target, msg);
    }

    fn flush(&self) {}
}

/// Initialize the global logger to forward `log` crate messages to Nexus.
pub fn init() -> Result<(), log::SetLoggerError> {
    log::set_boxed_logger(Box::new(NexusLogger))?;
    log::set_max_level(log::LevelFilter::Debug);
    Ok(())
}

fn log_channel(record: &Record) -> &str {
    if cfg!(debug_assertions) && !record.target().is_empty() {
        record.target()
    } else {
        ADDON_LOG_CHANNEL
    }
}
