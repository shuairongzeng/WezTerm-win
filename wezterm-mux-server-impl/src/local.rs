use anyhow::{Context as _, anyhow};
use config::{UnixDomain, create_user_owned_dirs};
use promise::spawn::spawn_into_main_thread;
use std::time::{Duration, Instant};
use wezterm_uds::UnixListener;

pub struct LocalListener {
    listener: UnixListener,
    unix_dom: UnixDomain,
}

#[cfg(windows)]
fn classify_windows_accept_error(code: Option<i32>) -> (bool, bool) {
    match code {
        None => (false, false),               // Unknown error, don't blindly recover
        Some(10004 | 10035) => (true, false), // WSAEINTR, WSAEWOULDBLOCK: just retry
        Some(10053 | 10054) => (true, false), // Connection-level: retry without rebuild
        Some(10024) => (true, false),         // WSAEMFILE: wait for FDs, retry
        Some(10038 | 10022 | 10050 | 10093) => (true, true), // Socket broken: rebuild
        Some(_) => (false, false),            // Unknown code, don't recover
    }
}

impl LocalListener {
    pub fn new(listener: UnixListener, unix_dom: UnixDomain) -> Self {
        Self { listener, unix_dom }
    }

    pub fn with_domain(unix_dom: &UnixDomain) -> anyhow::Result<Self> {
        let listener = safely_create_sock_path(unix_dom)?;
        Ok(Self::new(listener, unix_dom.clone()))
    }

    /// Attempt to recover the socket listener after an error.
    /// Returns true if recovery was successful, false otherwise.
    #[cfg(windows)]
    fn try_recover(&mut self) -> bool {
        let sock_path = self.unix_dom.socket_path();
        log::warn!(
            "LocalListener: attempting to recover socket at {}",
            sock_path.display()
        );

        // Try to recreate the socket
        match safely_create_sock_path(&self.unix_dom) {
            Ok(new_listener) => {
                self.listener = new_listener;
                log::info!(
                    "LocalListener: successfully recovered socket at {}",
                    sock_path.display()
                );
                true
            }
            Err(e) => {
                log::error!(
                    "LocalListener: failed to recover socket at {}: {:#}",
                    sock_path.display(),
                    e
                );
                false
            }
        }
    }

