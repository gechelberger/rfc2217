//! How the process ends: the cleanup actions that must run however it ends
//! (restore the terminal; in `pty`, remove the `--link` symlink), the
//! signals that end it, and the panic hook.
//!
//! [`std::process::exit`] runs no destructors, on any thread, and a worker
//! that panics never reaches the owner's: so the actions live in a registry
//! rather than in `Drop` impls alone, and each runs
//! exactly once, from whichever of these paths gets there first:
//!
//! - `main` returning, with any [`Exit`](crate::Exit): it calls [`run_all`].
//! - A caught signal (Unix: SIGINT, SIGTERM, SIGHUP, SIGQUIT). The signal
//!   thread hands it to the running session ([`on_signal`]), which ends with
//!   `Exit::Signal(n)`, so that `main` runs the registry and exits 128 + n.
//!   If no session listens, or it has not finished within [`GRACE`], the
//!   signal thread runs the registry and exits itself.
//! - A panic on any thread: the hook runs the registry, prints the panic and
//!   exits 101. A session must never carry on after one of its threads
//!   died: with the terminal restored under it, it would carry on with local
//!   echo and Ctrl-C killing it.
//!
//! A signal that was ignored when the process started stays ignored, as
//! `nohup` and a shell's background jobs expect: SIGHUP, SIGINT and SIGQUIT
//! are caught only if they were not. SIGTERM is always caught.
//!
//! On other systems no signals are caught (Ctrl-C ends the process, and no
//! terminal is in raw mode there); panics still run the registry.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, ThreadId};
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use crate::ui;

/// How long the signal thread waits for the session to end on its own
/// before it runs the registry and exits itself.
#[cfg(unix)]
const GRACE: Duration = Duration::from_secs(1);

type Action = Box<dyn FnOnce() + Send>;

/// The registered actions not run yet, oldest first, under one lock.
struct Registry {
    actions: Mutex<Actions>,
    /// Signalled whenever an action has finished.
    finished: Condvar,
}

// Only Unix registers anything (raw mode, the pty's link): elsewhere the
// registry stays empty.
#[cfg_attr(not(unix), allow(dead_code))]
struct Actions {
    next: u64,
    list: Vec<(Cleanup, Action)>,
    /// The threads running an action now, one entry per action. The process
    /// exits right after a `run_all` returns (the signal thread, the panic
    /// hook), so `run_all` waits for the actions other threads are in.
    running: Vec<ThreadId>,
}

/// One registered action, to run it early with [`run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cleanup(u64);

#[cfg_attr(not(unix), allow(dead_code))]
impl Registry {
    const fn new() -> Self {
        Self {
            actions: Mutex::new(Actions {
                next: 0,
                list: Vec::new(),
                running: Vec::new(),
            }),
            finished: Condvar::new(),
        }
    }

    fn register(&self, action: Action) -> Cleanup {
        let mut actions = lock(&self.actions);
        let id = Cleanup(actions.next);
        actions.next += 1;
        actions.list.push((id, action));
        id
    }

    /// Runs `id`'s action, unless it already ran.
    fn run(&self, id: Cleanup) {
        let mut actions = lock(&self.actions);
        if let Some(at) = actions.list.iter().position(|(other, _)| *other == id) {
            let (_, action) = actions.list.remove(at);
            self.run_one(actions, action);
        }
    }

    /// Runs every action not run yet, newest first: what was set up last is
    /// undone first. Returns once none is left and no other thread is still
    /// in one.
    fn run_all(&self) {
        let me = thread::current().id();
        let mut actions = lock(&self.actions);
        loop {
            // One at a time: if an action panics, the panic hook's run_all
            // (on this thread) takes the ones left.
            if let Some((_, action)) = actions.list.pop() {
                self.run_one(actions, action);
                actions = lock(&self.actions);
            } else if actions.running.iter().any(|&thread| thread != me) {
                actions = self
                    .finished
                    .wait(actions)
                    .unwrap_or_else(PoisonError::into_inner);
            } else {
                return;
            }
        }
    }

