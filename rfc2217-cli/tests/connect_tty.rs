//! `rfc2217 connect` on a terminal (Unix). The binary runs in a session of
//! its own with a pty as its controlling terminal, as it would on a user's
//! terminal: without one, Ctrl-C never raises SIGINT, and a CLI that left
//! ISIG on would pass every test. A CI runner has no terminal at all, so the
//! test makes its own pty.
//!
//! The test types on the master; the CLI's stdout is the slave, so what the
//! echo bridge sends back shows up on the master. stderr is a pipe, which
//! keeps the CLI's messages out of the data.
//!
//! The last test sends a signal to a session without a terminal: it needs
//! Unix too. Linux and macOS only, as `pty`: the harness needs `ptsname`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use common::{BIN, Bridge, DEADLINE, Options, Script, Seen, scripted, wait};
use rfc2217::ServerEvent;
use rustix::fs::{Mode, OFlags};
use rustix::process::{Pid, Signal, kill_process};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{LocalModes, OptionalActions, SpecialCodeIndex, Termios, tcgetattr};

/// The escape key, ^].
const ESC: u8 = 0x1D;

/// A pty pair, and the slave's settings before anything ran on it.
struct Pty {
    master: File,
    slave: File,
    before: Termios,
}

fn pty() -> Pty {
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("openpt");
    grantpt(&master).expect("grantpt");
    unlockpt(&master).expect("unlockpt");
    let name = ptsname(&master, Vec::new()).expect("ptsname");
    // NOCTTY: the test process must not adopt it as its own terminal.
    let slave = rustix::fs::open(
        name.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY,
        Mode::empty(),
    )
    .expect("open the pty slave");
    let before = tcgetattr(&slave).expect("tcgetattr");
    Pty {
        master: File::from(master),
        slave: File::from(slave),
        before,
    }
}

/// Bytes from a reader thread, searched as they arrive.
struct Feed {
    rx: Receiver<Vec<u8>>,
    /// Everything so far.
    all: Vec<u8>,
    /// Where the next search starts: after the last match.
    at: usize,
}

impl Feed {
    fn new(mut source: impl Read + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buf = [0; 4096];
            // A pty master reads EIO once every slave descriptor is closed.
            while let Ok(n @ 1..) = source.read(&mut buf) {
                if tx.send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
        });
        Self {
            rx,
            all: Vec::new(),
            at: 0,
        }
    }

    /// Waits until `want` arrives, after what earlier calls found.
    fn expect(&mut self, what: &str, want: &[u8]) {
        let end = Instant::now() + DEADLINE;
        loop {
            let rest = &self.all[self.at..];
            if let Some(i) = rest.windows(want.len()).position(|w| w == want) {
                self.at += i + want.len();
                return;
            }
            match self
                .rx
                .recv_timeout(end.saturating_duration_since(Instant::now()))
            {
                Ok(bytes) => self.all.extend(bytes),
                Err(_) => panic!(
                    "waited for {want:?} on {what}, which showed {:?}",
                    String::from_utf8_lossy(&self.all)
                ),
            }
        }
    }

    /// Everything, once the source has ended.
    fn all(mut self) -> String {
        let end = Instant::now() + DEADLINE;
        while let Ok(bytes) = self
            .rx
            .recv_timeout(end.saturating_duration_since(Instant::now()))
        {
            self.all.extend(bytes);
        }
        String::from_utf8_lossy(&self.all).into_owned()
    }
}

/// A process on the pty: what it shows there, and its messages.
struct Session {
    child: Child,
    screen: Feed,
    stderr: Feed,
    master: File,
}

/// Starts `program` with the slave as stdin, stdout and controlling
/// terminal, in a new session; stderr is a pipe.
fn spawn(pty: &Pty, program: &str, args: &[&str]) -> Session {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(pty.slave.try_clone().expect("dup"))
        .stdout(pty.slave.try_clone().expect("dup"))
        .stderr(Stdio::piped());
    // SAFETY: the closure runs in the child between fork and exec, where
    // only async-signal-safe calls are allowed: it makes two system calls
    // (setsid, ioctl) and neither allocates nor takes a lock.
    #[allow(unsafe_code)]
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid()?;
            rustix::process::ioctl_tiocsctty(rustix::stdio::stdin())?;
            Ok(())
        });
    }
    let mut child = command.spawn().expect("spawn");
    let stderr = Feed::new(child.stderr.take().expect("piped stderr"));
    Session {
        child,
        screen: Feed::new(pty.master.try_clone().expect("dup")),
        stderr,
        master: pty.master.try_clone().expect("dup"),
    }
}

impl Session {
    fn connect(pty: &Pty, bridge: &Bridge) -> Self {
        let session = spawn(pty, BIN, &["connect", &bridge.target()]);
        wait_raw(pty);
        session
    }

