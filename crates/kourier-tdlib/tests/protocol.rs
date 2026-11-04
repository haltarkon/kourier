//! The frame protocol (TG-1 plan §2.3, §5.4 T-a1).

use std::io::Cursor;

use kourier_tdlib::protocol::{
    Bye, FailureCode, FrameError, FromHelper, Hello, LineReader, MAX_CREDENTIAL, MAX_LINE,
    ToHelper, decode_from_helper, decode_to_helper,
};
use kourier_tdlib::scripted::Script;

fn line(frame: &[u8]) -> &[u8] {
    frame
        .strip_suffix(b"\n")
        .expect("a frame ends in a newline")
}

/// Every frame kind encodes and decodes to itself; a missing tag, a missing space, a bad number
/// and a line over the limit are refused, with an error that names no content.
#[test]
fn frames_round_trip_and_bad_lines_are_refused() {
    let to = [
        ToHelper::Create { seq: 7 },
        ToHelper::Gate {
            client: 1,
            port: 40123,
            user: "dXNlcg".to_owned(),
            pass: "cGFzcy0xMjM0NTY3OA".to_owned(),
        },
        ToHelper::Send {
            client: 1,
            json: r#"{"@type":"getAuthorizationState","@extra":3}"#.to_owned(),
        },
        ToHelper::Execute {
            seq: 9,
            json: r#"{"@type":"getOption","name":"version"}"#.to_owned(),
        },
        ToHelper::Quit,
    ];
    for f in &to {
        let mut out = Vec::new();
        f.encode(&mut out).expect("encode");
        assert_eq!(&decode_to_helper(line(&out)).expect("decode"), f);
    }
    let from = [
        FromHelper::Hello(Hello {
            protocol: 1,
            tdlib: Script::default().version,
            commit: Script::default().commit,
            patch_level: "0002".to_owned(),
            landlock: "off".to_owned(),
        }),
        FromHelper::Created { seq: 7, client: 1 },
        FromHelper::Update(r#"{"@type":"updateOption","@client_id":1}"#.to_owned()),
        FromHelper::Executed {
            seq: 9,
            json: r#"{"@type":"ok"}"#.to_owned(),
        },
        FromHelper::Failure {
            code: FailureCode::RefusedLogRequest,
            extra: Some(12),
        },
        FromHelper::Failure {
            code: FailureCode::BadFrame,
            extra: None,
        },
        FromHelper::Heartbeat,
        FromHelper::Bye(Bye {
            closed: vec![1, 2],
            forced: false,
        }),
    ];
    for f in &from {
        let mut out = Vec::new();
        f.encode(&mut out).expect("encode");
        assert_eq!(&decode_from_helper(line(&out)).expect("decode"), f);
    }

    // Refused lines.
    let refused_to: [(&[u8], FrameError); 12] = [
        (b"", FrameError::Empty),
        (b"Z 1", FrameError::UnknownTag),
        (b"S1 {}", FrameError::Shape),
        (b"S", FrameError::Shape),
        (b"S  1 {}", FrameError::BadNumber),
        (b"S x {}", FrameError::BadNumber),
        (b"S -1 {}", FrameError::BadNumber),
        (b"S 1", FrameError::Shape),
        (b"S 1 ", FrameError::Shape),
        (b"C +5", FrameError::BadNumber),
        (b"Q now", FrameError::Shape),
        (b"G 1 2 user", FrameError::Shape),
    ];
    for (l, want) in refused_to {
        assert_eq!(
            decode_to_helper(l),
            Err(want),
            "{:?} was not refused as wanted",
            String::from_utf8_lossy(l)
        );
    }
    assert_eq!(decode_to_helper(b"S 1 \xff"), Err(FrameError::NotUtf8));
    assert_eq!(
        decode_from_helper(b"F no-such-code"),
        Err(FrameError::Shape)
    );
    assert_eq!(decode_from_helper(b"K 1 x"), Err(FrameError::BadNumber));
    assert_eq!(decode_from_helper(b"H {"), Err(FrameError::BadJson));
    assert_eq!(decode_from_helper(b"P 1"), Err(FrameError::Shape));
    // A port outside u16 and a client outside i32 are bad numbers.
    assert_eq!(
        decode_to_helper(b"G 1 70000 u p"),
        Err(FrameError::BadNumber)
    );
    assert_eq!(
        decode_to_helper(b"S 4294967296 {}"),
        Err(FrameError::BadNumber)
    );

    // A payload with a newline is refused at encoding.
    let mut out = Vec::new();
    let bad = ToHelper::Send {
        client: 1,
        json: "{\"a\":\n1}".to_owned(),
    };
    assert_eq!(bad.encode(&mut out), Err(FrameError::Newline));
    assert!(out.is_empty(), "a refused frame wrote bytes");

    // A line over the limit is refused by the decoder and by the reader.
    let mut long = b"U ".to_vec();
    long.resize(MAX_LINE + 1, b'x');
    assert_eq!(decode_from_helper(&long), Err(FrameError::TooLong));
    let mut r = LineReader::with_limit(Cursor::new(b"U 12345\nU 123456789\n".to_vec()), 8);
    assert_eq!(r.next_line().unwrap().as_deref(), Some(&b"U 12345"[..]));
    assert_eq!(r.next_line(), Err(FrameError::TooLong));

    // The errors' texts name no content.
    for e in [FrameError::BadJson, FrameError::Shape, FrameError::TooLong] {
        assert!(!e.to_string().contains('{'), "{e}");
    }
    // Nor does the Debug of a frame with JSON or credentials.
    let secret = ToHelper::Gate {
        client: 1,
        port: 1,
        user: "user-secret".to_owned(),
        pass: "pass-secret".to_owned(),
    };
    let shown = format!("{secret:?} {:?}", to[2]);
    assert!(
        !shown.contains("secret") && !shown.contains("getAuthorizationState"),
        "a frame's Debug shows its content"
    );
}

/// Lines split across reads and a stream that ends inside a line.
#[test]
fn the_line_reader_joins_reads_and_sees_a_truncated_stream() {
    struct Trickle(Vec<u8>, usize);
    impl std::io::Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.1 >= self.0.len() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.0[self.1];
            self.1 += 1;
            Ok(1)
        }
    }
    let mut r = LineReader::new(Trickle(b"C 1\nQ\nU {\"a\":1}".to_vec(), 0));
    assert_eq!(r.next_line().unwrap().as_deref(), Some(&b"C 1"[..]));
    assert_eq!(r.next_line().unwrap().as_deref(), Some(&b"Q"[..]));
    assert_eq!(r.next_line(), Err(FrameError::Truncated));
    let mut r = LineReader::new(Cursor::new(b"Q\n".to_vec()));
    assert_eq!(r.next_line().unwrap().as_deref(), Some(&b"Q"[..]));
    assert_eq!(r.next_line(), Ok(None));
}

