use crate::client::{ClientId, ClientInfo};
use crate::pane::{CachePolicy, Pane, PaneId};
use crate::ssh_agent::AgentProxy;
use crate::tab::{SplitRequest, Tab, TabId};
use crate::window::{Window, WindowId};
use anyhow::{anyhow, Context, Error};
use config::keyassignment::SpawnTabDomain;
use config::{configuration, ExitBehavior, GuiPosition};
use crossbeam::channel::{
    bounded as crossbeam_bounded, Receiver as CrossbeamReceiver, SendError,
    Sender as CrossbeamSender,
};
use domain::{Domain, DomainId, DomainState, SplitSource};
use log::error;
use metrics::histogram;
use parking_lot::{
    MappedRwLockReadGuard, MappedRwLockWriteGuard, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
use percent_encoding::percent_decode_str;
use portable_pty::{CommandBuilder, ExitStatus, PtySize};
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, Instant};
use termwiz::escape::csi::{DecPrivateMode, DecPrivateModeCode, Device, Mode};
use termwiz::escape::{Action, CSI};
use thiserror::*;
use wezterm_term::{Clipboard, ClipboardSelection, DownloadHandler, TerminalSize};

pub mod activity;
pub mod client;
pub mod connui;
pub mod domain;
pub mod localpane;
pub mod pane;
pub mod renderable;
pub mod ssh;
pub mod ssh_agent;
pub mod tab;
pub mod termwiztermtab;
pub mod tmux;
pub mod tmux_commands;
mod tmux_pty;
pub mod window;

use crate::activity::Activity;

pub const DEFAULT_WORKSPACE: &str = "default";

#[derive(Clone, Debug)]
pub enum MuxNotification {
    PaneOutput(PaneId),
    PaneAdded(PaneId),
    PaneRemoved(PaneId),
    WindowCreated(WindowId),
    WindowRemoved(WindowId),
    WindowInvalidated(WindowId),
    WindowWorkspaceChanged(WindowId),
    ActiveWorkspaceChanged(Arc<ClientId>),
    Alert {
        pane_id: PaneId,
        alert: wezterm_term::Alert,
    },
    Empty,
    AssignClipboard {
        pane_id: PaneId,
        selection: ClipboardSelection,
        clipboard: Option<String>,
    },
    SaveToDownloads {
        name: Option<String>,
        data: Arc<Vec<u8>>,
    },
    TabAddedToWindow {
        tab_id: TabId,
        window_id: WindowId,
    },
    PaneFocused(PaneId),
    TabResized(TabId),
    TabTitleChanged {
        tab_id: TabId,
        title: String,
    },
    WindowTitleChanged {
        window_id: WindowId,
        title: String,
    },
    WorkspaceRenamed {
        old_workspace: String,
        new_workspace: String,
    },
}

static SUB_ID: AtomicUsize = AtomicUsize::new(0);

pub struct Mux {
    tabs: RwLock<HashMap<TabId, Arc<Tab>>>,
    panes: RwLock<HashMap<PaneId, Arc<dyn Pane>>>,
    windows: RwLock<HashMap<WindowId, Window>>,
    prune_dead_windows_retry_scheduled: AtomicBool,
    default_domain: RwLock<Option<Arc<dyn Domain>>>,
    domains: RwLock<HashMap<DomainId, Arc<dyn Domain>>>,
    domains_by_name: RwLock<HashMap<String, Arc<dyn Domain>>>,
    subscribers: RwLock<HashMap<usize, Box<dyn Fn(MuxNotification) -> bool + Send + Sync>>>,
    banner: RwLock<Option<String>>,
    clients: RwLock<HashMap<ClientId, ClientInfo>>,
    identity: RwLock<Option<Arc<ClientId>>>,
    num_panes_by_workspace: RwLock<HashMap<String, usize>>,
    main_thread_id: std::thread::ThreadId,
    agent: Option<AgentProxy>,
}

const BUFSIZE: usize = 1024 * 1024;

/// This function applies parsed actions to the pane and notifies any
/// mux subscribers about the output event
fn send_actions_to_mux(pane: &Weak<dyn Pane>, dead: &Arc<AtomicBool>, actions: Vec<Action>) {
    let start = Instant::now();
    match pane.upgrade() {
        Some(pane) => {
            let action_count = actions.len();
            pane.perform_actions(actions);
            let elapsed = start.elapsed();
            histogram!("send_actions_to_mux.perform_actions.latency").record(elapsed);
            if elapsed > Duration::from_millis(100) {
                log::warn!(
                    "send_actions_to_mux: perform_actions took {:?} for {} actions on pane {}",
                    elapsed,
                    action_count,
                    pane.pane_id()
                );
            }
            Mux::notify_from_any_thread(MuxNotification::PaneOutput(pane.pane_id()));
        }
        None => {
            // Something else removed the pane from
            // the mux, so signal that we should stop
            // trying to process it in read_from_pane_pty.
            log::warn!("send_actions_to_mux: pane.upgrade() failed, pane was removed from mux, setting dead=true");
            dead.store(true, Ordering::Release); // F12: Use Release for cross-thread visibility
        }
    }
    histogram!("send_actions_to_mux.rate").record(1.);
}

fn parse_buffered_data(
    pane: Weak<dyn Pane>,
    dead: &Arc<AtomicBool>,
    rx: CrossbeamReceiver<Vec<u8>>,
) {
    let pane_id = pane.upgrade().map(|p| p.pane_id()).unwrap_or(0);
    let start_time = Instant::now();
    log::info!(
        "parse_buffered_data: starting parser thread for pane {} at {:?}",
        pane_id,
        start_time
    );

    let mut parser = termwiz::escape::parser::Parser::new();
    let mut actions = vec![];
    let mut hold = false;
    let mut action_size = 0;
    let mut delay = Duration::from_millis(configuration().mux_output_parser_coalesce_delay_ms);
    let mut deadline = None;

    loop {
        match rx.recv() {
            Err(_) => {
                // Channel disconnected = sender dropped = EOF
                log::info!(
                    "parse_buffered_data: channel disconnected for pane {} (ran for {:?})",
                    pane_id,
                    start_time.elapsed()
                );
                break;
            }
            Ok(data) if data.is_empty() => {
                // Empty Vec = explicit EOF signal
                log::info!(
                    "parse_buffered_data: EOF signal for pane {} (ran for {:?})",
                    pane_id,
                    start_time.elapsed()
                );
                break;
            }
            Ok(data) => {
                let size = data.len();
                parser.parse(&data, |action| {
                    let mut flush = false;
                    match &action {
                        Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                            DecPrivateModeCode::SynchronizedOutput,
                        )))) => {
                            hold = true;

                            // Flush prior actions
                            if !actions.is_empty() {
                                send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
                                action_size = 0;
                            }
                        }
                        Action::CSI(CSI::Mode(Mode::ResetDecPrivateMode(
                            DecPrivateMode::Code(DecPrivateModeCode::SynchronizedOutput),
                        ))) => {
                            hold = false;
                            flush = true;
                        }
                        Action::CSI(CSI::Device(dev)) if matches!(**dev, Device::SoftReset) => {
                            hold = false;
                            flush = true;
                        }
                        _ => {}
                    };
                    action.append_to(&mut actions);

                    if flush && !actions.is_empty() {
                        send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
                        action_size = 0;
                    }
                });
                action_size += size;
                if !actions.is_empty() && !hold {
                    // If we haven't accumulated too much data,
                    // pause for a short while to increase the chances
                    // that we coalesce a full "frame" from an unoptimized
                    // TUI program.
                    // IMPORTANT: Use an inner loop for coalescing so that
                    // actions are always flushed before returning to the
                    // blocking rx.recv() at the top. Unlike poll()+read()
                    // on sockets, recv_timeout() consumes the data, so we
                    // must not `continue` back to the outer loop with
                    // unflushed actions.
                    let config = configuration();
                    let buf_size = config.mux_output_parser_buffer_size;
                    'coalesce: while action_size < buf_size {
                        let poll_delay = match deadline {
                            None => {
                                deadline.replace(Instant::now() + delay);
                                Some(delay)
                            }
                            Some(target) => target.checked_duration_since(Instant::now()),
                        };
                        let Some(poll_delay) = poll_delay else {
                            break 'coalesce;
                        };
                        match rx.recv_timeout(poll_delay) {
                            Ok(more_data) if more_data.is_empty() => {
                                // EOF signal - flush and exit
                                send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
                                return;
                            }
                            Ok(more_data) => {
                                // More data available within coalesce window
                                let more_size = more_data.len();
                                parser.parse(&more_data, |action| {
                                    action.append_to(&mut actions);
                                });
                                action_size += more_size;
                                // Stay in inner loop to try coalescing more
                                continue 'coalesce;
                            }
                            Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
                                // No more data within window - flush
                                break 'coalesce;
                            }
                            Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                                // Channel closed during coalesce
                                send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
                                return;
                            }
                        }
                    }

                    // Always flush actions before returning to blocking rx.recv()
                    send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
                    deadline = None;
                    action_size = 0;
                }

                delay = Duration::from_millis(configuration().mux_output_parser_coalesce_delay_ms);
            }
        }
    }

    // Don't forget to send anything that we might have buffered
    // to be displayed before we return from here; this is important
    // for very short lived commands so that we don't forget to
    // display what they displayed.
    if !actions.is_empty() {
        send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
    }
}

