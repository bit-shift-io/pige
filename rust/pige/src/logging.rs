//! Process-wide logging, shared by the desktop and Android shells.
//!
//! One [`tracing_subscriber`] captures every source: this crate and its
//! dependencies log through `log`, while the vendored Proton transport reports
//! wire activity through `tracing` (request, status, timing - no secrets; SRP
//! never sends the password). Desktop formats to stdout. Android writes the
//! same lines to logcat under the `pige` tag with the record's priority.
//!
//! `RUST_LOG` selects the filter where a shell can set it; Android falls back
//! to a verbose default so a release build still yields Proton wire evidence
//! (`adb logcat -s pige`) while reproducing a provider bug.

use tracing_subscriber::EnvFilter;

/// The logcat tag every Android record carries; `adb logcat -s pige` shows
/// the app's full pipeline.
#[cfg(target_os = "android")]
const TAG: &str = "pige";

/// Filter used when `RUST_LOG` is unset. Desktop stays quiet; Android opts
/// into Proton wire logs because there is no env to opt in with.
#[cfg(not(target_os = "android"))]
const DEFAULT_FILTER: &str = "warn";
#[cfg(target_os = "android")]
const DEFAULT_FILTER: &str = "info,proton_core=debug";

/// Install the subscriber once. Later calls (the JNI entry points race on
/// cold start) are no-ops.
pub(crate) fn init() {
    #[cfg(target_os = "android")]
    init_android();
    #[cfg(not(target_os = "android"))]
    init_desktop();
}

#[cfg(not(target_os = "android"))]
fn init_desktop() {
    tracing_subscriber::fmt()
        .with_env_filter(filter())
        .with_ansi(false)
        .try_init()
        .ok();
}

#[cfg(target_os = "android")]
fn init_android() {
    tracing_subscriber::fmt()
        .with_env_filter(filter())
        .with_ansi(false)
        .with_writer(android::AndroidLogWriter)
        .try_init()
        .ok();
}

fn filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

/// Buffer written bytes and hand back complete newline-terminated lines.
///
/// Logcat wants one line per record, while the formatter may deliver one
/// event across several `write` calls (and multi-line for pretty errors), so
/// the Android writer splits on newline boundaries and flushes the remainder.
#[cfg(any(target_os = "android", test))]
#[derive(Default)]
struct LineBuffer {
    text: String,
}

#[cfg(any(target_os = "android", test))]
impl LineBuffer {
    /// Consume `bytes`, returning every line completed by them.
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.text.push_str(&String::from_utf8_lossy(bytes));
        let mut lines = Vec::new();
        while let Some(end) = self.text.find('\n') {
            let line: String = self.text.drain(..=end).collect();
            lines.push(trim_line(&line));
        }
        lines
    }

    /// Return and clear any unterminated remainder.
    fn flush(&mut self) -> Option<String> {
        if self.text.is_empty() {
            return None;
        }
        let remainder = std::mem::take(&mut self.text);
        Some(trim_line(&remainder))
    }
}

#[cfg(any(target_os = "android", test))]
fn trim_line(line: &str) -> String {
    line.trim_end_matches(['\n', '\r']).to_string()
}

#[cfg(target_os = "android")]
mod android {
    use std::ffi::CString;
    use std::io::{self, Write};

    use tracing::Level;
    use tracing_subscriber::fmt::MakeWriter;

    use super::{LineBuffer, TAG};

    /// Map a record level onto logcat's priority scale.
    fn priority(level: &Level) -> i32 {
        use android_log_sys::LogPriority;
        match *level {
            Level::ERROR => LogPriority::ERROR as i32,
            Level::WARN => LogPriority::WARN as i32,
            Level::INFO => LogPriority::INFO as i32,
            Level::DEBUG => LogPriority::DEBUG as i32,
            Level::TRACE => LogPriority::VERBOSE as i32,
        }
    }

    /// Write one logcat line. Long messages are cut to a UTF-8-safe prefix of
    /// logcat's ~4 KB line budget so a huge record stays visible, not dropped.
    fn write_line(priority: i32, line: &str) {
        if line.is_empty() {
            return;
        }
        const MAX_MESSAGE_BYTES: usize = 3500;
        let Ok(tag) = CString::new(TAG) else {
            return;
        };
        let mut end = line.len().min(MAX_MESSAGE_BYTES);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        let Ok(message) = CString::new(&line[..end]) else {
            return;
        };
        // SAFETY: both pointers are valid NUL-terminated C strings for the
        // duration of the call, which copies them into the log buffer.
        unsafe {
            android_log_sys::__android_log_write(priority, tag.as_ptr(), message.as_ptr());
        }
    }

    /// A writer for one formatted record: buffers the event's bytes and emits
    /// each completed line to logcat at the event's priority.
    pub(super) struct AndroidWriter {
        priority: i32,
        buffer: LineBuffer,
    }

    impl Write for AndroidWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            for line in self.buffer.push(bytes) {
                write_line(self.priority, &line);
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if let Some(line) = self.buffer.flush() {
                write_line(self.priority, &line);
            }
            Ok(())
        }
    }

    /// Hand the formatter a fresh writer per record, carrying the record's
    /// level so logcat filtering (`adb logcat pige:W`) still works.
    pub(super) struct AndroidLogWriter;

    impl<'a> MakeWriter<'a> for AndroidLogWriter {
        type Writer = AndroidWriter;

        fn make_writer(&'a self) -> Self::Writer {
            Self::writer_for(&Level::INFO)
        }

        fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
            Self::writer_for(meta.level())
        }
    }

    impl AndroidLogWriter {
        fn writer_for(level: &Level) -> AndroidWriter {
            AndroidWriter {
                priority: priority(level),
                buffer: LineBuffer::default(),
            }
        }
    }
}

#[cfg(all(test, not(target_os = "android")))]
mod tests {
    use super::*;

    #[test]
    fn lines_complete_across_partial_writes() {
        let mut buffer = LineBuffer::default();
        assert_eq!(buffer.push(b"pro"), Vec::<String>::new());
        assert_eq!(buffer.push(b"ton wire\nnext"), vec!["proton wire"]);
        assert_eq!(buffer.flush().as_deref(), Some("next"));
        assert_eq!(buffer.flush(), None);
    }

    #[test]
    fn multi_line_records_yield_one_line_each() {
        let mut buffer = LineBuffer::default();
        let lines = buffer.push(b"first\nsecond\n");
        assert_eq!(lines, vec!["first", "second"]);
        assert_eq!(buffer.flush(), None);
    }

    #[test]
    fn carriage_returns_are_trimmed() {
        let mut buffer = LineBuffer::default();
        assert_eq!(buffer.push(b"crlf\r\n"), vec!["crlf"]);
    }

    #[test]
    fn invalid_utf8_is_lossily_kept() {
        let mut buffer = LineBuffer::default();
        let lines = buffer.push(&b"ok \xff bytes\n"[..]);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("ok \u{fffd} bytes"));
    }
}