#[test]
fn failure_codes_parse_back_and_only_refusals_are_not_fatal() {
    for code in [
        FailureCode::LogHardeningFailed,
        FailureCode::RefusedLogRequest,
        FailureCode::RefusedProxyRequest,
        FailureCode::RefusedRequest,
        FailureCode::RefusedParametersWithoutGate,
        FailureCode::BadFrame,
        FailureCode::TdlibFatal,
        FailureCode::LibOpenFailed,
        FailureCode::LibSymbolMissing,
        FailureCode::GateProxyFailed,
        FailureCode::LockDownFailed,
        FailureCode::RefusedParametersPaths,
        FailureCode::FilesOptionFailed,
    ] {
        assert_eq!(FailureCode::parse(code.as_str()), Some(code));
        // A refusal does not end the helper; nor does a gate proxy failure or a files option
        // failure (that client is closed, the helper goes on, fix F2, K2).
        let goes_on = code.as_str().starts_with("refused-")
            || matches!(
                code,
                FailureCode::GateProxyFailed | FailureCode::FilesOptionFailed
            );
        assert_eq!(code.is_fatal(), !goes_on, "{code}");
    }
}

/// Fix F1: a gate is refused, by the decoder and by the encoder alike, unless the port is 1..=65535
/// and the user name and the password are each 1..=127 bytes (TDLib: `Proxy.cpp:28-30` takes no
/// port 0, `Socks5.cpp:59-63` no login of 128 bytes).
#[test]
fn a_gate_outside_tdlibs_limits_is_refused_both_ways() {
    let gate = |port: u16, user: &str, pass: &str| ToHelper::Gate {
        client: 1,
        port,
        user: user.to_owned(),
        pass: pass.to_owned(),
    };
    let longest = "u".repeat(MAX_CREDENTIAL);
    for ok in [
        gate(1, "u", "p"),
        gate(65535, &longest, &longest),
        gate(40123, "u", &longest),
    ] {
        let mut out = Vec::new();
        ok.encode(&mut out).expect("a gate inside the limits");
        assert_eq!(decode_to_helper(line(&out)).as_ref(), Ok(&ok));
    }
    let too_long = "u".repeat(MAX_CREDENTIAL + 1);
    for (bad, want) in [
        (gate(0, "u", "p"), FrameError::BadNumber),
        (gate(40123, &too_long, "p"), FrameError::Shape),
        (gate(40123, "u", &too_long), FrameError::Shape),
        (gate(40123, "", "p"), FrameError::Shape),
        (gate(40123, "u", ""), FrameError::Shape),
    ] {
        let mut out = Vec::new();
        assert_eq!(bad.encode(&mut out), Err(want), "{bad:?} was written");
        assert!(out.is_empty(), "a refused gate wrote bytes");
    }
    // The same on the wire, where a peer need not have encoded it.
    assert_eq!(
        decode_to_helper(b"G 1 0 u p"),
        Err(FrameError::BadNumber),
        "port 0 was decoded"
    );
    for (user, pass) in [(&too_long[..], "p"), ("u", &too_long[..])] {
        let wire = format!("G 1 40123 {user} {pass}");
        assert_eq!(
            decode_to_helper(wire.as_bytes()),
            Err(FrameError::Shape),
            "a login of {} bytes was decoded",
            MAX_CREDENTIAL + 1
        );
    }
}
