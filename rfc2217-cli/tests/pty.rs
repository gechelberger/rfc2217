//! `rfc2217 pty` (Linux and macOS): the binary bridges a pty to the test
//! bridge, and the test plays the local program, opening the device by the
//! path the CLI prints and changing its settings through termios.
//!
//! The termios calls go through rustix, which makes the ioctls itself: with
//! Debian's and Ubuntu's glibc, `tcsetattr` on a pty fails with EINVAL for
//! some requests, and the tests must not depend on the C library.

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use common::{BIN, Bridge, DEADLINE, Options, Script, Seen, closed_port, read_all, scripted, wait};
use rfc2217::{DataBits, FlowControl, Parity, PortSettings, ServerEvent, StopBits};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::{Mode, OFlags};
use rustix::process::{Pid, Signal, kill_process};
use rustix::termios::{ControlModes, OptionalActions, Termios, tcgetattr, tcsetattr};

/// A running `rfc2217 pty`, and the device path it printed.
#[derive(Debug)]
struct Run {
    child: Child,
    path: PathBuf,
    /// What stdout carried after the path.
    stdout: JoinHandle<Vec<u8>>,
    stderr: JoinHandle<Vec<u8>>,
}

impl Run {
    /// Starts `rfc2217 pty args` and waits for the path on stdout.
    fn start(args: &[&str]) -> Self {
        Self::wait_for_path(spawn(args))
    }

    /// Waits for the path on stdout of `child`, a `rfc2217 pty`.
    fn wait_for_path(mut child: Child) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let stdout = thread::spawn(move || {
            let mut line = String::new();
            let _ = stdout.read_line(&mut line);
            let _ = tx.send(line);
            let mut rest = Vec::new();
            let _ = stdout.read_to_end(&mut rest);
            rest
        });
        let stderr = read_all(child.stderr.take().expect("piped stderr"));
        let line = match rx.recv_timeout(DEADLINE) {
            Ok(line) if !line.is_empty() => line,
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let stderr = stderr.join().unwrap_or_default();
                let stderr = String::from_utf8_lossy(&stderr);
                panic!("no path on stdout; stderr:\n{stderr}");
            }
        };
        Self {
            child,
            path: PathBuf::from(line.trim_end_matches('\n')),
            stdout,
            stderr,
        }
    }

    /// Sends `signal`, and returns how the CLI exited and its stderr.
    fn stop(self, signal: Signal) -> (ExitStatus, String) {
        kill_process(Pid::from_child(&self.child), signal).expect("kill");
        self.exit()
    }

    /// Waits for the CLI to exit on its own.
    fn exit(mut self) -> (ExitStatus, String) {
        let status = wait(&mut self.child);
        let rest = self.stdout.join().expect("stdout reader");
        assert_eq!(rest, b"", "stdout carries nothing but the path");
        let stderr = self.stderr.join().expect("stderr reader");
        (status, String::from_utf8_lossy(&stderr).into_owned())
    }
}

fn spawn(args: &[&str]) -> Child {
    Command::new(BIN)
        .arg("pty")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the CLI")
}

/// A local program with the device open (non-blocking, as most serial
/// libraries open it).
#[derive(Debug)]
struct Program(File);

impl Program {
    fn open(path: &Path) -> Self {
        let fd = rustix::fs::open(
            path,
            OFlags::RDWR | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap_or_else(|e| panic!("opening {}: {e}", path.display()));
        Self(File::from(fd))
    }

    fn termios(&self) -> Termios {
        tcgetattr(&self.0).expect("tcgetattr")
    }

    fn configure(&self, change: impl FnOnce(&mut Termios)) {
        let mut termios = self.termios();
        change(&mut termios);
        tcsetattr(&self.0, OptionalActions::Now, &termios).expect("tcsetattr");
    }

    fn write(&self, mut data: &[u8]) {
        let end = Instant::now() + DEADLINE;
        while !data.is_empty() {
            match (&self.0).write(data) {
                Ok(n) => data = &data[n..],
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < end, "the pty took no more data");
                    thread::sleep(Duration::from_millis(1));
                }
                Err(e) => panic!("writing to the pty: {e}"),
            }
        }
    }

    /// Reads until `len` bytes came, for at most [`DEADLINE`].
    fn read_exactly(&self, len: usize) -> Vec<u8> {
        let got = self.read_until(Instant::now() + DEADLINE, len);
        assert_eq!(got.len(), len, "only {got:?} arrived");
        got
    }

    /// Everything that arrives within `time`.
    fn read_for(&self, time: Duration) -> Vec<u8> {
        self.read_until(Instant::now() + time, usize::MAX)
    }

    fn read_until(&self, end: Instant, len: usize) -> Vec<u8> {
        let mut got = Vec::new();
        let mut buf = [0; 4096];
        while got.len() < len {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let timeout = Timespec::try_from(left).expect("a timespec");
            let mut fds = [PollFd::new(&self.0, PollFlags::IN)];
            if rustix::event::poll(&mut fds, Some(&timeout)).expect("poll") == 0 {
                break;
            }
            match (&self.0).read(&mut buf) {
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("reading the pty: {e}"),
            }
        }
        got
    }
}

