//! The local terminal in raw mode (Unix), for an interactive `connect`.
//!
//! Raw mode hands every key to the port as typed: no line editing, no echo,
//! and no signals, so Ctrl-C and Ctrl-Z reach the device as 0x03 and 0x1A
//! (review finding cn-2) and the escape key is the only way out from the
//! keyboard. Output processing is off too, which is why [`ui::set_raw`]
//! switches the CLI's own messages to CR LF while raw mode is on.
//!
//! Only stdin's terminal is touched, and only when stdin is one: a pipe on
//! stdin must not put the controlling terminal into raw mode behind the
//! user's back (finding cn-4).

use std::io;

use rustix::stdio::stdin;
use rustix::termios::{OptionalActions, Termios, tcgetattr, tcsetattr};

use crate::shutdown::{self, Cleanup};
use crate::ui;

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
        let restore = shutdown::register({
            let saved = saved.clone();
            move || restore(&saved)
        });
        let raw = Self { saved, restore };
        // On an error the restore runs on drop: harmless, nothing changed.
        make_raw(&raw.saved)?;
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
