#[cfg(unix)]
use command_group::UnixChildExt;
use command_group::{CommandGroup, GroupChild};
use log::{debug, info, warn};

use std::{
    collections::HashMap,
    fs::canonicalize,
    process::Child,
    sync::{
        mpsc::{channel, Receiver},
        Arc, Mutex,
    },
    time::Duration,
};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::gitignore;
use crate::ignore;
use crate::notification_filter::NotificationFilter;
use crate::pathop::PathOp;
use crate::signal::{self, Signal};
use crate::watcher::{Event, Watcher};

/// Behaviour to use when handling updates while the command is running.
#[derive(Clone, Copy, Debug)]
pub enum OnBusyUpdate {
    /// ignore updates while busy
    DoNothing,

    /// wait for the command to exit, then start a new one
    Queue,

    /// restart the command immediately
    Restart,

    /// send a signal only
    Signal,
}

impl Default for OnBusyUpdate {
    fn default() -> Self {
        Self::DoNothing
    }
}

pub trait Handler {
    /// Called through a manual request, such as an initial run.
    ///
    /// # Returns
    ///
    /// A `Result` which means:
    ///
    /// - `Err`: an error has occurred while processing, quit.
    /// - `Ok(true)`: everything is fine and the loop can continue.
    /// - `Ok(false)`: everything is fine but we should gracefully stop.
    fn on_manual(&self) -> Result<bool>;

    /// Called through a file-update request.
    ///
    /// # Parameters
    ///
    /// - `ops`: The list of events that triggered this update.
    ///
    /// # Returns
    ///
    /// A `Result` which means:
    ///
    /// - `Err`: an error has occurred while processing, quit.
    /// - `Ok(true)`: everything is fine and the loop can continue.
    /// - `Ok(false)`: everything is fine but we should gracefully stop.
    fn on_update(&self, ops: &[PathOp]) -> Result<bool>;

    /// Called once by `watch` at the very start.
    ///
    /// Not called again; any changes will never be picked up.
    ///
    /// The `Config` instance should be created using `ConfigBuilder` rather than direct initialisation
    /// to resist potential breaking changes (see semver policy on crate root).
    fn args(&self) -> Config;
}

/// Starts watching, and calls a handler when something happens.
///
/// Given an argument structure and a `Handler` type, starts the watcher loop, blocking until done.
pub fn watch<H>(handler: &H) -> Result<()>
where
    H: Handler,
{
    let args = handler.args();

    let mut paths = vec![];
    for path in &args.paths {
        paths.push(
            canonicalize(&path)
                .map_err(|e| Error::Canonicalization(path.to_string_lossy().into_owned(), e))?,
        );
    }

    let ignore = ignore::load(if args.no_ignore { &[] } else { &paths });
    let gitignore = gitignore::load(if args.no_vcs_ignore || args.no_ignore {
        &[]
    } else {
        &paths
    });
    let filter = NotificationFilter::new(&args.filters, &args.ignores, gitignore, ignore)?;

    let (tx, rx) = channel();

    #[cfg_attr(not(target_os = "linux"), allow(clippy::redundant_clone, unused_mut))]
    let mut maybe_watcher = Watcher::new(tx.clone(), &paths, args.poll, args.poll_interval);

    #[cfg(target_os = "linux")]
    if !args.poll {
        if let Err(notify::Error::Io(ref e)) = maybe_watcher {
            if e.raw_os_error() == Some(nix::libc::ENOSPC) {
                warn!("System notification limit is too small, falling back to polling mode. For better performance increase system limit:\n\tsysctl fs.inotify.max_user_watches=524288");
                maybe_watcher = Watcher::new(tx, &paths, true, args.poll_interval);
            }
        }
    }

    let watcher = maybe_watcher?;
    if watcher.is_polling() {
        warn!("Polling for changes every {:?}", args.poll_interval);
    }

    // Call handler initially, if necessary
    if args.run_initially && !handler.on_manual()? {
        return Ok(());
    }

    loop {
        debug!("Waiting for filesystem activity");
        let paths = wait_fs(&rx, &filter, args.debounce, args.no_meta);
        info!("Paths updated: {:?}", paths);

        if !handler.on_update(&paths)? {
            break;
        }
    }

    Ok(())
}