/// Helper function for the reader recovery loop.
/// Returns true if the reader disconnected (allowing another recovery attempt),
/// false if we should break out of the main loop entirely.
#[allow(clippy::too_many_arguments)]
fn run_reader_loop(
    pane: &Weak<dyn Pane>,
    pane_id: PaneId,
    parser_tx: &mut CrossbeamSender<Vec<u8>>,
    dead: &mut Arc<AtomicBool>,
    parser_needs_recovery: &Arc<AtomicBool>,
    data_rx: &std::sync::mpsc::Receiver<Result<Vec<u8>, std::io::ErrorKind>>,
    _start_time: Instant,
    #[cfg(windows)] recovery_count: &mut u32,
    #[cfg(windows)] do_recovery: &mut dyn FnMut(
        &mut CrossbeamSender<Vec<u8>>,
        &mut Arc<AtomicBool>,
        &Arc<AtomicBool>,
        &Weak<dyn Pane>,
        PaneId,
        &mut u32,
    ) -> bool,
) -> bool {
    loop {
        if parser_needs_recovery.load(Ordering::Acquire) {
            #[cfg(windows)]
            {
                if !do_recovery(
                    parser_tx,
                    dead,
                    parser_needs_recovery,
                    pane,
                    pane_id,
                    recovery_count,
                ) {
                    return false;
                }
                continue;
            }
            #[cfg(not(windows))]
            {
                return false;
            }
        }

        let timeout = Duration::from_millis(500);
        match data_rx.recv_timeout(timeout) {
            Ok(Ok(data)) if data.is_empty() => {
                let _ = parser_tx.send(Vec::new());
                return false;
            }
            Ok(Err(err_kind)) => {
                log::warn!(
                    "read_pty recovered reader error for pane {}: {:?}",
                    pane_id,
                    err_kind
                );
                let _ = parser_tx.send(Vec::new());
                return false;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if dead.load(Ordering::Acquire) {
                    return false;
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Reader disconnected again - allow another recovery attempt
                return true;
            }
            Ok(Ok(data)) => {
                let size = data.len();
                histogram!("read_from_pane_pty.bytes.rate").record(size as f64);

                if parser_needs_recovery.load(Ordering::Acquire) {
                    #[cfg(windows)]
                    {
                        if !do_recovery(
                            parser_tx,
                            dead,
                            parser_needs_recovery,
                            pane,
                            pane_id,
                            recovery_count,
                        ) {
                            return false;
                        }
                        if let Err(SendError(_)) = parser_tx.send(data) {
                            return false;
                        }
                        continue;
                    }
                    #[cfg(not(windows))]
                    {
                        return false;
                    }
                }

                if let Err(SendError(_)) = parser_tx.send(data) {
                    #[cfg(windows)]
                    {
                        continue;
                    }
                    #[cfg(not(windows))]
                    {
                        return false;
                    }
                }
            }
        }
    }
}

/// This function is run in a separate thread; its purpose is to perform
/// blocking reads from the pty (non-blocking reads are not portable to
/// all platforms and pty/tty types), parse the escape sequences and
/// relay the actions to the mux thread to apply them to the pane.
fn read_from_pane_pty(
    pane: Weak<dyn Pane>,
    banner: Option<String>,
    reader: Box<dyn std::io::Read + Send>,
) {
    let start_time = Instant::now();

    // This is used to signal that an error occurred either in this thread,
    // or in the main mux thread.  If `true`, this thread will terminate.
    let mut dead = Arc::new(AtomicBool::new(false));

    // Shared flag to signal that the parser has exited and needs recovery.
    let parser_needs_recovery = Arc::new(AtomicBool::new(false));

    let (pane_id, exit_behavior) = match pane.upgrade() {
        Some(pane) => (pane.pane_id(), pane.exit_behavior()),
        None => return,
    };

    log::info!(
        "read_from_pane_pty: starting for pane {} at {:?}",
        pane_id,
        start_time
    );

    // Create crossbeam bounded channel instead of socketpair.
    // This eliminates TCP loopback (127.0.0.1) which was the root cause of
    // silent pane death after ~8 hours on Windows due to TCP timeouts/resets.
    let (mut parser_tx, parser_rx) = crossbeam_bounded::<Vec<u8>>(1024);

    // Use a channel to receive data from a dedicated reader thread.
    let (data_tx, data_rx) =
        std::sync::mpsc::sync_channel::<Result<Vec<u8>, std::io::ErrorKind>>(64);

    // Spawn a dedicated PTY reader thread that sends data through the channel
    let pane_id_for_reader = pane_id;
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut local_buf = vec![0u8; BUFSIZE];
        loop {
            match reader.read(&mut local_buf) {
                Ok(0) => {
                    let _ = data_tx.send(Ok(Vec::new()));
                    break;
                }
                Ok(size) => {
                    if data_tx.send(Ok(local_buf[..size].to_vec())).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = data_tx.send(Err(e.kind()));
                    break;
                }
            }
        }
        log::info!(
            "read_from_pane_pty: PTY reader thread exited for pane {}",
            pane_id_for_reader
        );
    });

    // Spawn parser thread using crossbeam channel
    std::thread::spawn({
        let dead = Arc::clone(&dead);
        let pane = pane.clone();
        let parser_needs_recovery = Arc::clone(&parser_needs_recovery);
        let pane_id_for_parser = pane_id;
        move || {
            parse_buffered_data(pane, &dead, parser_rx);
            parser_needs_recovery.store(true, Ordering::Release);
            log::info!(
                "parse_buffered_data: parser thread exited, recovery flag set for pane {}",
                pane_id_for_parser
            );
        }
    });

    if let Some(banner) = banner {
        let _ = parser_tx.send(banner.into_bytes());
    }

    // Recovery helper closure - now uses crossbeam channel instead of socketpair
    #[cfg(windows)]
    let mut recovery_count = 0u32;
    #[cfg(windows)]
    const MAX_PARSER_RECOVERIES: u32 = 5;

    #[cfg(windows)]
    let mut do_recovery = |parser_tx: &mut CrossbeamSender<Vec<u8>>,
                           dead: &mut Arc<AtomicBool>,
                           parser_needs_recovery: &Arc<AtomicBool>,
                           pane: &Weak<dyn Pane>,
                           pane_id: PaneId,
                           recovery_count: &mut u32|
     -> bool {
        *recovery_count += 1;
        log::warn!(
            "read_from_pane_pty: parser died for pane {}, attempting recovery #{} (uptime: {:?})",
            pane_id,
            recovery_count,
            start_time.elapsed()
        );

        if *recovery_count > MAX_PARSER_RECOVERIES {
            log::error!(
                "read_from_pane_pty: parser recovery limit ({}) reached for pane {}",
                MAX_PARSER_RECOVERIES,
                pane_id
            );
            // Don't let the pane silently die. Trigger EOF to parser
            // and let the normal cleanup path handle it.
            let _ = parser_tx.send(Vec::new());
            return false;
        }

        if pane.upgrade().is_none() {
            log::error!(
                "read_from_pane_pty: pane {} no longer exists, skipping recovery",
                pane_id
            );
            parser_needs_recovery.store(false, Ordering::Release);
            return false;
        }

        // Create new crossbeam channel (no more socketpair!)
        let (new_tx, new_rx) = crossbeam_bounded::<Vec<u8>>(1024);
        *parser_tx = new_tx;
        let new_dead = Arc::new(AtomicBool::new(false));
        *dead = new_dead;
        parser_needs_recovery.store(false, Ordering::Release);

        let pane_clone = pane.clone();
        let dead_clone = Arc::clone(dead);
        let recovery_clone = Arc::clone(parser_needs_recovery);
        let pane_id_for_recovered = pane_id;
        std::thread::spawn(move || {
            parse_buffered_data(pane_clone, &dead_clone, new_rx);
            recovery_clone.store(true, Ordering::Release);
            log::info!(
                "parse_buffered_data: recovered parser thread exited, recovery flag set for pane {}",
                pane_id_for_recovered
            );
        });
        log::info!(
            "read_from_pane_pty: recovered parser for pane {} (recovery #{})",
            pane_id,
            recovery_count
        );
        true
    };

    // PTY reader recovery tracking
    let mut reader_recovery_count = 0u32;
    const MAX_READER_RECOVERIES: u32 = 3;

    loop {
        // Check if parser died and needs recovery FIRST
        if parser_needs_recovery.load(Ordering::Acquire) {
            #[cfg(windows)]
            {
                if !do_recovery(
                    &mut parser_tx,
                    &mut dead,
                    &parser_needs_recovery,
                    &pane,
                    pane_id,
                    &mut recovery_count,
                ) {
                    break;
                }
                continue;
            }
            #[cfg(not(windows))]
            {
                log::error!(
                    "read_from_pane_pty: parser died for pane {}, exiting",
                    pane_id
                );
                break;
            }
        }

        let timeout = Duration::from_millis(500);
        match data_rx.recv_timeout(timeout) {
            Ok(Ok(data)) if data.is_empty() => {
                // EOF from PTY
                log::info!(
                    "read_pty EOF (shell exited normally): pane_id {} (uptime: {:?})",
                    pane_id,
                    start_time.elapsed()
                );
                // Send EOF signal to parser
                let _ = parser_tx.send(Vec::new());
                break;
            }
            Ok(Err(err_kind)) => {
                log::warn!(
                    "read_pty reader error for pane {}: {:?} (uptime: {:?})",
                    pane_id,
                    err_kind,
                    start_time.elapsed()
                );
                let _ = parser_tx.send(Vec::new());
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if dead.load(Ordering::Acquire) {
                    log::info!(
                        "read_from_pane_pty: dead flag set, exiting loop for pane {} (uptime: {:?})",
                        pane_id,
                        start_time.elapsed()
                    );
                    break;
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // PTY reader thread exited - check if child is still alive
                log::info!(
                    "read_from_pane_pty: data channel disconnected for pane {} (uptime: {:?})",
                    pane_id,
                    start_time.elapsed()
                );

                // Step 3: PTY Reader recovery
                if reader_recovery_count < MAX_READER_RECOVERIES {
                    if let Some(pane_ref) = pane.upgrade() {
                        if !pane_ref.is_dead() {
                            log::warn!(
                                "PTY reader died but child still alive for pane {}, attempting reader recovery #{}",
                                pane_id,
                                reader_recovery_count + 1
                            );
                            match pane_ref.reader() {
                                Ok(Some(new_reader)) => {
                                    let (new_data_tx, new_data_rx) =
                                        std::sync::mpsc::sync_channel::<
                                            Result<Vec<u8>, std::io::ErrorKind>,
                                        >(64);
                                    // Replace data_rx - we need to shadow it
                                    // Since data_rx is immutable in the match, we break and re-enter
                                    let pane_id_for_new_reader = pane_id;
                                    std::thread::spawn(move || {
                                        let mut reader = new_reader;
                                        let mut local_buf = vec![0u8; BUFSIZE];
                                        loop {
                                            match reader.read(&mut local_buf) {
                                                Ok(0) => {
                                                    let _ = new_data_tx.send(Ok(Vec::new()));
                                                    break;
                                                }
                                                Ok(size) => {
                                                    if new_data_tx
                                                        .send(Ok(local_buf[..size].to_vec()))
                                                        .is_err()
                                                    {
                                                        break;
                                                    }
                                                }
                                                Err(e) => {
                                                    let _ = new_data_tx.send(Err(e.kind()));
                                                    break;
                                                }
                                            }
                                        }
                                        log::info!(
                                            "read_from_pane_pty: recovered PTY reader thread exited for pane {}",
                                            pane_id_for_new_reader
                                        );
                                    });
                                    reader_recovery_count += 1;
                                    log::info!(
                                        "PTY reader recovered for pane {} (recovery #{})",
                                        pane_id,
                                        reader_recovery_count
                                    );
                                    // We can't reassign data_rx in a match arm,
                                    // so we run a nested loop with the new receiver
                                    // and then break out when done
                                    let nested_result = run_reader_loop(
                                        &pane,
                                        pane_id,
                                        &mut parser_tx,
                                        &mut dead,
                                        &parser_needs_recovery,
                                        &new_data_rx,
                                        start_time,
                                        #[cfg(windows)]
                                        &mut recovery_count,
                                        #[cfg(windows)]
                                        &mut do_recovery,
                                    );
                                    if !nested_result {
                                        break;
                                    }
                                    // If nested loop returned true, it means the reader
                                    // disconnected again - continue outer loop for another
                                    // recovery attempt
                                    continue;
                                }
                                _ => {
                                    log::error!("Failed to get new reader for pane {}", pane_id);
                                }
                            }
                        }
                    }
                } else {
                    log::error!(
                        "PTY reader recovery limit ({}) reached for pane {}",
                        MAX_READER_RECOVERIES,
                        pane_id
                    );
                    // Send EOF to parser to trigger normal pane cleanup
                    let _ = parser_tx.send(Vec::new());
                    break;
                }

                let _ = parser_tx.send(Vec::new());
                break;
            }
            Ok(Ok(data)) => {
                let size = data.len();
                histogram!("read_from_pane_pty.bytes.rate").record(size as f64);
                log::trace!("read_pty pane {pane_id} read {size} bytes");

                // Check if parser died BEFORE attempting to send
                if parser_needs_recovery.load(Ordering::Acquire) {
                    #[cfg(windows)]
                    {
                        if !do_recovery(
                            &mut parser_tx,
                            &mut dead,
                            &parser_needs_recovery,
                            &pane,
                            pane_id,
                            &mut recovery_count,
                        ) {
                            break;
                        }
                        // Send the current data to the new channel
                        if let Err(SendError(_)) = parser_tx.send(data) {
                            error!(
                                "read_pty failed to send data after recovery for pane {}",
                                pane_id
                            );
                            break;
                        }
                        continue;
                    }
                    #[cfg(not(windows))]
                    {
                        log::error!(
                            "read_from_pane_pty: parser died for pane {}, exiting",
                            pane_id
                        );
                        break;
                    }
                }

                // Send data through crossbeam channel instead of socketpair write
                if let Err(SendError(_)) = parser_tx.send(data) {
                    // Channel closed = parser thread exited
                    #[cfg(windows)]
                    {
                        log::warn!(
                            "read_pty channel send failed for pane {}, parser may have died",
                            pane_id
                        );
                        // The parser_needs_recovery flag should be set soon;
                        // loop back to check it
                        continue;
                    }
                    #[cfg(not(windows))]
                    {
                        error!("read_pty failed to send to parser: pane {}", pane_id);
                        break;
                    }
                }
            }
        }
    }

    match exit_behavior.unwrap_or_else(|| configuration().exit_behavior) {
        ExitBehavior::Hold | ExitBehavior::CloseOnCleanExit => {
            // We don't know if we can unilaterally close
            // this pane right now, so don't!
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::get();
                log::trace!("checking for dead windows after EOF on pane {}", pane_id);
                mux.prune_dead_windows();
            })
            .detach();
        }
        ExitBehavior::Close => {
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::get();
                mux.remove_pane(pane_id);
            })
            .detach();
        }
    }

    dead.store(true, Ordering::Release); // F12: Use Release for cross-thread visibility
}