    /// Runs `action`, just taken from the list, outside the lock: an action
    /// that panics reaches the panic hook, which takes the lock again.
    fn run_one(&self, mut actions: MutexGuard<'_, Actions>, action: Action) {
        let me = thread::current().id();
        actions.running.push(me);
        drop(actions);
        action();
        let mut actions = lock(&self.actions);
        if let Some(at) = actions.running.iter().position(|&thread| thread == me) {
            actions.running.swap_remove(at);
        }
        self.finished.notify_all();
    }
}

static REGISTRY: Registry = Registry::new();

/// Where a caught signal goes while a session runs.
type Sink = Box<dyn Fn(i32) + Send>;

static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// A panic anywhere ends the process (see the panic hook), so a poisoned
/// lock only means that the thread holding it is gone: the data is still
/// consistent, since nothing here panics while holding one.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Installs the panic hook and, on Unix, the signal thread. Called first in
/// `main`, before anything needs cleaning up.
pub(crate) fn install() {
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Restore first: the report is unreadable on a raw terminal.
        REGISTRY.run_all();
        report(info);
        std::process::exit(101);
    }));
    #[cfg(unix)]
    catch_signals();
}

/// Registers `action` to run once when the process ends, however it ends,
/// or earlier through [`run`]. It must not block: the process waits for it
/// on its way out.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn register(action: impl FnOnce() + Send + 'static) -> Cleanup {
    REGISTRY.register(Box::new(action))
}

/// Runs one action now, unless it already ran.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn run(cleanup: Cleanup) {
    REGISTRY.run(cleanup);
}

/// Runs every action that has not run yet: `main` calls this on its way out.
pub(crate) fn run_all() {
    REGISTRY.run_all();
}

/// While the returned guard lives, a caught signal is handed to `sink`
/// (the session's channel) instead of ending the process at once. The sink
/// must not block: the signal thread calls it.
pub(crate) fn on_signal(sink: impl Fn(i32) + Send + 'static) -> SignalSink {
    *lock(&SINK) = Some(Box::new(sink));
    SignalSink(())
}

/// Hands signals to a session's sink while it lives (see [`on_signal`]).
#[derive(Debug)]
pub(crate) struct SignalSink(());

impl Drop for SignalSink {
    fn drop(&mut self) {
        *lock(&SINK) = None;
    }
}

#[cfg(unix)]
fn catch_signals() {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
    use signal_hook::iterator::Signals;

    let caught = [SIGINT, SIGHUP, SIGQUIT]
        .into_iter()
        .filter(|&signal| !ignored(signal))
        .chain([SIGTERM]);
    let mut signals = match Signals::new(caught) {
        Ok(signals) => signals,
        Err(e) => {
            ui::warn(format_args!(
                "cannot catch signals ({e}): one may leave the terminal in raw mode"
            ));
            return;
        }
    };
    let spawned = std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            // The first signal ends the process; any later one waits here.
            let Some(signal) = signals.forever().next() else {
                return;
            };
            // Nothing on this path writes to stderr: what keeps a session
            // from ending is a stderr that nobody reads, and this thread
            // must still get to its exit below. The session logs the signal.
            let told = lock(&SINK).as_ref().map(|sink| sink(signal)).is_some();
            if told {
                // The session ends with Exit::Signal and `main` runs the
                // registry and exits, unless something keeps it.
                std::thread::sleep(GRACE);
            }
            // Not even through a cleanup action's warning.
            ui::silence();
            REGISTRY.run_all();
            std::process::exit(128 + signal);
        });
    if let Err(e) = spawned {
        ui::warn(format_args!("cannot catch signals: {e}"));
    }
}

