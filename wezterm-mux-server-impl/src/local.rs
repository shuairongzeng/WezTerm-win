use anyhow::{anyhow, Context as _};
use config::{create_user_owned_dirs, UnixDomain};
use promise::spawn::spawn_into_main_thread;
use std::time::{Duration, Instant};
use wezterm_uds::UnixListener;

pub struct LocalListener {
    listener: UnixListener,
    unix_dom: UnixDomain,
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
                        "LocalListener: accept failed (error {} of {}): {}",
                        consecutive_errors,
                        max_consecutive_errors,
                        err
                    );

                    // On Windows, try to recover from socket errors
                    #[cfg(windows)]
                    {
                        // Check for specific Windows socket errors that are recoverable:
                        // 10053 = WSAECONNABORTED (connection aborted)
                        // 10054 = WSAECONNRESET (connection reset by peer)
                        // 10038 = WSAENOTSOCK (socket operation on non-socket)
                        // 10024 = WSAEMFILE (too many open files)
                        // 10022 = WSAEINVAL (invalid argument, socket in bad state)
                        // 10093 = WSANOTINITIALISED (WSAStartup not called)
                        // 10050 = WSAENETDOWN (network subsystem failed)
                        //
                        // We do NOT recover from errors without an OS code (None) as they
                        // may indicate logic errors or unexpected conditions.
                        let should_recover = match err.raw_os_error() {
                            None => false, // Unknown error, don't blindly recover - may mask real issues
                            Some(code) => matches!(code, 10053 | 10054 | 10038 | 10024 | 10022 | 10093 | 10050),
                        };

                        if should_recover && consecutive_errors < max_consecutive_errors {
                            // Exponential backoff before retry
                            let backoff =
                                Duration::from_millis(100 * (1u64 << consecutive_errors.min(6)));
                            log::warn!(
                                "LocalListener: waiting {:?} before recovery attempt",
                                backoff
                            );
                            std::thread::sleep(backoff);

                            if self.try_recover() {
                                log::info!("LocalListener: recovery successful, continuing");
                                // Reset error count after successful recovery
                                consecutive_errors = 0;
                                last_error_time = None;
                                continue;
                            }
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
