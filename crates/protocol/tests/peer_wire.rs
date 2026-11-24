use lyteboard_protocol::peer::{
    Context, FrameKind, Invite, Origin, Path, Record, RoutedFrame, WireError, route_proof_message,
};
use zeroize::Zeroizing;

const QR_FIXTURE: &str = concat!(
    "lyteboard-pair:v1:hwF4GHdzczovL3JlbGF5LmV4YW1wbGU6ODQ0M1ARERERERERERERERERERER",
    "WCAiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIlggMzMzMzMzMzMzMzMzMzMzMzMzMzMz",
    "MzMzMzMzMzMzMzNYIEREREREREREREREREREREREREREREREREREREREREREGmVT8iw",
);

fn origin() -> Origin {
    Origin::parse("wss://relay.example:8443").unwrap()
}

fn invitation() -> Invite {
    Invite::new(
        origin(),
        [0x11; 16],
        Zeroizing::new([0x22; 32]),
        [0x33; 32],
        [0x44; 32],
        1_700_000_300,
    )
}

#[test]
fn qr_reference_wire_and_redacted_debug() {
    let invite = invitation();
    let qr = invite.encode_qr();
    assert_eq!(qr.as_str(), QR_FIXTURE);
    let decoded = Invite::decode_qr(&qr).unwrap();
    assert_eq!(decoded.origin, origin());
    assert_eq!(decoded.id, [0x11; 16]);
    assert_eq!(decoded.psk(), &[0x22; 32]);
    assert_eq!(decoded.noise_key, [0x33; 32]);
    assert_eq!(decoded.route, [0x44; 32]);
    assert_eq!(decoded.expires_at, 1_700_000_300);
    assert_eq!(format!("{invite:?}"), "Invite([redacted])");
    assert_eq!(format!("{qr:?}"), "QrCode([redacted])");
}

#[test]
fn rejects_oversized_and_noncanonical_qr() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

    let oversized = "X".repeat(513);
    assert_eq!(
        Invite::decode_qr(&oversized).unwrap_err(),
        WireError::TooLarge
    );
    assert!(Invite::decode_qr(&format!("{QR_FIXTURE}=")).is_err());
    assert!(Invite::decode_qr(&QR_FIXTURE.replace("v1:", "v2:")).is_err());
    assert!(Invite::decode_qr("lyteboard-pair:v1:ü").is_err());
    let mut version = URL_SAFE_NO_PAD
        .decode(QR_FIXTURE.strip_prefix("lyteboard-pair:v1:").unwrap())
        .unwrap();
    version[1] = 2;
    assert_eq!(
        Invite::decode_qr(&format!(
            "lyteboard-pair:v1:{}",
            URL_SAFE_NO_PAD.encode(&version)
        ))
        .unwrap_err(),
        WireError::UnsupportedVersion,
    );
    version[1] = 1;
    version.insert(1, 0x18);
    assert!(
        Invite::decode_qr(&format!(
            "lyteboard-pair:v1:{}",
            URL_SAFE_NO_PAD.encode(&version)
        ))
        .is_err()
    );
    version.remove(1);
    version.push(0xf6);
    assert!(
        Invite::decode_qr(&format!(
            "lyteboard-pair:v1:{}",
            URL_SAFE_NO_PAD.encode(&version)
        ))
        .is_err()
    );
}