lazy_static::lazy_static! {
    static ref MUX: Mutex<Option<Arc<Mux>>> = Mutex::new(None);
}

pub struct MuxWindowBuilder {
    window_id: WindowId,
    activity: Option<Activity>,
    notified: bool,
}

impl MuxWindowBuilder {
    fn notify(&mut self) {
        if self.notified {
            return;
        }
        self.notified = true;
        let activity = self.activity.take().unwrap();
        let window_id = self.window_id;
        let mux = Mux::get();
        if mux.is_main_thread() {
            // If we're already on the mux thread, just send the notification
            // immediately.
            // This is super important for Wayland; if we push it to the
            // spawn queue below then the extra milliseconds of delay
            // causes it to get confused and shutdown the connection!?
            mux.notify(MuxNotification::WindowCreated(window_id));
        } else {
            promise::spawn::spawn_into_main_thread(async move {
                if let Some(mux) = Mux::try_get() {
                    mux.notify(MuxNotification::WindowCreated(window_id));
                    drop(activity);
                }
            })
            .detach();
        }
    }
}

impl Drop for MuxWindowBuilder {
    fn drop(&mut self) {
        self.notify();
    }
}

impl std::ops::Deref for MuxWindowBuilder {
    type Target = WindowId;

    fn deref(&self) -> &WindowId {
        &self.window_id
    }
}

