//! `tracing` output: logcat on Android (`adb logcat -s AetherLink`), stderr elsewhere.

use tracing_subscriber::EnvFilter;

pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    #[cfg(target_os = "android")]
    {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .without_time()
            .with_writer(logcat::Writer::default)
            .try_init();
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    }
}

#[cfg(target_os = "android")]
mod logcat {
    use std::ffi::{CString, c_char, c_int};

    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    const ANDROID_LOG_INFO: c_int = 4;

    /// Buffers one formatted event and writes it as a single logcat line on drop.
    #[derive(Default)]
    pub struct Writer(Vec<u8>);

    impl std::io::Write for Writer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Drop for Writer {
        fn drop(&mut self) {
            let text = String::from_utf8_lossy(&self.0).trim_end().replace('\0', " ");
            if text.is_empty() {
                return;
            }
            if let (Ok(tag), Ok(msg)) = (CString::new("AetherLink"), CString::new(text)) {
                // SAFETY: both pointers are valid NUL-terminated strings for the call's duration.
                unsafe { __android_log_write(ANDROID_LOG_INFO, tag.as_ptr(), msg.as_ptr()) };
            }
        }
    }
}