#[derive(Debug)]
pub enum ChildProcess {
    None,
    Grouped(GroupChild),
    Ungrouped(Child),
}

impl Default for ChildProcess {
    fn default() -> Self {
        ChildProcess::None
    }
}

impl ChildProcess {
    #[cfg(unix)]
    fn signal(&mut self, sig: Signal) -> Result<()> {
        match self {
            Self::None => Ok(()),
            Self::Grouped(c) => {
                debug!("Sending signal {} to process group id={}", sig, c.id());
                c.signal(sig)
            }
            Self::Ungrouped(c) => {
                debug!("Sending signal {} to process id={}", sig, c.id());
                c.signal(sig)
            }
        }
        .map_err(|e| e.into())
    }

    fn kill(&mut self) -> Result<()> {
        match self {
            Self::None => Ok(()),
            Self::Grouped(c) => {
                debug!("Killing process group id={}", c.id());
                c.kill()
            }
            Self::Ungrouped(c) => {
                debug!("Killing process id={}", c.id());
                c.kill()
            }
        }
        .map_err(|e| e.into())
    }

    fn is_running(&mut self) -> Result<bool> {
        match self {
            Self::None => Ok(false),
            Self::Grouped(c) => c.try_wait().map(|w| w.is_none()),
            Self::Ungrouped(c) => c.try_wait().map(|w| w.is_none()),
        }
        .map_err(|e| e.into())
    }

    fn wait(&mut self) -> Result<()> {
        match self {
            Self::None => Ok(()),
            Self::Grouped(c) => c.wait().map(drop),
            Self::Ungrouped(c) => c.wait().map(drop),
        }
        .map_err(|e| e.into())
    }
}

pub struct ExecHandler {
    args: Config,
    signal: Option<Signal>,
    child_process: Arc<Mutex<ChildProcess>>,
}

impl ExecHandler {
    pub fn new(args: Config) -> Result<Self> {
        let child_process: Arc<Mutex<ChildProcess>> = Arc::default();
        let weak_child = Arc::downgrade(&child_process);

        // Convert signal string to the corresponding integer
        let signal = signal::new(args.signal.clone());

        signal::install_handler(move |sig: Signal| {
            if let Some(lock) = weak_child.upgrade() {
                let mut child = lock.lock().expect("poisoned lock in install_handler");
                match sig {
                    Signal::SIGCHLD => {
                        child.is_running().ok();
                    }
                    _ => {
                        #[cfg(unix)]
                        child.signal(sig).unwrap_or_else(|err| {
                            warn!("Could not pass on signal to command: {}", err)
                        });

                        #[cfg(not(unix))]
                        child.kill().unwrap_or_else(|err| {
                            warn!("Could not pass on termination to command: {}", err)
                        });
                    }
                }
            }
        });

        Ok(Self {
            args,
            signal,
            child_process,
        })
    }

    fn spawn(&self, ops: &[PathOp]) -> Result<()> {
        if self.args.clear_screen {
            clearscreen::clear()?;
        }

        let mut child = self.child_process.lock()?;
        child.kill().ok();

        let mut command = self.args.shell.to_command(&self.args.cmd);
        debug!("Assembled command: {:?}", command);

        if !self.args.no_environment {
            for (name, val) in crate::paths::collect_path_env_vars(ops) {
                debug!("Command environment: {}={:?}", name, val);
                command.env(name, val);
            }
        }

        debug!("Launching command");
        *child = if self.args.use_process_group {
            ChildProcess::Grouped(command.group_spawn()?)
        } else {
            ChildProcess::Ungrouped(command.spawn()?)
        };

        Ok(())
    }

