//! The local terminal in raw mode (Unix), for an interactive `connect`.
//!
//! Raw mode hands every key to the port as typed: no line editing, no echo,
//! and no signals, so Ctrl-C and Ctrl-Z reach the device as 0x03 and 0x1A
//! and the escape key is the only way out from the keyboard. Output
//! processing is off too, which is why [`ui::set_raw`] switches the CLI's own
//! messages to CR LF while raw mode is on.
//!
//! Only stdin's terminal is touched, and only when stdin is one: a pipe on
//! stdin must not put the controlling terminal into raw mode behind the
//! user's back.
//!
//! A stop from outside (`kill -STOP`, or SIGTSTP from another process) lets
//! the shell put its own settings back; on SIGCONT raw mode is set again.

use std::io;
use std::sync::{Mutex, Once, PoisonError};

use rustix::stdio::stdin;
use rustix::termios::{OptionalActions, Termios, tcgetattr, tcsetattr};

use crate::shutdown::{self, Cleanup};
use crate::ui;

/// The settings to restore while raw mode is on, for the SIGCONT thread.
/// The lock also orders a SIGCONT against the final restore, so that the
/// terminal never ends up raw after it.
static SAVED: Mutex<Option<Termios>> = Mutex::new(None);

/// Stdin's terminal in raw mode. The settings it had before are restored
/// when this is dropped, and by the cleanup registry however else the
/// process ends.
#[derive(Debug)]
pub(crate) struct RawMode {
    saved: Termios,
    restore: Cleanup,
}

impl RawMode {
    /// Puts stdin's terminal into raw mode.
    ///
    /// # Errors
    /// If stdin is not a terminal, or its settings cannot be read or set.
    pub(crate) fn enter() -> io::Result<Self> {
        let saved = tcgetattr(stdin())?;
        *lock_saved() = Some(saved.clone());
        let restore = shutdown::register({
            let saved = saved.clone();
            move || {
                let mut current = lock_saved();
                *current = None;
                restore(&saved);
            }
        });
        let raw = Self { saved, restore };
        // On an error the restore runs on drop: harmless, nothing changed.
        make_raw(&raw.saved)?;
        static CONTINUE: Once = Once::new();
        CONTINUE.call_once(raw_again_on_continue);
        Ok(raw)
    }

    /// Gives the terminal back as it was and stops the process (SIGTSTP),
    /// as Ctrl-Z would outside raw mode; raw mode again once the shell
    /// continues the process. `raise` returns only then, or at once in an
    /// orphaned process group, where the kernel discards stop signals.
    ///
    /// # Errors
    /// If raw mode cannot be set again: the session must not go on on a
    /// cooked terminal.
    pub(crate) fn suspend(&self) -> io::Result<()> {
        restore(&self.saved);
        if let Err(e) = signal_hook::low_level::raise(signal_hook::consts::SIGTSTP) {
            ui::warn(format_args!("cannot suspend: {e}"));
        }
        make_raw(&self.saved)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        shutdown::run(self.restore);
    }
}

/// A panic anywhere ends the process, so a poisoned lock only means that the
/// thread holding it is gone.
fn lock_saved() -> std::sync::MutexGuard<'static, Option<Termios>> {
    SAVED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sets raw mode again on every SIGCONT while it is on. After `z` the
/// suspend itself does too; twice is harmless.
fn raw_again_on_continue() {
    use signal_hook::consts::SIGCONT;
    use signal_hook::iterator::Signals;

    let mut signals = match Signals::new([SIGCONT]) {
        Ok(signals) => signals,
        Err(e) => {
            ui::warn(format_args!(
                "cannot catch SIGCONT ({e}): after a stop from outside, the terminal stays as \
                 the shell left it"
            ));
            return;
        }
    };
    let spawned = std::thread::Builder::new()
        .name("continue".into())
        .spawn(move || {
            for _ in signals.forever() {
                if let Some(saved) = lock_saved().as_ref() {
                    // Continued in the background (`bg`), this stops the
                    // process again (SIGTTOU) until `fg`, and then succeeds.
                    let _ = make_raw(saved);
                }
            }
        });
    if let Err(e) = spawned {
        ui::warn(format_args!("cannot catch SIGCONT: {e}"));
    }
}

/// Raw mode derived from the saved settings (`cfmakeraw`), so that a
/// resumed session starts from the user's terminal settings, not from
/// whatever the shell left behind.
fn make_raw(saved: &Termios) -> io::Result<()> {
    let mut raw = saved.clone();
    raw.make_raw();
    tcsetattr(stdin(), OptionalActions::Now, &raw)?;
    ui::set_raw(true);
    Ok(())
}

/// Puts the saved settings back. Errors are ignored: this runs on the way
/// out, also from a signal or a panic, and a terminal that is gone (SIGHUP)
/// needs nothing restored.
fn restore(saved: &Termios) {
    // `Now`: the line discipline processed what was written already, so
    // nothing in flight changes meaning, and a stalled terminal cannot
    // keep the process from exiting.
    let _ = tcsetattr(stdin(), OptionalActions::Now, saved);
    ui::set_raw(false);
}