impl Mux {
    pub fn new(default_domain: Option<Arc<dyn Domain>>) -> Self {
        let mut domains = HashMap::new();
        let mut domains_by_name = HashMap::new();
        if let Some(default_domain) = default_domain.as_ref() {
            domains.insert(default_domain.domain_id(), Arc::clone(default_domain));

            domains_by_name.insert(
                default_domain.domain_name().to_string(),
                Arc::clone(default_domain),
            );
        }

        let agent = if config::configuration().mux_enable_ssh_agent {
            Some(AgentProxy::new())
        } else {
            None
        };

        Self {
            tabs: RwLock::new(HashMap::new()),
            panes: RwLock::new(HashMap::new()),
            windows: RwLock::new(HashMap::new()),
            prune_dead_windows_retry_scheduled: AtomicBool::new(false),
            default_domain: RwLock::new(default_domain),
            domains_by_name: RwLock::new(domains_by_name),
            domains: RwLock::new(domains),
            subscribers: RwLock::new(HashMap::new()),
            banner: RwLock::new(None),
            clients: RwLock::new(HashMap::new()),
            identity: RwLock::new(None),
            num_panes_by_workspace: RwLock::new(HashMap::new()),
            main_thread_id: std::thread::current().id(),
            agent,
        }
    }

    fn get_default_workspace(&self) -> String {
        let config = configuration();
        config
            .default_workspace
            .as_deref()
            .unwrap_or(DEFAULT_WORKSPACE)
            .to_string()
    }

    pub fn is_main_thread(&self) -> bool {
        std::thread::current().id() == self.main_thread_id
    }

    fn recompute_pane_count(&self) {
        let mut count = HashMap::new();
        for window in self.windows.read().values() {
            let workspace = window.get_workspace();
            for tab in window.iter() {
                *count.entry(workspace.to_string()).or_insert(0) += match tab.count_panes() {
                    Some(n) => n,
                    None => {
                        // Busy: abort this and we'll retry later
                        return;
                    }
                };
            }
        }
        *self.num_panes_by_workspace.write() = count;
    }

    pub fn client_had_input(&self, client_id: &ClientId) {
        if let Some(info) = self.clients.write().get_mut(client_id) {
            info.update_last_input();
        }
        if let Some(agent) = &self.agent {
            agent.update_target();
        }
    }

    pub fn record_input_for_current_identity(&self) {
        if let Some(ident) = self.identity.read().as_ref() {
            self.client_had_input(ident);
        }
    }

    pub fn record_focus_for_current_identity(&self, pane_id: PaneId) {
        if let Some(ident) = self.identity.read().as_ref() {
            self.record_focus_for_client(ident, pane_id);
        }
    }

    pub fn resolve_focused_pane(
        &self,
        client_id: &ClientId,
    ) -> Option<(DomainId, WindowId, TabId, PaneId)> {
        let pane_id = self.clients.read().get(client_id)?.focused_pane_id?;
        let (domain, window, tab) = self.resolve_pane_id(pane_id)?;
        Some((domain, window, tab, pane_id))
    }

    pub fn record_focus_for_client(&self, client_id: &ClientId, pane_id: PaneId) {
        let mut prior = None;
        if let Some(info) = self.clients.write().get_mut(client_id) {
            prior = info.focused_pane_id;
            info.update_focused_pane(pane_id);
        }

        if prior == Some(pane_id) {
            return;
        }
        // Synthesize focus events
        if let Some(prior_id) = prior {
            if let Some(pane) = self.get_pane(prior_id) {
                pane.focus_changed(false);
            }
        }
        if let Some(pane) = self.get_pane(pane_id) {
            pane.focus_changed(true);
        }
    }

    /// Called by PaneFocused event handlers to reconcile a remote
    /// pane focus event and apply its effects locally
    pub fn focus_pane_and_containing_tab(&self, pane_id: PaneId) -> anyhow::Result<()> {
        let pane = self
            .get_pane(pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {pane_id} not found"))?;

        let (_domain, window_id, tab_id) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow::anyhow!("can't find {pane_id} in the mux"))?;

        // Focus/activate the containing tab within its window
        {
            let mut win = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow::anyhow!("window_id {window_id} not found"))?;
            let tab_idx = win
                .idx_by_id(tab_id)
                .ok_or_else(|| anyhow::anyhow!("tab {tab_id} not in {window_id}"))?;
            win.save_and_then_set_active(tab_idx);
        }

        // Focus/activate the pane locally
        let tab = self
            .get_tab(tab_id)
            .ok_or_else(|| anyhow::anyhow!("tab {tab_id} not found"))?;

        tab.set_active_pane(&pane);