/// Whether `signal` was ignored when the process started (`SIG_IGN`).
#[cfg(unix)]
#[allow(unsafe_code)]
fn ignored(signal: i32) -> bool {
    let mut old = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: with a null new action, sigaction only reads the current one
    // into `old`, which is valid for writes; nothing changes.
    let read = unsafe { libc::sigaction(signal, std::ptr::null(), old.as_mut_ptr()) } == 0;
    // SAFETY: zeroed is a valid sigaction (plain integers and pointers), and
    // a successful call filled it in.
    read && unsafe { old.assume_init() }.sa_sigaction == libc::SIG_IGN
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use super::*;

    /// A registry and a log of the actions that ran, in order.
    fn registry() -> (Registry, Arc<Mutex<Vec<&'static str>>>) {
        (Registry::new(), Arc::default())
    }

    fn logs(log: &Arc<Mutex<Vec<&'static str>>>, name: &'static str) -> Action {
        let log = Arc::clone(log);
        Box::new(move || lock(&log).push(name))
    }

    #[test]
    fn run_all_runs_each_action_once_newest_first() {
        let (registry, log) = registry();
        registry.register(logs(&log, "terminal"));
        registry.register(logs(&log, "link"));
        registry.run_all();
        registry.run_all();
        assert_eq!(*lock(&log), ["link", "terminal"]);
    }

    #[test]
    fn an_action_run_early_does_not_run_again() {
        let (registry, log) = registry();
        let terminal = registry.register(logs(&log, "terminal"));
        registry.register(logs(&log, "link"));
        registry.run(terminal);
        registry.run(terminal);
        registry.run_all();
        assert_eq!(*lock(&log), ["terminal", "link"]);
    }

    #[test]
    fn run_all_waits_for_an_action_another_thread_runs() {
        // The signal thread exits right after its run_all: that must not cut
        // off the terminal restore that `main` is running at that moment.
        let registry = Arc::new(Registry::new());
        let (_, log) = self::registry();
        let (started, has_started) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        registry.register(Box::new({
            let log = Arc::clone(&log);
            move || {
                let _ = started.send(());
                let _ = released.recv_timeout(Duration::from_secs(10));
                lock(&log).push("restored");
            }
        }));
        let first = thread::spawn({
            let registry = Arc::clone(&registry);
            move || registry.run_all()
        });
        has_started
            .recv_timeout(Duration::from_secs(10))
            .expect("the action started");
        let (returned, has_returned) = mpsc::channel();
        let second = thread::spawn({
            let (registry, log) = (Arc::clone(&registry), Arc::clone(&log));
            move || {
                registry.run_all();
                lock(&log).push("second returned");
                let _ = returned.send(());
            }
        });
        // A second run_all that does not wait returns now.
        let _ = has_returned.recv_timeout(Duration::from_millis(200));
        release.send(()).expect("the action waits");
        first.join().expect("first run_all");
        second.join().expect("second run_all");
        assert_eq!(*lock(&log), ["restored", "second returned"]);
    }

    #[test]
    fn the_actions_left_run_when_one_ends_the_process() {
        // An action that panics reaches the panic hook, whose run_all must
        // run the older actions: the hook exits, and the run_all that ran
        // the panicking action never goes on.
        let registry = Arc::new(Registry::new());
        let (_, log) = self::registry();
        registry.register(logs(&log, "terminal"));
        registry.register(Box::new({
            let inner = Arc::clone(&registry);
            move || {
                inner.run_all();
                // As `process::exit` would: the outer run_all does not go on.
                std::panic::resume_unwind(Box::new("exit"));
            }
        }));
        let outer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| registry.run_all()));
        assert!(outer.is_err());
        assert_eq!(*lock(&log), ["terminal"]);
    }

    #[test]
    fn an_action_may_use_the_registry() {
        // The panic hook calls run_all from inside a panicking action: the
        // lock must not be held while an action runs.
        let registry = Arc::new(Registry::new());
        let (_, log) = self::registry();
        let inner = Arc::clone(&registry);
        registry.register(Box::new(move || inner.run_all()));
        registry.register(logs(&log, "link"));
        registry.run_all();
        assert_eq!(*lock(&log), ["link"]);
    }
}