#[test]
fn accepts_only_canonical_relay_origins() {
    for valid in [
        "wss://relay.example",
        "wss://relay.example:8443",
        "wss://localhost",
        "wss://xn--bcher-kva.example",
    ] {
        assert_eq!(Origin::parse(valid).unwrap().as_str(), valid);
    }
    for invalid in [
        "ws://relay.example",
        "wss://RELAY.example",
        "wss://relay.example/",
        "wss://relay.example?x=1",
        "wss://relay.example#fragment",
        "wss://user@relay.example",
        "wss://relay..example",
        "wss://-relay.example",
        "wss://relay.example.",
        "wss://relay.example:08443",
        "wss://relay.example:443",
        "wss://relay.example:0",
        "wss://relay.example:65536",
        "wss://127.0.0.1",
        "wss://[::1]",
    ] {
        assert!(Origin::parse(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn peer_record_reference_wire_and_rejects_malformed_cbor() {
    assert_eq!(Record::Ready.encode().unwrap(), [0x82, 1, 6]);
    assert_eq!(Record::decode(&[0x82, 1, 6]).unwrap(), Record::Ready);
    assert_eq!(
        Record::decode(&[0x82, 2, 6]).unwrap_err(),
        WireError::UnsupportedVersion
    );
    for invalid in [
        &[0x82, 1, 6, 0xf6][..],
        &[0x83, 1, 6, 0xf6],
        &[0x82, 1],
        &[0x98, 2, 1, 6],
        &[0x82, 0x18, 1, 6],
        &[0x82, 1, 0x18, 6],
        &[0x9f, 1, 6, 0xff],
        &[0x82, 1, 0],
        &[0x82, 1, 8],
        &[0xa1, 1, 6],
        &[0xc0, 0x82, 1, 6],
    ] {
        assert!(Record::decode(invalid).is_err(), "{invalid:x?}");
    }
    let record = Record::Text {
        id: [0x11; 16],
        text: "hi".into(),
    };
    let mut fixture = vec![0x84, 1, 7, 0x50];
    fixture.extend([0x11; 16]);
    fixture.extend([0x62, b'h', b'i']);
    assert_eq!(record.encode().unwrap(), fixture);
    assert_eq!(Record::decode(&fixture).unwrap(), record);
    fixture.splice(20..21, [0x78, 2]);
    assert!(Record::decode(&fixture).is_err());
    assert!(!format!("{record:?}").contains("hi"));
}

#[test]
fn transcript_context_and_route_proof_reference_wire() {
    let ctx = Context {
        origin: Origin::parse("wss://r.example").unwrap(),
        path: Path::Relay,
        channel: [0x11; 16],
        initiator: [0x22; 32],
        responder: [0x33; 32],
        invite: Some([0x44; 16]),
    };
    let mut fixture = b"\x89\x6elyteboard.peer\x01\x00\x00\x6fwss://r.example\x50".to_vec();
    fixture.extend([0x11; 16]);
    fixture.extend([0x58, 0x20]);
    fixture.extend([0x22; 32]);
    fixture.extend([0x58, 0x20]);
    fixture.extend([0x33; 32]);
    fixture.push(0x50);
    fixture.extend([0x44; 16]);
    assert_eq!(ctx.encode().unwrap(), fixture);
    assert!(
        Context {
            path: Path::Lan,
            ..ctx
        }
        .encode()
        .is_err()
    );

    let mut proof = b"\x84\x74lyteboard.pair-route\x01\x58\x20".to_vec();
    proof.extend([0x22; 32]);
    proof.extend([0x58, 0x20]);
    proof.extend([0x33; 32]);
    assert_eq!(route_proof_message(&[0x22; 32], &[0x33; 32]), proof);
}

#[test]
fn routing_envelope_reference_wire_and_bounds() {
    let mut frame = RoutedFrame {
        to: [0x22; 32],
        channel: [0x33; 16],
        kind: FrameKind::PairingOpen,
        selector: Some([0x44; 16]),
        body: vec![0xaa],
    };
    let mut fixture = vec![0x87, 1, 3, 0x58, 0x20];
    fixture.extend([0x22; 32]);
    fixture.push(0x50);
    fixture.extend([0x33; 16]);
    fixture.extend([0x00, 0x50]);
    fixture.extend([0x44; 16]);
    fixture.extend([0x41, 0xaa]);
    assert_eq!(frame.encode_outgoing().unwrap(), fixture);
    assert_eq!(RoutedFrame::decode_outgoing(&fixture).unwrap(), frame);
    let mut forward = vec![0x88, 1, 4, 0x58, 0x20];
    forward.extend([0x11; 32]);
    forward.extend_from_slice(&fixture[3..]);
    assert_eq!(frame.encode_forwarded(&[0x11; 32]).unwrap(), forward);
    assert_eq!(
        RoutedFrame::decode_forwarded(&forward).unwrap(),
        ([0x11; 32], frame.clone())
    );
    assert!(RoutedFrame::decode_outgoing(&forward).is_err());
    assert!(RoutedFrame::decode_forwarded(&fixture).is_err());
    let mut noncanonical = forward.clone();
    noncanonical.splice(3..5, [0x59, 0, 0x20]);
    assert!(RoutedFrame::decode_forwarded(&noncanonical).is_err());
    forward.push(0xf6);
    assert!(RoutedFrame::decode_forwarded(&forward).is_err());
    frame.body = vec![0xaa; 1025];
    assert_eq!(frame.encode_outgoing().unwrap_err(), WireError::TooLarge);
    frame.body.clear();
    assert!(frame.encode_outgoing().is_err());
    frame.body.resize(34 * 1024, 0xaa);
    frame.kind = FrameKind::Transport;
    assert!(frame.encode_outgoing().is_err());
    frame.selector = None;
    assert_eq!(
        RoutedFrame::decode_outgoing(&frame.encode_outgoing().unwrap()).unwrap(),
        frame
    );
    frame.body.push(0xaa);
    assert_eq!(frame.encode_outgoing().unwrap_err(), WireError::TooLarge);
}

#[test]
fn record_size_bounds_and_utf8_validation() {
    for text in [String::new(), "nul\0".into(), "x".repeat(16 * 1024 + 1)] {
        assert!(Record::Text { id: [0; 16], text }.encode().is_err());
    }
    let largest = Record::Text {
        id: [0; 16],
        text: "é".repeat(8 * 1024),
    };
    assert_eq!(Record::decode(&largest.encode().unwrap()).unwrap(), largest);
    assert!(
        Record::Hello {
            label: "x".repeat(65),
            proof: [0; 64]
        }
        .encode()
        .is_err()
    );
    let mut invalid = vec![0x84, 1, 7, 0x50];
    invalid.extend([0; 16]);
    invalid.extend([0x61, 0xff]);
    assert!(Record::decode(&invalid).is_err());
}