    fn type_keys(&mut self, keys: &[u8]) {
        self.master
            .write_all(keys)
            .expect("write to the pty master");
    }

    /// Waits until the session has settled, so that commands reach the
    /// bridge at once: a key typed now comes back only then.
    fn settle(&mut self) {
        self.type_keys(b"~");
        self.expect(b"~");
    }

    /// Waits until `want` shows on the terminal.
    fn expect(&mut self, want: &[u8]) {
        self.screen.expect("the terminal", want);
    }

    /// Waits until the CLI says `want` on stderr.
    fn expect_message(&mut self, want: &str) {
        self.stderr.expect("stderr", want.as_bytes());
    }

    fn alive(&mut self) -> bool {
        self.child.try_wait().expect("try_wait").is_none()
    }

    /// Waits for the exit; returns the status and all of stderr.
    fn exit(mut self) -> (std::process::ExitStatus, String) {
        let status = wait(&mut self.child);
        (status, self.stderr.all())
    }
}

/// Waits until the CLI has put the terminal into raw mode: only then are
/// keys like ^C plain bytes.
fn wait_raw(pty: &Pty) {
    let end = Instant::now() + DEADLINE;
    while tcgetattr(&pty.slave)
        .expect("tcgetattr")
        .local_modes
        .contains(LocalModes::ICANON)
    {
        assert!(
            Instant::now() < end,
            "the CLI never put the terminal into raw mode"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

/// The terminal has its settings from before the CLI ran.
fn assert_restored(pty: &Pty) {
    let now = tcgetattr(&pty.slave).expect("tcgetattr");
    let before = &pty.before;
    assert_eq!(now.input_modes, before.input_modes);
    assert_eq!(now.output_modes, before.output_modes);
    assert_eq!(now.control_modes, before.control_modes);
    assert_eq!(now.local_modes, before.local_modes);
    for index in [
        SpecialCodeIndex::VMIN,
        SpecialCodeIndex::VTIME,
        SpecialCodeIndex::VINTR,
    ] {
        assert_eq!(now.special_codes[index], before.special_codes[index]);
    }
}

#[test]
fn the_harness_delivers_ctrl_c_as_a_signal() {
    // The control case: `cat` on a cooked terminal dies on ^C. Without it,
    // a CLI that left ISIG on could pass the tests below. (It fails where
    // the tests inherit SIGINT ignored, as `cargo test &` in a script does:
    // `cat` inherits that too.)
    let pty = pty();
    let mut cat = spawn(&pty, "cat", &[]);
    cat.type_keys(b"ping\n");
    cat.expect(b"ping\r\nping\r\n");
    cat.type_keys(&[0x03]);
    let (status, _) = cat.exit();
    assert_eq!(status.signal(), Some(Signal::INT.as_raw()), "{status:?}");
}

#[test]
fn typed_bytes_reach_the_port_and_ctrl_c_does_not_kill() {
    let mut bridge = Bridge::echo();
    let pty = pty();
    let mut session = Session::connect(&pty, &bridge);
    let typed = b"ab\x03cd\x1a\x1c";
    session.type_keys(typed);
    session.expect(typed);
    assert!(session.alive(), "^C, ^Z or ^\\ ended the CLI");
    bridge.wait_for_data(typed);
    session.type_keys(&[ESC, b'q']);
    let (status, stderr) = session.exit();
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("the escape key is ^]"), "{stderr}");
}

#[test]
fn escape_q_quits_and_restores_the_terminal() {
    let bridge = Bridge::echo();
    let pty = pty();
    let mut session = Session::connect(&pty, &bridge);
    session.type_keys(b"hi\r");
    session.expect(b"hi\r");
    session.type_keys(&[ESC, b'q']);
    let (status, stderr) = session.exit();
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert_restored(&pty);
    assert_eq!(bridge.finish().data, b"hi\r");
}

#[test]
fn escape_b_sends_a_timed_break() {
    let mut bridge = Bridge::echo();
    let pty = pty();
    let mut session = Session::connect(&pty, &bridge);
    // While negotiating, the CLI refuses a BREAK it could not time.
    session.settle();
    session.type_keys(&[ESC, b'b']);
    let on = bridge.wait_for("BREAK on", |s| *s == Seen::Event(ServerEvent::Break(true)));
    let off = bridge.wait_for("BREAK off", |s| {
        *s == Seen::Event(ServerEvent::Break(false))
    });
    // The CLI times the BREAK-OFF at 250 ms; the margin is for the
    // scheduler, which may delay the bridge's view of the BREAK-ON.
    assert!(off - on >= Duration::from_millis(150), "{:?}", off - on);
    session.type_keys(&[ESC, b'q']);
    let (status, stderr) = session.exit();
    assert_eq!(status.code(), Some(0), "{stderr}");
}

#[test]
fn a_break_asked_for_while_negotiating_is_not_cut_short() {
    // The bridge answers after the BREAK-OFF timer has run out: a BREAK
    // held until then would go out with its BREAK-OFF right behind it.
    let bridge = Bridge::start(Options {
        answer_after: Duration::from_millis(600),
        ..Options::default()
    });
    let pty = pty();
    let mut session = Session::connect(&pty, &bridge);
    session.type_keys(&[ESC, b'b']);
    // Once a key has come back, the session has settled.
    session.type_keys(b"a");
    session.expect(b"a");
    session.type_keys(&[ESC, b'q']);
    let (status, stderr) = session.exit();
    assert_eq!(status.code(), Some(0), "{stderr}");
    let seen = bridge.finish().seen;
    let at = |state| {
        seen.iter()
            .find(|(_, s)| *s == Seen::Event(ServerEvent::Break(state)))
            .map(|(at, _)| *at)
    };
    if let (Some(on), Some(off)) = (at(true), at(false)) {
        assert!(off - on >= Duration::from_millis(150), "{:?}", off - on);
    }
}

#[test]
fn a_command_waits_for_the_negotiation_with_the_keys_typed_before_it() {
    // Typed keys are held until the bridge has answered; a DTR change sent
    // meanwhile would reach the device before them.
    let bridge = Bridge::start(Options {
        answer_after: Duration::from_millis(600),
        ..Options::default()
    });
    let pty = pty();
    let mut session = Session::connect(&pty, &bridge);
    session.type_keys(&[b'a', ESC, b'd']);
    session.expect_message("no DTR yet");
    session.expect(b"a");
    session.type_keys(&[ESC, b'q']);
    let (status, stderr) = session.exit();
    assert_eq!(status.code(), Some(0), "{stderr}");
    let report = bridge.finish();
    assert!(
        !report
            .events
            .iter()
            .any(|e| matches!(e, ServerEvent::Dtr(_))),
        "{:?}",
        report.events
    );
    assert_eq!(report.data, b"a");
}

#[test]
fn escape_d_and_r_toggle_dtr_and_rts() {
    let bridge = Bridge::echo();
    let pty = pty();
    let mut session = Session::connect(&pty, &bridge);
    session.settle();
    // Typed in one go, then a byte that comes back after the commands.
    session.type_keys(&[ESC, b'd', ESC, b'r', ESC, b'd', b'.']);
    session.expect(b".");
    session.type_keys(&[ESC, b'q']);
    let (status, stderr) = session.exit();
    assert_eq!(status.code(), Some(0), "{stderr}");
    let lines: Vec<_> = bridge
        .finish()
        .events
        .into_iter()
        .filter(|e| matches!(e, ServerEvent::Dtr(_) | ServerEvent::Rts(_)))
        .collect();
    // From "on", the state bridges open the port with.
    assert_eq!(
        lines,
        [
            ServerEvent::Dtr(false),
            ServerEvent::Rts(false),
            ServerEvent::Dtr(true)
        ]
    );
}

#[test]
fn a_command_acts_after_the_keys_typed_before_it() {
    let mut bridge = Bridge::echo();
    let pty = pty();
    let mut session = Session::connect(&pty, &bridge);
    // Settled: the CLI holds nothing back.
    session.settle();
    // In one write, so that the CLI reads them in one go.
    session.type_keys(&[b'x', ESC, b'd']);
    bridge.wait_for("DTR off", |s| *s == Seen::Event(ServerEvent::Dtr(false)));
    session.type_keys(&[ESC, b'q']);
    let (status, stderr) = session.exit();
    assert_eq!(status.code(), Some(0), "{stderr}");
    let seen = bridge.finish().seen;
    let x = seen
        .iter()
        .position(|(_, s)| matches!(s, Seen::Data(data) if data.contains(&b'x')));
    let dtr = seen
        .iter()
        .position(|(_, s)| *s == Seen::Event(ServerEvent::Dtr(false)));
    assert!(
        x.is_some() && x < dtr,
        "DTR went out before the x: {seen:?}"
    );
}

#[test]
fn the_escape_key_twice_sends_it_once() {
    let bridge = Bridge::echo();
    let pty = pty();
    let mut session = Session::connect(&pty, &bridge);
    session.type_keys(&[b'<', ESC, ESC, b'>']);
    session.expect(&[b'<', ESC, b'>']);
    session.type_keys(&[ESC, b'q']);
    let (status, stderr) = session.exit();
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert_eq!(bridge.finish().data, [b'<', ESC, b'>']);
}

#[test]
fn escape_z_comes_back_to_raw_mode() {
    // In its own session the CLI's process group is orphaned, so the kernel
    // discards the SIGTSTP: the terminal is restored and raw again at once.
    let bridge = Bridge::echo();
    let pty = pty();
    let mut session = Session::connect(&pty, &bridge);
    session.type_keys(&[ESC, b'z']);
    // Raw again only after this: keys typed while it is cooked would
    // raise signals.
    session.expect_message("resumed");
    session.type_keys(b"x\x03y");
    session.expect(b"x\x03y");
    session.type_keys(&[ESC, b'q']);
    let (status, stderr) = session.exit();
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert_restored(&pty);
}

#[test]
fn sigterm_is_exit_143_with_the_terminal_restored() {
    let mut bridge = Bridge::echo();
    let pty = pty();
    let session = Session::connect(&pty, &bridge);
    // Wait until the session runs, past the raw mode.
    bridge.wait_for("the settings", |s| {
        matches!(s, Seen::Event(ServerEvent::ComPortActive))
    });
    let pid = Pid::from_child(&session.child);
    kill_process(pid, Signal::TERM).expect("kill");
    let (status, stderr) = session.exit();
    assert_eq!(status.code(), Some(143), "{status:?} {stderr}");
    assert_restored(&pty);
}

#[test]
fn a_signal_ends_a_break_in_progress() {
    let bridge = Bridge::echo();
    let pty = pty();
    let mut session = Session::connect(&pty, &bridge);
    session.settle();
    session.type_keys(&[ESC, b'b']);
    session.expect_message("BREAK for");
    kill_process(Pid::from_child(&session.child), Signal::TERM).expect("kill");
    let (status, stderr) = session.exit();
    assert_eq!(status.code(), Some(143), "{status:?} {stderr}");
    let events = bridge.finish().events;
    assert!(
        events.ends_with(&[ServerEvent::Break(true), ServerEvent::Break(false)]),
        "{events:?}"
    );
}

#[test]
fn a_stop_from_outside_comes_back_to_raw_mode() {
    // A shell puts its own settings back when a job stops.
    let bridge = Bridge::echo();
    let pty = pty();
    let mut session = Session::connect(&pty, &bridge);
    session.settle();
    let pid = Pid::from_child(&session.child);
    kill_process(pid, Signal::STOP).expect("SIGSTOP");
    rustix::termios::tcsetattr(&pty.slave, OptionalActions::Now, &pty.before)
        .expect("the shell's settings");
    kill_process(pid, Signal::CONT).expect("SIGCONT");
    wait_raw(&pty);
    session.type_keys(b"x\x03y");
    session.expect(b"x\x03y");
    session.type_keys(&[ESC, b'q']);
    let (status, stderr) = session.exit();
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert_restored(&pty);
}

#[test]
fn a_stop_past_the_deadline_is_no_timeout() {
    // The bridge answered while the process was stopped: its answer is
    // waiting in the socket when the deadline is found long past. Whether
    // the owner looks at the deadline before the socket reader has run again
    // is up to the scheduler: without the grace for a late deadline, this
    // fails in some runs only (after `z` it fails every time, but here the
    // process group is orphaned and the kernel discards SIGTSTP).
    let bridge = Bridge::start(Options {
        answer_after: Duration::from_millis(200),
        ..Options::default()
    });
    let mut child = Command::new(BIN)
        .args(["connect", &bridge.target(), "--timeout", "0.5"])
        .args(["--linger", "0.2"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the CLI");
    let mut stderr = Feed::new(child.stderr.take().expect("piped stderr"));
    // The notice comes as the session starts: its deadline runs from then.
    stderr.expect("stderr", b"setting the port to 8N1");
    let pid = Pid::from_child(&child);
    kill_process(pid, Signal::STOP).expect("SIGSTOP");
    thread::sleep(Duration::from_millis(1200));
    kill_process(pid, Signal::CONT).expect("SIGCONT");
    drop(child.stdin.take());
    let status = wait(&mut child);
    assert_eq!(status.code(), Some(0), "{}", stderr.all());
}

#[test]
fn a_signal_ends_a_session_stuck_on_its_stderr() {
    // A session blocked on a full stderr (`2>&1 | less`, paused) cannot take
    // the signal, so the signal thread exits after its grace period. It must
    // not write to that stderr itself, as it would with -v to log the signal.
    let target = scripted(Script::FlowFlood).to_string();
    let mut child = Command::new(BIN)
        .args(["-v", "connect", &target])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the CLI");
    // Never read: the notices fill it.
    let _stderr = child.stderr.take();
    thread::sleep(Duration::from_millis(500));
    kill_process(Pid::from_child(&child), Signal::TERM).expect("kill");
    let status = wait(&mut child);
    assert_eq!(status.code(), Some(143), "{status:?}");
}