/// A directory of its own for one test's link, under the target directory.
fn scratch(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("pty-{test}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

#[test]
fn the_path_comes_on_stdout_and_bytes_pass_both_ways() {
    let mut bridge = Bridge::echo();
    let run = Run::start(&[&bridge.target()]);
    let program = Program::open(&run.path);
    let data: Vec<u8> = (0..=255).collect();
    program.write(&data);
    // The echo comes back unchanged, and only once: the pty is raw both
    // ways.
    assert_eq!(program.read_exactly(data.len()), data);
    assert_eq!(program.read_for(Duration::from_millis(100)), b"");
    bridge.wait_for_data(&data);
    let (status, stderr) = run.stop(Signal::TERM);
    assert_eq!(status.code(), Some(143), "{stderr}");
    assert_eq!(bridge.finish().data, data);
}

#[test]
fn keep_shows_the_bridges_rate_in_the_pty() {
    let bridge = Bridge::start(Options {
        settings: PortSettings {
            baud_rate: 57_600,
            ..PortSettings::default()
        },
        ..Options::default()
    });
    let run = Run::start(&[&bridge.target()]);
    let termios = Program::open(&run.path).termios();
    assert_eq!(termios.output_speed(), 57_600);
    let (status, stderr) = run.stop(Signal::TERM);
    assert_eq!(status.code(), Some(143), "{stderr}");
}

#[test]
fn the_programs_settings_reach_the_bridge() {
    let mut bridge = Bridge::echo();
    // Data bits and parity come from the command line only.
    let run = Run::start(&[&bridge.target(), "-b", "9600", "--frame", "7E1"]);
    let program = Program::open(&run.path);
    assert_eq!(program.termios().output_speed(), 9600);
    program.configure(|t| {
        t.set_speed(19_200).expect("19200 baud");
        t.control_modes |= ControlModes::CSTOPB | ControlModes::CRTSCTS;
    });
    let want = PortSettings {
        baud_rate: 19_200,
        data_bits: DataBits::Seven,
        parity: Parity::Even,
        stop_bits: StopBits::Two,
        flow_control: FlowControl::RtsCts,
    };
    bridge.wait_for("the program's settings", |seen| {
        *seen == Seen::Event(ServerEvent::SettingsChanged(want))
    });
    // An arbitrary rate, as pyserial sets it.
    program.configure(|t| t.set_speed(250_000).expect("250000 baud"));
    bridge.wait_for("250000 baud", |seen| {
        matches!(seen, Seen::Event(ServerEvent::SettingsChanged(s)) if s.baud_rate == 250_000)
    });
    let (status, stderr) = run.stop(Signal::TERM);
    assert_eq!(status.code(), Some(143), "{stderr}");
    assert!(
        stderr.contains("the bridge confirmed 19200 baud"),
        "{stderr}"
    );
    assert_eq!(
        bridge.finish().settings,
        PortSettings {
            baud_rate: 250_000,
            ..want
        }
    );
}

#[test]
fn the_link_points_at_the_pty_and_goes_on_a_signal() {
    let bridge = Bridge::echo();
    let link = scratch("signal").join("ttyRFC0");
    let run = Run::start(&[&bridge.target(), "--link", link.to_str().unwrap()]);
    assert_eq!(fs::read_link(&link).expect("the link"), run.path);
    // Through the link, as a program given its path.
    let program = Program::open(&link);
    program.write(b"ping");
    assert_eq!(program.read_exactly(4), b"ping");
    let (status, stderr) = run.stop(Signal::INT);
    assert_eq!(status.code(), Some(130), "{stderr}");
    assert!(!exists(&link), "the link is left behind");
}

#[test]
fn the_link_appears_once_the_bridge_has_answered() {
    // A script waiting for the link finds the pty with the bridge's rate.
    let bridge = Bridge::start(Options {
        answer_after: Duration::from_millis(600),
        ..Options::default()
    });
    let link = scratch("late").join("ttyRFC0");
    let child = spawn(&[&bridge.target(), "--link", link.to_str().unwrap()]);
    thread::sleep(Duration::from_millis(300));
    assert!(
        !exists(&link),
        "the link is there before the bridge answered"
    );
    let run = Run::wait_for_path(child);
    assert_eq!(fs::read_link(&link).expect("the link"), run.path);
    assert_eq!(Program::open(&link).termios().output_speed(), 115_200);
    let (status, stderr) = run.stop(Signal::TERM);
    assert_eq!(status.code(), Some(143), "{stderr}");
    assert!(!exists(&link), "the link is left behind");
}

#[test]
fn an_ignored_hangup_stays_ignored() {
    // As `nohup` leaves it: the bridge must outlive the terminal.
    let bridge = Bridge::echo();
    let child = Command::new("sh")
        .args([
            "-c",
            "trap '' HUP; exec \"$0\" pty \"$1\"",
            BIN,
            &bridge.target(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the CLI through sh");
    let mut run = Run::wait_for_path(child);
    kill_process(Pid::from_child(&run.child), Signal::HUP).expect("SIGHUP");
    thread::sleep(Duration::from_millis(300));
    assert!(
        run.child.try_wait().expect("try_wait").is_none(),
        "SIGHUP ended it"
    );
    let program = Program::open(&run.path);
    program.write(b"still here");
    assert_eq!(program.read_exactly(10), b"still here");
    let (status, stderr) = run.stop(Signal::TERM);
    assert_eq!(status.code(), Some(143), "{stderr}");
}

#[test]
fn the_bridge_closing_the_connection_is_exit_1() {
    let bridge = Bridge::start(Options {
        close_on: Some(b"bye".to_vec()),
        ..Options::default()
    });
    let link = scratch("remote-close").join("ttyRFC0");
    let run = Run::start(&[&bridge.target(), "--link", link.to_str().unwrap()]);
    Program::open(&run.path).write(b"bye");
    let (status, stderr) = run.exit();
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("the bridge closed the connection"),
        "{stderr}"
    );
    assert!(!exists(&link), "the link is left behind");
}

#[test]
fn a_failed_negotiation_is_exit_4_without_a_path() {
    let peer = scripted(Script::Silent).to_string();
    let link = scratch("negotiation").join("ttyRFC0");
    let mut child = spawn(&[&peer, "--timeout", "0.3", "--link", link.to_str().unwrap()]);
    let stdout = read_all(child.stdout.take().expect("piped stdout"));
    let stderr = read_all(child.stderr.take().expect("piped stderr"));
    let status = wait(&mut child);
    let stderr = String::from_utf8_lossy(&stderr.join().unwrap()).into_owned();
    assert_eq!(status.code(), Some(4), "{stderr}");
    // A script waiting for the path gets none.
    assert_eq!(stdout.join().unwrap(), b"");
    assert!(!exists(&link), "the link is left behind");
}

#[test]
fn a_file_in_the_links_way_is_left_alone() {
    let path = scratch("in-the-way").join("ttyRFC0");
    fs::write(&path, "mine").unwrap();
    // The link comes before the connection: nobody listens there, and the
    // exit code is still the link's.
    let mut child = spawn(&[&closed_port().to_string(), "--link", path.to_str().unwrap()]);
    let stderr = read_all(child.stderr.take().expect("piped stderr"));
    let status = wait(&mut child);
    let stderr = String::from_utf8_lossy(&stderr.join().unwrap()).into_owned();
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("already exists"), "{stderr}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "mine");
}

#[test]
fn a_port_nobody_listens_on_is_exit_3() {
    let mut child = spawn(&[&closed_port().to_string()]);
    let stderr = read_all(child.stderr.take().expect("piped stderr"));
    let status = wait(&mut child);
    let stderr = String::from_utf8_lossy(&stderr.join().unwrap()).into_owned();
    assert_eq!(status.code(), Some(3), "{stderr}");
    assert!(status.signal().is_none());
}

/// Linux only: macOS cannot tell when a program opens the device, so there
/// the data is kept for the next program.
#[cfg(target_os = "linux")]
#[test]
fn data_for_nobody_is_dropped_before_the_next_program() {
    let delay = Duration::from_millis(300);
    let mut bridge = Bridge::start(Options {
        delay,
        ..Options::default()
    });
    let run = Run::start(&[&bridge.target()]);
    // The echo comes back after this program has gone.
    Program::open(&run.path).write(b"stale");
    bridge.wait_for_data(b"stale");
    thread::sleep(delay + Duration::from_millis(700));
    let program = Program::open(&run.path);
    program.write(b"fresh");
    // Only what arrived while it had the device open.
    assert_eq!(program.read_exactly(5), b"fresh");
    assert_eq!(program.read_for(Duration::from_millis(100)), b"");
    let (status, stderr) = run.stop(Signal::TERM);
    assert_eq!(status.code(), Some(143), "{stderr}");
}
