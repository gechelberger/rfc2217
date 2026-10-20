//! `rfc2217 connect` with stdin and stdout piped, on every OS: the bytes
//! pass unchanged both ways, the line settings and control lines reach the
//! bridge, and the readiness policy ends the session with the documented
//! exit codes.

mod common;

use std::time::Duration;

use common::{Bridge, Options, Script, Seen, closed_port, run, run_open, scripted};
use rfc2217::{DataBits, FlowControl, Parity, PortSettings, ServerEvent, StopBits};

/// Long enough for an echo over loopback, short enough for a fast suite.
const LINGER: &str = "0.5";

/// Bytes without a pattern, the same on every run.
fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[3]
        })
        .collect()
}

#[test]
fn every_byte_value_passes_both_ways() {
    let bridge = Bridge::echo();
    // 0xFF twice in a row in the middle: IAC IAC on the wire.
    let input: Vec<u8> = (0..=255).chain((0..=255).rev()).collect();
    let out = run(&["connect", &bridge.target(), "--linger", LINGER], &input);
    assert_eq!(out.code(), Some(0), "{}", out.stderr);
    assert_eq!(out.stdout, input);
    assert_eq!(bridge.finish().data, input);
}

#[test]
fn a_mebibyte_passes_without_deadlock() {
    // The bridge writes all it has before it reads again, through socket
    // buffers a few KiB small: only a CLI that reads while it writes gets
    // through.
    let bridge = Bridge::start(Options {
        small_buffers: true,
        ..Options::default()
    });
    let input = noise(1 << 20);
    let out = run(&["connect", &bridge.target(), "--linger", LINGER], &input);
    assert_eq!(out.code(), Some(0), "{}", out.stderr);
    assert_eq!(out.stdout.len(), input.len());
    assert!(out.stdout == input, "the data came back changed");
    assert!(bridge.finish().data == input, "the bridge got other data");
}

#[test]
fn the_linger_waits_for_late_output() {
    let bridge = Bridge::start(Options {
        delay: Duration::from_millis(400),
        ..Options::default()
    });
    let out = run(&["connect", &bridge.target(), "--linger", "1"], b"AT\r");
    assert_eq!(out.code(), Some(0), "{}", out.stderr);
    assert_eq!(out.stdout, b"AT\r");
    assert!(out.took >= Duration::from_millis(400), "{:?}", out.took);
}

#[test]
fn stdout_carries_only_the_ports_data() {
    let bridge = Bridge::echo();
    let input = b"data\nmore\r\n";
    let out = run(
        &["-vvv", "connect", &bridge.target(), "--linger", LINGER],
        input,
    );
    assert_eq!(out.code(), Some(0), "{}", out.stderr);
    assert_eq!(out.stdout, input);
    // The diagnostics went to stderr.
    assert!(out.stderr.contains("ComPortActive"), "{}", out.stderr);
    assert!(
        out.stderr.contains("the bridge confirmed"),
        "{}",
        out.stderr
    );
}

#[test]
fn by_default_the_baud_rate_is_kept_and_8n1_sent() {
    let bridge = Bridge::start(Options {
        settings: PortSettings {
            baud_rate: 57_600,
            data_bits: DataBits::Seven,
            parity: Parity::Even,
            stop_bits: StopBits::Two,
            flow_control: FlowControl::RtsCts,
        },
        ..Options::default()
    });
    let out = run(&["connect", &bridge.target(), "--linger", "0.1"], b"");
    assert_eq!(out.code(), Some(0), "{}", out.stderr);
    assert!(
        out.stderr
            .contains("setting the port to 8N1 without flow control"),
        "{}",
        out.stderr
    );
    let expected = PortSettings {
        baud_rate: 57_600,
        ..PortSettings::default()
    };
    assert_eq!(bridge.finish().settings, expected);
}