    pub fn has_running_process(&self) -> Result<bool> {
        self.child_process
            .lock()
            .expect("poisoned lock in has_running_process")
            .is_running()
    }
}

impl Handler for ExecHandler {
    fn args(&self) -> Config {
        self.args.clone()
    }

    // Only returns Err() on lock poisoning.
    fn on_manual(&self) -> Result<bool> {
        if self.args.once {
            return Ok(true);
        }

        self.spawn(&[])?;
        Ok(true)
    }

    fn on_update(&self, ops: &[PathOp]) -> Result<bool> {
        log::debug!("ON UPDATE: called");

        let signal = self.signal.unwrap_or(Signal::SIGTERM);
        let has_running_processes = self.has_running_process()?;

        log::debug!(
            "ON UPDATE: has_running_processes: {} --- on_busy_update: {:?}",
            has_running_processes,
            self.args.on_busy_update
        );

        match (has_running_processes, self.args.on_busy_update) {
            // If nothing is running, start the command
            (false, _) => {
                self.spawn(ops)?;
            }

            // Just send a signal to the command, do nothing more
            (true, OnBusyUpdate::Signal) => signal_process(&self.child_process, signal)?,

            // Send a signal to the command, wait for it to exit, then run the command again
            (true, OnBusyUpdate::Restart) => {
                signal_process(&self.child_process, signal)?;
                wait_on_process(&self.child_process)?;
                self.spawn(ops)?;
            }

            // Wait for the command to end, then run it again
            (true, OnBusyUpdate::Queue) => {
                wait_on_process(&self.child_process)?;
                self.spawn(ops)?;
            }

            (true, OnBusyUpdate::DoNothing) => {}
        }

        // Handle once option for integration testing
        if self.args.once {
            if let Some(signal) = self.signal {
                signal_process(&self.child_process, signal)?;
            }

            wait_on_process(&self.child_process)?;

            return Ok(false);
        }

        Ok(true)
    }
}

pub fn run(args: Config) -> Result<()> {
    watch(&ExecHandler::new(args)?)
}

fn wait_fs(
    rx: &Receiver<Event>,
    filter: &NotificationFilter,
    debounce: Duration,
    no_meta: bool,
) -> Vec<PathOp> {
    let mut paths = Vec::new();
    let mut cache = HashMap::new();

    loop {
        let e = rx.recv().expect("error when reading event");

        if let Some(ref path) = e.path {
            let pathop = PathOp::new(path, e.op.ok(), e.cookie);
            if let Some(op) = pathop.op {
                if no_meta && PathOp::is_meta(op) {
                    continue;
                }
            }

            // Ignore cache for the initial file. Otherwise, in
            // debug mode it's hard to track what's going on
            let excluded = filter.is_excluded(path);
            if !cache.contains_key(&pathop) {
                cache.insert(pathop.clone(), excluded);
            }

            if !excluded {
                paths.push(pathop);
                break;
            }
        }
    }

    // Wait for filesystem activity to cool off
    while let Ok(e) = rx.recv_timeout(debounce) {
        if let Some(ref path) = e.path {
            let pathop = PathOp::new(path, e.op.ok(), e.cookie);
            if cache.contains_key(&pathop) {
                continue;
            }

            let excluded = filter.is_excluded(path);

            cache.insert(pathop.clone(), excluded);

            if !excluded {
                paths.push(pathop);
            }
        }
    }

    paths
}

fn signal_process(process: &Mutex<ChildProcess>, signal: Signal) -> Result<()> {
    let mut child = process.lock().expect("poisoned lock in signal_process");

    #[cfg(unix)]
    child.signal(signal)?;

    #[cfg(not(unix))]
    if matches!(signal, Signal::SIGTERM | Signal::SIGKILL) {
        child.kill()?;
    } else {
        debug!("Ignoring signal to send to process");
    }

    Ok(())
}

fn wait_on_process(process: &Mutex<ChildProcess>) -> Result<()> {
    process
        .lock()
        .expect("poisoned lock in wait_on_process")
        .wait()
}