        Ok(())
    }

    pub fn register_client(&self, client_id: Arc<ClientId>) {
        self.clients
            .write()
            .insert((*client_id).clone(), ClientInfo::new(client_id));
    }

    pub fn iter_clients(&self) -> Vec<ClientInfo> {
        self.clients
            .read()
            .values()
            .map(|info| info.clone())
            .collect()
    }

    /// Returns a list of the unique workspace names known to the mux.
    /// This is taken from all known windows.
    pub fn iter_workspaces(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .windows
            .read()
            .values()
            .map(|w| w.get_workspace().to_string())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Generate a new unique workspace name
    pub fn generate_workspace_name(&self) -> String {
        let used = self.iter_workspaces();
        for candidate in names::Generator::default() {
            if !used.contains(&candidate) {
                return candidate;
            }
        }
        unreachable!();
    }

    /// Returns the effective active workspace name
    pub fn active_workspace(&self) -> String {
        self.identity
            .read()
            .as_ref()
            .and_then(|ident| {
                self.clients
                    .read()
                    .get(&ident)
                    .and_then(|info| info.active_workspace.clone())
            })
            .unwrap_or_else(|| self.get_default_workspace())
    }

    /// Returns the effective active workspace name for a given client
    pub fn active_workspace_for_client(&self, ident: &Arc<ClientId>) -> String {
        self.clients
            .read()
            .get(&ident)
            .and_then(|info| info.active_workspace.clone())
            .unwrap_or_else(|| self.get_default_workspace())
    }

    pub fn set_active_workspace_for_client(&self, ident: &Arc<ClientId>, workspace: &str) {
        let mut clients = self.clients.write();
        if let Some(info) = clients.get_mut(&ident) {
            info.active_workspace.replace(workspace.to_string());
            self.notify(MuxNotification::ActiveWorkspaceChanged(ident.clone()));
        }
    }

    /// Assigns the active workspace name for the current identity
    pub fn set_active_workspace(&self, workspace: &str) {
        if let Some(ident) = self.identity.read().clone() {
            self.set_active_workspace_for_client(&ident, workspace);
        }
    }

    pub fn rename_workspace(&self, old_workspace: &str, new_workspace: &str) {
        if old_workspace == new_workspace {
            return;
        }
        self.notify(MuxNotification::WorkspaceRenamed {
            old_workspace: old_workspace.to_string(),
            new_workspace: new_workspace.to_string(),
        });

        for window in self.windows.write().values_mut() {
            if window.get_workspace() == old_workspace {
                window.set_workspace(new_workspace);
            }
        }
        self.recompute_pane_count();
        for client in self.clients.write().values_mut() {
            if client.active_workspace.as_deref() == Some(old_workspace) {
                client.active_workspace.replace(new_workspace.to_string());
                self.notify(MuxNotification::ActiveWorkspaceChanged(
                    client.client_id.clone(),
                ));
            }
        }
    }

    /// Overrides the current client identity.
    /// Returns `IdentityHolder` which will restore the prior identity
    /// when it is dropped.
    /// This can be used to change the identity for the duration of a block.
    pub fn with_identity(&self, id: Option<Arc<ClientId>>) -> IdentityHolder {
        let prior = self.replace_identity(id);
        IdentityHolder { prior }
    }

    /// Replace the identity, returning the prior identity
    pub fn replace_identity(&self, id: Option<Arc<ClientId>>) -> Option<Arc<ClientId>> {
        std::mem::replace(&mut *self.identity.write(), id)
    }

    /// Returns the active identity
    pub fn active_identity(&self) -> Option<Arc<ClientId>> {
        self.identity.read().clone()
    }

    pub fn unregister_client(&self, client_id: &ClientId) {
        self.clients.write().remove(client_id);
    }

    pub fn subscribe<F>(&self, subscriber: F)
    where
        F: Fn(MuxNotification) -> bool + 'static + Send + Sync,
    {
        let sub_id = SUB_ID.fetch_add(1, Ordering::Relaxed);
        self.subscribers
            .write()
            .insert(sub_id, Box::new(subscriber));
    }

    pub fn notify(&self, notification: MuxNotification) {
        let mut subscribers = self.subscribers.write();
        subscribers.retain(|_, notify| notify(notification.clone()));
    }

    pub fn notify_from_any_thread(notification: MuxNotification) {
        if let Some(mux) = Mux::try_get() {
            if mux.is_main_thread() {
                mux.notify(notification);
                return;
            }
        }
        promise::spawn::spawn_into_main_thread(async {
            if let Some(mux) = Mux::try_get() {
                mux.notify(notification);
            }
        })
        .detach();
    }

    pub fn default_domain(&self) -> Arc<dyn Domain> {
        self.default_domain.read().as_ref().map(Arc::clone).unwrap()
    }

    pub fn set_default_domain(&self, domain: &Arc<dyn Domain>) {
        *self.default_domain.write() = Some(Arc::clone(domain));
    }

    pub fn get_domain(&self, id: DomainId) -> Option<Arc<dyn Domain>> {
        self.domains.read().get(&id).cloned()
    }

    pub fn get_domain_by_name(&self, name: &str) -> Option<Arc<dyn Domain>> {
        self.domains_by_name.read().get(name).cloned()
    }

    pub fn add_domain(&self, domain: &Arc<dyn Domain>) {
        if self.default_domain.read().is_none() {
            *self.default_domain.write() = Some(Arc::clone(domain));
        }
        self.domains
            .write()
            .insert(domain.domain_id(), Arc::clone(domain));
        self.domains_by_name
            .write()
            .insert(domain.domain_name().to_string(), Arc::clone(domain));
    }

    pub fn set_mux(mux: &Arc<Mux>) {
        MUX.lock().replace(Arc::clone(mux));
    }

    pub fn shutdown() {
        // Explicitly clean up all panes before dropping the MUX.
        // This ensures child processes are signaled to terminate
        // and have a chance to clean up before we destroy the Mux.
        let pane_ids: Vec<PaneId> = {
            let mux_guard = MUX.lock();
            if let Some(mux) = mux_guard.as_ref() {
                mux.panes.read().keys().copied().collect()
            } else {
                return;
            }
        };

        for pane_id in pane_ids {
            let pane = {
                let mux_guard = MUX.lock();
                if let Some(mux) = mux_guard.as_ref() {
                    mux.get_pane(pane_id)
                } else {
                    None
                }
            };
            if let Some(pane) = pane {
                pane.kill();
            }
        }

        // Give processes a brief moment to react to kill signals
        #[cfg(windows)]
        std::thread::sleep(std::time::Duration::from_millis(100));

        MUX.lock().take();
    }

    pub fn get() -> Arc<Mux> {
        Self::try_get().unwrap()
    }

    pub fn try_get() -> Option<Arc<Mux>> {
        MUX.lock().as_ref().map(Arc::clone)
    }

    pub fn get_pane(&self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        self.panes.read().get(&pane_id).map(Arc::clone)
    }

    pub fn get_tab(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        self.tabs.read().get(&tab_id).map(Arc::clone)
    }

    pub fn add_pane(&self, pane: &Arc<dyn Pane>) -> Result<(), Error> {
        if self.panes.read().contains_key(&pane.pane_id()) {
            return Ok(());
        }

        let clipboard: Arc<dyn Clipboard> = Arc::new(MuxClipboard {
            pane_id: pane.pane_id(),
        });
        pane.set_clipboard(&clipboard);

        let downloader: Arc<dyn DownloadHandler> = Arc::new(MuxDownloader {});
        pane.set_download_handler(&downloader);

        self.panes.write().insert(pane.pane_id(), Arc::clone(pane));
        let pane_id = pane.pane_id();
        if let Some(reader) = pane.reader()? {
            let banner = self.banner.read().clone();
            let pane = Arc::downgrade(pane);
            thread::spawn(move || read_from_pane_pty(pane, banner, reader));
        }
        self.recompute_pane_count();
        self.notify(MuxNotification::PaneAdded(pane_id));
        Ok(())
    }

    pub fn add_tab_no_panes(&self, tab: &Arc<Tab>) {
        self.tabs.write().insert(tab.tab_id(), Arc::clone(tab));
        self.recompute_pane_count();
    }

    pub fn add_tab_and_active_pane(&self, tab: &Arc<Tab>) -> Result<(), Error> {
        self.tabs.write().insert(tab.tab_id(), Arc::clone(tab));
        let pane = tab
            .get_active_pane()
            .ok_or_else(|| anyhow!("tab MUST have an active pane"))?;
        self.add_pane(&pane)
    }

    fn remove_pane_internal(&self, pane_id: PaneId) {
        log::debug!("removing pane {}", pane_id);
        let mut changed = false;
        if let Some(pane) = self.panes.write().remove(&pane_id).clone() {
            log::debug!("killing pane {}", pane_id);
            pane.kill();
            self.notify(MuxNotification::PaneRemoved(pane_id));
            changed = true;
        }

        if changed {
            self.recompute_pane_count();
        }
    }

    fn remove_tab_internal(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        log::debug!("remove_tab_internal tab {}", tab_id);

        let tab = self.tabs.write().remove(&tab_id)?;

        if let Some(mut windows) = self.windows.try_write() {
            for w in windows.values_mut() {
                w.remove_by_id(tab_id);
            }
        }

        let mut pane_ids = vec![];
        for pos in tab.iter_panes_ignoring_zoom() {
            pane_ids.push(pos.pane.pane_id());
        }
        log::debug!("panes to remove: {pane_ids:?}");
        for pane_id in pane_ids {
            self.remove_pane_internal(pane_id);
        }
        self.recompute_pane_count();

        Some(tab)
    }

    fn remove_window_internal(&self, window_id: WindowId) {
        log::debug!("remove_window_internal {}", window_id);

        let window = self.windows.write().remove(&window_id);
        if let Some(window) = window {
            // Gather all the domains referenced by this window
            let mut domains_of_window = HashSet::new();
            for tab in window.iter() {
                for pane in tab.iter_panes_ignoring_zoom() {
                    domains_of_window.insert(pane.pane.domain_id());
                }
            }

            for domain_id in domains_of_window {
                if let Some(domain) = self.get_domain(domain_id) {
                    if domain.detachable() {
                        log::info!("detaching domain");
                        if let Err(err) = domain.detach() {
                            log::error!(
                                "while detaching domain {domain_id} {}: {err:#}",
                                domain.domain_name()
                            );
                        }
                    }
                }
            }

            for tab in window.iter() {
                self.remove_tab_internal(tab.tab_id());
            }
            self.notify(MuxNotification::WindowRemoved(window_id));
        }
        self.recompute_pane_count();
    }

    pub fn remove_pane(&self, pane_id: PaneId) {
        self.remove_pane_internal(pane_id);
        self.prune_dead_windows();
    }

    pub fn remove_tab(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        let tab = self.remove_tab_internal(tab_id);
        self.prune_dead_windows();
        tab
    }

    fn schedule_prune_dead_windows_retry(&self, reason: &'static str) {
        let Some(mux) = Mux::try_get() else {
            log::trace!(
                "prune_dead_windows: unable to schedule retry because mux is gone ({reason})"
            );
            return;
        };

        if !std::ptr::eq(self, Arc::as_ptr(&mux)) {
            log::trace!(
                "prune_dead_windows: skipping retry because mux instance is no longer current ({reason})"
            );
            return;
        }

        if mux
            .prune_dead_windows_retry_scheduled
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            log::trace!("prune_dead_windows: retry already scheduled ({reason})");
            return;
        }

        log::trace!("prune_dead_windows: scheduling retry ({reason})");
        promise::spawn::spawn_into_main_thread(async move {
            // Clear the flag before retrying so that a subsequent
            // lock-contention miss can schedule another retry.
            mux.prune_dead_windows_retry_scheduled
                .store(false, Ordering::SeqCst);
            log::trace!("prune_dead_windows: running scheduled retry");
            mux.prune_dead_windows();
        })
        .detach();
    }

    pub fn prune_dead_windows(&self) {
        if Activity::count() > 0 {
            // Activity::Drop already schedules another prune pass,
            // so preserve that behavior and avoid stacking retries here.
            log::trace!("prune_dead_windows: Activity::count={}", Activity::count());
            return;
        }
        let live_tab_ids: Vec<TabId> = self.tabs.read().keys().cloned().collect();
        let mut dead_windows = vec![];
        let dead_tab_ids: Vec<TabId>;

        {
            let mut windows = match self.windows.try_write() {
                Some(w) => w,
                None => {
                    // It's ok if our caller already locked it; we can prune later.
                    log::trace!(
                        "prune_dead_windows: self.windows already borrowed; scheduling retry"
                    );
                    self.schedule_prune_dead_windows_retry("self.windows already borrowed");
                    return;
                }
            };
            for (window_id, win) in windows.iter_mut() {
                win.prune_dead_tabs(&live_tab_ids);
                if win.is_empty() {
                    log::trace!("prune_dead_windows: window is now empty");
                    dead_windows.push(*window_id);
                }
            }

            dead_tab_ids = self
                .tabs
                .read()
                .iter()
                .filter_map(|(&id, tab)| if tab.is_dead() { Some(id) } else { None })
                .collect();
        }

        for tab_id in dead_tab_ids {
            log::trace!("tab {} is dead", tab_id);
            self.remove_tab_internal(tab_id);
        }

        for window_id in dead_windows {
            log::trace!("window {} is dead", window_id);
            self.remove_window_internal(window_id);
        }

        if self.is_empty() {
            log::trace!("prune_dead_windows: is_empty, send MuxNotification::Empty");
            self.notify(MuxNotification::Empty);
        } else {
            log::trace!("prune_dead_windows: not empty");
        }
    }

    pub fn kill_window(&self, window_id: WindowId) {
        self.remove_window_internal(window_id);
        self.prune_dead_windows();
    }

    pub fn get_window(&self, window_id: WindowId) -> Option<MappedRwLockReadGuard<'_, Window>> {
        if !self.windows.read().contains_key(&window_id) {
            return None;
        }
        Some(RwLockReadGuard::map(self.windows.read(), |windows| {
            windows.get(&window_id).unwrap()
        }))
    }

    pub fn get_window_mut(
        &self,
        window_id: WindowId,
    ) -> Option<MappedRwLockWriteGuard<'_, Window>> {
        if !self.windows.read().contains_key(&window_id) {
            return None;
        }
        Some(RwLockWriteGuard::map(self.windows.write(), |windows| {
            windows.get_mut(&window_id).unwrap()
        }))
    }

    pub fn get_active_tab_for_window(&self, window_id: WindowId) -> Option<Arc<Tab>> {
        let window = self.get_window(window_id)?;
        window.get_active().map(Arc::clone)
    }

    pub fn new_empty_window(
        &self,
        workspace: Option<String>,
        position: Option<GuiPosition>,
    ) -> MuxWindowBuilder {
        let window = Window::new(workspace, position);
        let window_id = window.window_id();
        self.windows.write().insert(window_id, window);
        MuxWindowBuilder {
            window_id,
            activity: Some(Activity::new()),
            notified: false,
        }
    }

    pub fn add_tab_to_window(&self, tab: &Arc<Tab>, window_id: WindowId) -> anyhow::Result<()> {
        let tab_id = tab.tab_id();
        {
            let mut window = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow!("add_tab_to_window: no such window_id {}", window_id))?;
            window.push(tab);
        }
        self.recompute_pane_count();
        self.notify(MuxNotification::TabAddedToWindow { tab_id, window_id });
        Ok(())
    }

    pub fn window_containing_tab(&self, tab_id: TabId) -> Option<WindowId> {
        for w in self.windows.read().values() {
            for t in w.iter() {
                if t.tab_id() == tab_id {
                    return Some(w.window_id());
                }
            }
        }
        None
    }

    pub fn is_empty(&self) -> bool {
        self.panes.read().is_empty()
    }

    pub fn is_workspace_empty(&self, workspace: &str) -> bool {
        *self
            .num_panes_by_workspace
            .read()
            .get(workspace)
            .unwrap_or(&0)
            == 0
    }

    pub fn is_active_workspace_empty(&self) -> bool {
        let workspace = self.active_workspace();
        self.is_workspace_empty(&workspace)
    }

    pub fn iter_panes(&self) -> Vec<Arc<dyn Pane>> {
        self.panes
            .read()
            .iter()
            .map(|(_, v)| Arc::clone(v))
            .collect()
    }

    pub fn iter_windows_in_workspace(&self, workspace: &str) -> Vec<WindowId> {
        let mut windows: Vec<WindowId> = self
            .windows
            .read()
            .iter()
            .filter_map(|(k, w)| {
                if w.get_workspace() == workspace {
                    Some(k)
                } else {
                    None
                }
            })
            .cloned()
            .collect();
        windows.sort();
        windows
    }

    pub fn iter_windows(&self) -> Vec<WindowId> {
        self.windows.read().keys().cloned().collect()
    }

    pub fn iter_domains(&self) -> Vec<Arc<dyn Domain>> {
        self.domains.read().values().cloned().collect()
    }

    pub fn resolve_pane_id(&self, pane_id: PaneId) -> Option<(DomainId, WindowId, TabId)> {
        let mut ids = None;
        for tab in self.tabs.read().values() {
            for p in tab.iter_panes_ignoring_zoom() {
                if p.pane.pane_id() == pane_id {
                    ids = Some((tab.tab_id(), p.pane.domain_id()));
                    break;
                }
            }
        }
        let (tab_id, domain_id) = ids?;
        let window_id = self.window_containing_tab(tab_id)?;
        Some((domain_id, window_id, tab_id))
    }

    pub fn domain_was_detached(&self, domain: DomainId) {
        let mut dead_panes = vec![];
        for pane in self.panes.read().values() {
            if pane.domain_id() == domain {
                dead_panes.push(pane.pane_id());
            }
        }

        {
            let mut windows = self.windows.write();
            for (_, win) in windows.iter_mut() {
                for tab in win.iter() {
                    tab.kill_panes_in_domain(domain);
                }
            }
        }

        log::info!("domain detached panes: {:?}", dead_panes);
        for pane_id in dead_panes {
            self.remove_pane_internal(pane_id);
        }

        self.prune_dead_windows();
    }

    pub fn set_banner(&self, banner: Option<String>) {
        *self.banner.write() = banner;
    }

    pub fn resolve_spawn_tab_domain(
        &self,
        // TODO: disambiguate with TabId
        pane_id: Option<PaneId>,
        domain: &config::keyassignment::SpawnTabDomain,
    ) -> anyhow::Result<Arc<dyn Domain>> {
        let domain = match domain {
            SpawnTabDomain::DefaultDomain => self.default_domain(),
            SpawnTabDomain::CurrentPaneDomain => match pane_id {
                Some(pane_id) => {
                    let (pane_domain_id, _window_id, _tab_id) = self
                        .resolve_pane_id(pane_id)
                        .ok_or_else(|| anyhow!("pane_id {} invalid", pane_id))?;
                    self.get_domain(pane_domain_id)
                        .expect("resolve_pane_id to give valid domain_id")
                }
                None => self.default_domain(),
            },
            SpawnTabDomain::DomainId(domain_id) => self
                .get_domain(*domain_id)
                .ok_or_else(|| anyhow!("domain id {} is invalid", domain_id))?,
            SpawnTabDomain::DomainName(name) => {
                self.get_domain_by_name(&name).ok_or_else(|| {
                    let names: Vec<String> = self
                        .domains_by_name
                        .read()
                        .keys()
                        .map(|name| format!("\"{name}\""))
                        .collect();
                    anyhow!(
                        "domain name \"{name}\" is invalid. Possible names are {}.",
                        names.join(", ")
                    )
                })?
            }
        };
        Ok(domain)
    }

    fn resolve_cwd(
        &self,
        command_dir: Option<String>,
        pane: Option<Arc<dyn Pane>>,
        target_domain: DomainId,
        policy: CachePolicy,
    ) -> Option<String> {
        command_dir.or_else(|| {
            match pane {
                Some(pane) if pane.domain_id() == target_domain => pane
                    .get_current_working_dir(policy)
                    .and_then(|url| {
                        percent_decode_str(url.path())
                            .decode_utf8()
                            .ok()
                            .map(|path| path.into_owned())
                    })
                    .map(|path| {
                        // On Windows the file URI can produce a path like:
                        // `/C:\Users` which is valid in a file URI, but the leading slash
                        // is not liked by the windows file APIs, so we strip it off here.
                        let bytes = path.as_bytes();
                        if bytes.len() > 2 && bytes[0] == b'/' && bytes[2] == b':' {
                            path[1..].to_owned()
                        } else {
                            path
                        }
                    }),
                _ => None,
            }
        })
    }

    pub async fn split_pane(
        &self,
        // TODO: disambiguate with TabId
        pane_id: PaneId,
        request: SplitRequest,
        source: SplitSource,
        domain: config::keyassignment::SpawnTabDomain,
    ) -> anyhow::Result<(Arc<dyn Pane>, TerminalSize)> {
        let (_pane_domain_id, window_id, tab_id) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} invalid", pane_id))?;

        let domain = self
            .resolve_spawn_tab_domain(Some(pane_id), &domain)
            .context("resolve_spawn_tab_domain")?;

        if domain.state() == DomainState::Detached {
            domain.attach(Some(window_id)).await?;
        }

        let current_pane = self
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} is invalid", pane_id))?;
        let term_config = current_pane.get_config();

        let source = match source {
            SplitSource::Spawn {
                command,
                command_dir,
            } => SplitSource::Spawn {
                command,
                command_dir: self.resolve_cwd(
                    command_dir,
                    Some(Arc::clone(&current_pane)),
                    domain.domain_id(),
                    CachePolicy::FetchImmediate,
                ),
            },
            other => other,
        };

        let pane = domain.split_pane(source, tab_id, pane_id, request).await?;
        if let Some(config) = term_config {
            pane.set_config(config);
        }

        // FIXME: clipboard

        let dims = pane.get_dimensions();

        let size = TerminalSize {
            cols: dims.cols,
            rows: dims.viewport_rows,
            pixel_height: 0, // FIXME: split pane pixel dimensions
            pixel_width: 0,
            dpi: dims.dpi,
        };

        Ok((pane, size))
    }

    pub async fn move_pane_to_new_tab(
        &self,
        pane_id: PaneId,
        window_id: Option<WindowId>,
        workspace_for_new_window: Option<String>,
    ) -> anyhow::Result<(Arc<Tab>, WindowId)> {
        let (domain_id, _src_window, src_tab) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {} not found", pane_id))?;

        let domain = self
            .get_domain(domain_id)
            .ok_or_else(|| anyhow::anyhow!("domain {domain_id} of pane {pane_id} not found"))?;

        if let Some((tab, window_id)) = domain
            .move_pane_to_new_tab(pane_id, window_id, workspace_for_new_window.clone())
            .await?
        {
            return Ok((tab, window_id));
        }

        let src_tab = match self.get_tab(src_tab) {
            Some(t) => t,
            None => anyhow::bail!("Invalid tab id {}", src_tab),
        };

        let window_builder;
        let (window_id, size) = if let Some(window_id) = window_id {
            let window = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow!("window_id {} not found on this server", window_id))?;
            let tab = window
                .get_active()
                .ok_or_else(|| anyhow!("window {} has no tabs", window_id))?;
            let size = tab.get_size();

            (window_id, size)
        } else {
            window_builder = self.new_empty_window(workspace_for_new_window, None);
            (*window_builder, src_tab.get_size())
        };

        let pane = src_tab
            .remove_pane(pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {} wasn't in its containing tab!?", pane_id))?;

        let tab = Arc::new(Tab::new(&size));
        tab.assign_pane(&pane);
        pane.resize(size)?;
        self.add_tab_and_active_pane(&tab)?;
        self.add_tab_to_window(&tab, window_id)?;

        if src_tab.is_dead() {
            self.remove_tab(src_tab.tab_id());
        }

        Ok((tab, window_id))
    }

    pub async fn spawn_tab_or_window(
        &self,
        window_id: Option<WindowId>,
        domain: SpawnTabDomain,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        size: TerminalSize,
        current_pane_id: Option<PaneId>,
        workspace_for_new_window: String,
        window_position: Option<GuiPosition>,
    ) -> anyhow::Result<(Arc<Tab>, Arc<dyn Pane>, WindowId)> {
        let domain = self
            .resolve_spawn_tab_domain(current_pane_id, &domain)
            .context("resolve_spawn_tab_domain")?;

        let window_builder;
        let term_config;

        let (window_id, size) = if let Some(window_id) = window_id {
            let window = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow!("window_id {} not found on this server", window_id))?;
            let tab = window
                .get_active()
                .ok_or_else(|| anyhow!("window {} has no tabs", window_id))?;
            let pane = tab
                .get_active_pane()
                .ok_or_else(|| anyhow!("active tab in window {} has no panes", window_id))?;
            term_config = pane.get_config();

            let size = tab.get_size();

            (window_id, size)
        } else {
            term_config = None;
            window_builder = self.new_empty_window(Some(workspace_for_new_window), window_position);
            (*window_builder, size)
        };

        if domain.state() == DomainState::Detached {
            domain.attach(Some(window_id)).await?;
        }

        let cwd = self.resolve_cwd(
            command_dir,
            match current_pane_id {
                Some(id) => {
                    // Only use the cwd from the current pane if the domain
                    // is the same as the one we are spawning into
                    let (current_domain_id, _, _) = self
                        .resolve_pane_id(id)
                        .ok_or_else(|| anyhow!("pane_id {} invalid", id))?;
                    if current_domain_id == domain.domain_id() {
                        self.get_pane(id)
                    } else {
                        None
                    }
                }
                None => None,
            },
            domain.domain_id(),
            CachePolicy::FetchImmediate,
        );

        let tab = domain
            .spawn(size, command.clone(), cwd.clone(), window_id)
            .await
            .with_context(|| {
                format!(
                    "Spawning in domain `{}`: {size:?} command={command:?} cwd={cwd:?}",
                    domain.domain_name()
                )
            })?;

        let pane = tab
            .get_active_pane()
            .ok_or_else(|| anyhow!("missing active pane on tab!?"))?;

        if let Some(config) = term_config {
            pane.set_config(config);
        }

        // FIXME: clipboard?

        let mut window = self
            .get_window_mut(window_id)
            .ok_or_else(|| anyhow!("no such window!?"))?;
        if let Some(idx) = window.idx_by_id(tab.tab_id()) {
            window.save_and_then_set_active(idx);
        }

        Ok((tab, pane, window_id))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use promise::spawn::SimpleExecutor;

    lazy_static::lazy_static! {
        static ref TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    }

    #[test]
    fn prune_dead_windows_removes_empty_window_immediately() {
        let _guard = TEST_LOCK.lock().unwrap();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        let window = Window::new(Some("test".to_string()), None);
        let window_id = window.window_id();
        mux.windows.write().insert(window_id, window);

        assert_eq!(mux.iter_windows(), vec![window_id]);
        mux.prune_dead_windows();

        assert!(mux.iter_windows().is_empty());
        assert!(!mux
            .prune_dead_windows_retry_scheduled
            .load(Ordering::SeqCst));

        Mux::shutdown();
    }

    #[test]
    fn prune_dead_windows_retries_after_window_lock_contention() {
        let _guard = TEST_LOCK.lock().unwrap();
        Mux::shutdown();

        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);

        let window = Window::new(Some("test".to_string()), None);
        let window_id = window.window_id();
        mux.windows.write().insert(window_id, window);

        let windows = mux.windows.write();
        mux.prune_dead_windows();
        assert!(mux
            .prune_dead_windows_retry_scheduled
            .load(Ordering::SeqCst));
        drop(windows);

        assert_eq!(mux.iter_windows(), vec![window_id]);

        executor.tick().expect("scheduled prune retry should run");

        assert!(mux.iter_windows().is_empty());
        assert!(!mux
            .prune_dead_windows_retry_scheduled
            .load(Ordering::SeqCst));

        Mux::shutdown();
    }

    #[test]
    fn prune_dead_windows_coalesces_multiple_retries() {
        let _guard = TEST_LOCK.lock().unwrap();
        Mux::shutdown();

        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);

        let window = Window::new(Some("test".to_string()), None);
        let window_id = window.window_id();
        mux.windows.write().insert(window_id, window);

        let windows = mux.windows.write();
        mux.prune_dead_windows();
        mux.prune_dead_windows();
        assert!(mux
            .prune_dead_windows_retry_scheduled
            .load(Ordering::SeqCst));
        drop(windows);

        executor.tick().expect("scheduled prune retry should run");

        assert!(mux.iter_windows().is_empty());
        assert!(!mux
            .prune_dead_windows_retry_scheduled
            .load(Ordering::SeqCst));

        Mux::shutdown();
    }

    #[test]
    fn prune_dead_windows_activity_deferral_is_resolved_by_activity_drop() {
        let _guard = TEST_LOCK.lock().unwrap();
        Mux::shutdown();

        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);

        let window = Window::new(Some("test".to_string()), None);
        let window_id = window.window_id();
        mux.windows.write().insert(window_id, window);

        let activity = Activity::new();
        mux.prune_dead_windows();
        assert_eq!(mux.iter_windows(), vec![window_id]);
        assert!(!mux
            .prune_dead_windows_retry_scheduled
            .load(Ordering::SeqCst));

        drop(activity);

        executor
            .tick()
            .expect("activity drop should schedule prune_dead_windows");

        assert!(mux.iter_windows().is_empty());

        Mux::shutdown();
    }
}