    pub fn run(&mut self) {
        let sock_path = self.unix_dom.socket_path();
        log::info!(
            "LocalListener: starting listener on {}",
            sock_path.display()
        );

        // Track consecutive errors for backoff
        let mut consecutive_errors: u32 = 0;
        let max_consecutive_errors: u32 = 10;
        let mut last_error_time: Option<Instant> = None;

        // F06: Track total recovery attempts to prevent infinite recovery loops
        // when the underlying problem is persistent.
        #[cfg(windows)]
        let mut total_recoveries: u32 = 0;
        #[cfg(windows)]
        let max_total_recoveries: u32 = 20;

        loop {
            // Reset error count if we've had a successful period
            if let Some(last_err) = last_error_time {
                if last_err.elapsed() > Duration::from_secs(60) {
                    consecutive_errors = 0;
                    last_error_time = None;
                }
            }

            match self.listener.accept() {
                Ok((stream, _addr)) => {
                    // Successful accept, reset error tracking
                    consecutive_errors = 0;
                    last_error_time = None;

                    spawn_into_main_thread(async move {
                        crate::dispatch::process(stream).await.map_err(|e| {
                            log::error!("{:#}", e);
                            e
                        })
                    })
                    .detach();
                }
                Err(err) => {
                    consecutive_errors += 1;
                    last_error_time = Some(Instant::now());

                    log::error!(
                        "LocalListener: accept failed (error {} of {}): {} (os_error={:?})",
                        consecutive_errors,
                        max_consecutive_errors,
                        err,
                        err.raw_os_error()
                    );

                    // On Windows, try to recover from socket errors
                    #[cfg(windows)]
                    {
                        // F10: Classify errors into retry-only vs needs-rebuild.
                        // Connection-level errors (10053, 10054) don't need socket rebuild.
                        // Transient errors (10004 WSAEINTR, 10035 WSAEWOULDBLOCK) just need retry.
                        // Socket-level errors (10038, 10022, 10050, 10093) need full rebuild.
                        let (should_retry, should_rebuild) =
                            classify_windows_accept_error(err.raw_os_error());

                        if should_retry && consecutive_errors < max_consecutive_errors {
                            // Exponential backoff before retry
                            let backoff =
                                Duration::from_millis(100 * (1u64 << consecutive_errors.min(6)));
                            log::warn!(
                                "LocalListener: waiting {:?} before recovery attempt",
                                backoff
                            );
                            std::thread::sleep(backoff);

                            if should_rebuild {
                                // F06: Check total recovery limit
                                if total_recoveries >= max_total_recoveries {
                                    log::error!(
                                        "LocalListener: reached total recovery limit ({}/{}), giving up",
                                        total_recoveries,
                                        max_total_recoveries
                                    );
                                    return;
                                }

                                if self.try_recover() {
                                    total_recoveries += 1;
                                    log::info!(
                                        "LocalListener: recovery successful ({}/{}), continuing",
                                        total_recoveries,
                                        max_total_recoveries
                                    );
                                    // Reset consecutive error count (but NOT total_recoveries)
                                    consecutive_errors = 0;
                                    last_error_time = None;
                                    continue;
                                }
                                // Recovery failed, fall through to exit check
                            } else {
                                // Retry-only: no rebuild needed, just loop back
                                continue;
                            }
                        }

                        // F05: Non-retryable errors should also sleep to prevent CPU spin
                        if !should_retry {
                            log::warn!(
                                "LocalListener: non-recoverable error code {:?}, waiting 500ms",
                                err.raw_os_error()
                            );
                            std::thread::sleep(Duration::from_millis(500));
                        }
                    }

                    // On non-Windows or after max errors, check if we should exit
                    #[cfg(not(windows))]
                    {
                        if consecutive_errors >= max_consecutive_errors {
                            log::error!(
                                "LocalListener: too many consecutive errors ({}), exiting",
                                consecutive_errors
                            );
                            return;
                        }
                        // Brief pause before retrying on non-Windows
                        std::thread::sleep(Duration::from_millis(100));
                    }

                    #[cfg(windows)]
                    {
                        if consecutive_errors >= max_consecutive_errors {
                            log::error!(
                                "LocalListener: too many consecutive errors ({}) and recovery failed, exiting",
                                consecutive_errors
                            );
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Take care when setting up the listener socket;
/// we need to be sure that the directory that we create it in
/// is owned by the user and has appropriate file permissions
/// that prevent other users from manipulating its contents.
fn safely_create_sock_path(unix_dom: &UnixDomain) -> anyhow::Result<UnixListener> {
    let sock_path = &unix_dom.socket_path();
    log::trace!("setting up {}", sock_path.display());

    let sock_dir = sock_path
        .parent()
        .ok_or_else(|| anyhow!("sock_path {} has no parent dir", sock_path.display()))?;

    create_user_owned_dirs(sock_dir)?;

    #[cfg(unix)]
    {
        use config::running_under_wsl;
        use std::os::unix::fs::PermissionsExt;

        if !running_under_wsl() && !unix_dom.skip_permissions_check {
            // Let's be sure that the ownership looks sane
            let meta = sock_dir.symlink_metadata()?;

            let permissions = meta.permissions();
            if (permissions.mode() & 0o22) != 0 {
                anyhow::bail!(
                    "The permissions for {} are insecure and currently \
                     allow other users to write to it (permissions={:?})",
                    sock_dir.display(),
                    permissions
                );
            }
        }
    }

    // We want to remove the socket if it exists.
    // However, on windows, we can't tell if the unix domain socket
    // exists using the methods on Path, so instead we just unconditionally
    // remove it and see what error occurs.
    match std::fs::remove_file(sock_path) {
        Ok(_) => {}
        Err(err) => match err.kind() {
            std::io::ErrorKind::NotFound => {}
            _ => return Err(err).context(format!("Unable to remove {}", sock_path.display())),
        },
    }

    let listener = UnixListener::bind(sock_path)
        .with_context(|| format!("Failed to bind to {}", sock_path.display()))?;

    config::set_sticky_bit(&sock_path);

    Ok(listener)
}

#[cfg(all(test, windows))]
mod tests {
    use super::classify_windows_accept_error;

    #[test]
    fn windows_accept_error_classification_matrix() {
        assert_eq!(classify_windows_accept_error(None), (false, false));
        assert_eq!(classify_windows_accept_error(Some(10004)), (true, false));
        assert_eq!(classify_windows_accept_error(Some(10035)), (true, false));
        assert_eq!(classify_windows_accept_error(Some(10053)), (true, false));
        assert_eq!(classify_windows_accept_error(Some(10054)), (true, false));
        assert_eq!(classify_windows_accept_error(Some(10024)), (true, false));
        assert_eq!(classify_windows_accept_error(Some(10038)), (true, true));
        assert_eq!(classify_windows_accept_error(Some(10022)), (true, true));
        assert_eq!(classify_windows_accept_error(Some(10050)), (true, true));
        assert_eq!(classify_windows_accept_error(Some(10093)), (true, true));
        assert_eq!(classify_windows_accept_error(Some(12345)), (false, false));
    }
}
