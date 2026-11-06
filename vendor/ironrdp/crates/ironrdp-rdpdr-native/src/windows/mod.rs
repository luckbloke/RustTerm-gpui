mod backend;
mod control;
mod directory;
mod factory;
mod file;
mod file_table;
mod handles;
mod locks;
mod path;
mod pending;
mod printer;
mod scard;
mod security;
mod status;
mod volume;

pub use backend::WindowsRdpdrBackend;
pub use factory::{RedirectedDrive, RedirectedDriveError, WindowsRdpdrBackendFactory};
pub use printer::{RdpdrWorkerThreadGuard, RdpdrWorkerThreadGuardResult, RdpdrWorkerThreadHooks};

/// NyaTerm diagnostic switch for redirected-drive troubleshooting.
///
/// The consuming application installs no `tracing` subscriber, so every
/// `debug!`/`trace!` record in this crate is dropped and a refused IRP stays
/// invisible even though the remote shell turns it into
/// "无法访问 \\\\tsclient\\\\...". With `NYATERM_RDPDR_TRACE=1` each filesystem
/// IRP is printed on stderr together with its status.
pub(crate) fn trace_enabled() -> bool {
    std::env::var_os("NYATERM_RDPDR_TRACE").is_some()
}

/// NyaTerm diagnostic sink: prints one diagnostic line on stderr and appends it
/// to the file named by `NYATERM_RDPDR_LOG` (the host application points that
/// at its own data directory).
///
/// stderr alone is invisible for windowed (`windows_subsystem = "windows"`)
/// launches, which is exactly how end users run the app while hitting a
/// misbehaving redirected drive.
pub(crate) fn log_diagnostic(line: core::fmt::Arguments<'_>) {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    eprintln!("[RDPDR {timestamp}] {line}");

    use std::io::Write as _;
    let Some(path) = std::env::var_os("NYATERM_RDPDR_LOG") else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(file, "[RDPDR {timestamp}] {line}");
}