pub struct IdentityHolder {
    prior: Option<Arc<ClientId>>,
}

impl Drop for IdentityHolder {
    fn drop(&mut self) {
        if let Some(mux) = Mux::try_get() {
            mux.replace_identity(self.prior.take());
        }
    }
}

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum SessionTerminated {
    #[error("Process exited: {:?}", status)]
    ProcessStatus { status: ExitStatus },
    #[error("Error: {:?}", err)]
    Error { err: Error },
    #[error("Window Closed")]
    WindowClosed,
}

pub(crate) fn terminal_size_to_pty_size(size: TerminalSize) -> anyhow::Result<PtySize> {
    Ok(PtySize {
        rows: size.rows.try_into()?,
        cols: size.cols.try_into()?,
        pixel_height: size.pixel_height.try_into()?,
        pixel_width: size.pixel_width.try_into()?,
    })
}

struct MuxClipboard {
    pane_id: PaneId,
}

impl Clipboard for MuxClipboard {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        clipboard: Option<String>,
    ) -> anyhow::Result<()> {
        let mux =
            Mux::try_get().ok_or_else(|| anyhow::anyhow!("MuxClipboard::set_contents: no Mux?"))?;
        mux.notify(MuxNotification::AssignClipboard {
            pane_id: self.pane_id,
            selection,
            clipboard,
        });
        Ok(())
    }
}

struct MuxDownloader {}

impl wezterm_term::DownloadHandler for MuxDownloader {
    fn save_to_downloads(&self, name: Option<String>, data: Vec<u8>) {
        if let Some(mux) = Mux::try_get() {
            mux.notify(MuxNotification::SaveToDownloads {
                name,
                data: Arc::new(data),
            });
        }
    }
}