#[test]
fn explicit_settings_reach_the_bridge() {
    let bridge = Bridge::echo();
    let target = bridge.target();
    let flags = "-b 19200 --databits 7 --parity even --stopbits 2 -f xonxoff";
    let args: Vec<_> = ["connect", &target, "--linger", "0.1"]
        .into_iter()
        .chain(flags.split(' '))
        .collect();
    let out = run(&args, b"");
    assert_eq!(out.code(), Some(0), "{}", out.stderr);
    assert!(!out.stderr.contains("8N1"), "{}", out.stderr);
    let expected = PortSettings {
        baud_rate: 19_200,
        data_bits: DataBits::Seven,
        parity: Parity::Even,
        stop_bits: StopBits::Two,
        flow_control: FlowControl::XonXoff,
    };
    assert_eq!(bridge.finish().settings, expected);

    let bridge = Bridge::echo();
    let target = bridge.target();
    let args = [
        "connect", &target, "--linger", "0.1", "-b", "1200", "--frame", "5o1.5",
    ];
    let out = run(&args, b"");
    assert_eq!(out.code(), Some(0), "{}", out.stderr);
    let expected = PortSettings {
        baud_rate: 1200,
        data_bits: DataBits::Five,
        parity: Parity::Odd,
        stop_bits: StopBits::OnePointFive,
        flow_control: FlowControl::None,
    };
    assert_eq!(bridge.finish().settings, expected);
}

#[test]
fn dtr_and_rts_reach_the_bridge() {
    let bridge = Bridge::echo();
    let target = bridge.target();
    let args = [
        "connect", &target, "--linger", "0.1", "--dtr", "off", "--rts", "on",
    ];
    let out = run(&args, b"");
    assert_eq!(out.code(), Some(0), "{}", out.stderr);
    let events = bridge.finish().events;
    let lines: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, ServerEvent::Dtr(_) | ServerEvent::Rts(_)))
        .collect();
    assert_eq!(lines, [&ServerEvent::Dtr(false), &ServerEvent::Rts(true)]);
}

#[test]
fn a_port_nobody_listens_on_is_exit_3() {
    let target = closed_port().to_string();
    let out = run(&["connect", &target], b"");
    assert_eq!(out.code(), Some(3), "{}", out.stderr);
    assert!(
        out.stderr.contains(&format!("cannot connect to {target}")),
        "{}",
        out.stderr
    );
}

#[test]
fn one_v_shows_the_clis_own_debug_logs() {
    // The binary's crate is named `rfc2217`, like the library: its events
    // must name their target, or -v filters them as the library's.
    let target = closed_port().to_string();
    let out = run(&["-v", "connect", &target], b"");
    assert_eq!(out.code(), Some(3), "{}", out.stderr);
    assert!(
        out.stderr
            .contains(&format!("DEBUG rfc2217_cli: {target}: ")),
        "{}",
        out.stderr
    );
}

#[test]
fn a_telnet_server_without_rfc2217_is_exit_4() {
    let target = scripted(Script::Refuse).to_string();
    let out = run(&["connect", &target], b"never sent");
    assert_eq!(out.code(), Some(4), "{}", out.stderr);
    assert!(out.stderr.contains("refused"), "{}", out.stderr);
    // Its output was shown, the input held back.
    assert_eq!(out.stdout, b"login: ");
}

#[test]
fn allow_telnet_carries_on_without_rfc2217() {
    let target = scripted(Script::Refuse).to_string();
    let args = ["connect", &target, "--allow-telnet", "--linger", LINGER];
    let out = run(&args, b"hello");
    assert_eq!(out.code(), Some(0), "{}", out.stderr);
    assert!(out.stderr.contains("warning"), "{}", out.stderr);
    assert_eq!(out.stdout, b"login: hello");
}

#[test]
fn a_port_that_never_answers_is_exit_4_at_the_deadline() {
    let target = scripted(Script::Silent).to_string();
    let out = run(&["connect", &target, "--timeout", "0.3"], b"never sent");
    assert_eq!(out.code(), Some(4), "{}", out.stderr);
    assert!(out.stderr.contains("raw TCP port"), "{}", out.stderr);
    assert!(out.took >= Duration::from_millis(300), "{:?}", out.took);
    assert!(out.took < Duration::from_secs(3), "{:?}", out.took);
}

#[test]
fn without_input_a_port_that_never_answers_is_still_exit_4() {
    // The input ends at once, and the linger is shorter than the deadline:
    // the session must not end as a success before the policy has decided.
    let target = scripted(Script::Silent).to_string();
    let args = ["connect", &target, "--timeout", "0.3", "--linger", "0.05"];
    let out = run(&args, b"");
    assert_eq!(out.code(), Some(4), "{}", out.stderr);
    assert!(out.stderr.contains("raw TCP port"), "{}", out.stderr);
}

#[test]
fn a_raw_port_that_echoes_is_exit_4_at_once() {
    let target = scripted(Script::Echo).to_string();
    let out = run(&["connect", &target, "--timeout", "10"], b"never sent");
    assert_eq!(out.code(), Some(4), "{}", out.stderr);
    assert!(
        out.stderr.contains("echoes the RFC 2217 requests"),
        "{}",
        out.stderr
    );
    assert!(out.took < Duration::from_secs(5), "{:?}", out.took);
}

#[test]
fn other_values_with_strict_are_exit_4() {
    let target = scripted(Script::Mismatch).to_string();
    let out = run(
        &["connect", &target, "--frame", "7E1", "--strict"],
        b"never sent",
    );
    assert_eq!(out.code(), Some(4), "{}", out.stderr);
    assert!(
        out.stderr
            .contains("the bridge reported different values for: data bits, parity"),
        "{}",
        out.stderr
    );
    assert!(out.stdout.is_empty(), "{:?}", out.stdout);
}

#[test]
fn other_values_without_strict_are_a_warning() {
    let target = scripted(Script::Mismatch).to_string();
    let args = ["connect", &target, "--frame", "7E1", "--linger", LINGER];
    let out = run(&args, b"hi");
    assert_eq!(out.code(), Some(0), "{}", out.stderr);
    assert!(
        out.stderr.contains(
            "warning: the bridge reported different values for: data bits, parity \
             (requested kept baud rate, 7E1, no flow control; \
             reported 9600 baud, 8N1, no flow control)"
        ),
        "{}",
        out.stderr
    );
    assert_eq!(out.stdout, b"hi");
}

#[test]
fn a_missing_answer_with_strict_is_exit_4() {
    let target = scripted(Script::NoStopBits).to_string();
    let args = ["connect", &target, "--timeout", "0.3", "--strict"];
    let out = run(&args, b"never sent");
    assert_eq!(out.code(), Some(4), "{}", out.stderr);
    assert!(
        out.stderr
            .contains("no answer from the bridge in 0.3 s for: stop bits"),
        "{}",
        out.stderr
    );
    assert!(out.stdout.is_empty(), "{:?}", out.stdout);
}

#[test]
fn a_missing_answer_without_strict_is_a_warning() {
    let target = scripted(Script::NoStopBits).to_string();
    let args = ["connect", &target, "--timeout", "0.3", "--linger", LINGER];
    let out = run(&args, b"late");
    assert_eq!(out.code(), Some(0), "{}", out.stderr);
    assert!(out.stderr.contains("warning: no answer"), "{}", out.stderr);
    assert_eq!(out.stdout, b"late");
}

#[test]
fn input_waits_while_the_bridge_suspends_the_flow() {
    let bridge = Bridge::start(Options {
        suspend_for: Some(Duration::from_millis(300)),
        ..Options::default()
    });
    let out = run(&["connect", &bridge.target(), "--linger", LINGER], b"held");
    assert_eq!(out.code(), Some(0), "{}", out.stderr);
    assert!(
        out.stderr.contains("suspended the data flow"),
        "{}",
        out.stderr
    );
    assert_eq!(out.stdout, b"held");
    let report = bridge.finish();
    let resumed = report.seen.iter().position(|(_, s)| *s == Seen::Resumed);
    let data = report
        .seen
        .iter()
        .position(|(_, s)| matches!(s, Seen::Data(_)));
    assert!(
        resumed.is_some() && data > resumed,
        "the data came before the resume: {:?}",
        report.seen
    );
}

#[test]
fn the_bridge_closing_the_connection_is_exit_0() {
    let bridge = Bridge::start(Options {
        close_on: Some(b"bye".to_vec()),
        ..Options::default()
    });
    // stdin stays open: only the bridge ends this session.
    let out = run_open(&["connect", &bridge.target()], b"bye");
    assert_eq!(out.code(), Some(0), "{}", out.stderr);
    assert!(
        out.stderr.contains("the bridge closed the connection"),
        "{}",
        out.stderr
    );
    assert_eq!(out.stdout, b"bye");
}
