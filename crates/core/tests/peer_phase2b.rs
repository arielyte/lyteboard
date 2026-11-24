use std::{
    sync::{Arc, Barrier},
    time::Duration,
};

use lyteboard_core::peer::{
    DeviceIdentity, Endpoint, Error, Pairing, PairingStatus, Session, SessionEvent, Time,
};
use lyteboard_protocol::peer::{Invite, Origin, Path, RoutedFrame};

const NOW: Time = Time {
    unix_seconds: 1_800_000_000,
    monotonic: Duration::from_secs(600),
};

fn after(seconds: u64) -> Time {
    Time {
        unix_seconds: NOW.unix_seconds + seconds,
        monotonic: NOW.monotonic + Duration::from_secs(seconds),
    }
}

fn endpoint() -> Endpoint {
    Endpoint::new(
        DeviceIdentity::generate().unwrap(),
        Origin::parse("wss://relay.example").unwrap(),
    )
}

// Round-trip the actual relay-facing envelope over the in-memory link.
fn forward(source: &Endpoint, frame: RoutedFrame) -> RoutedFrame {
    let transmitted = frame.encode_outgoing().unwrap();
    let frame = RoutedFrame::decode_outgoing(&transmitted).unwrap();
    let forwarded = frame.encode_forwarded(&source.route_id()).unwrap();
    let (origin, frame) = RoutedFrame::decode_forwarded(&forwarded).unwrap();
    assert_eq!(origin, source.route_id());
    frame
}

fn open_pair<'a>(a: &'a Endpoint, b: &'a Endpoint) -> (Pairing<'a>, Pairing<'a>) {
    let qr = a.create_invite(NOW).unwrap();
    let (mut initiator, first) = b.begin_pairing(qr.as_str(), "Scanner", NOW).unwrap();
    let (mut responder, second) = a
        .accept_pairing(b.route_id(), forward(b, first), "Inviter", NOW)
        .unwrap();
    let flights = initiator
        .receive(a.route_id(), forward(a, second), NOW)
        .unwrap();
    let mut to_b = Vec::new();
    for flight in flights {
        to_b.extend(
            responder
                .receive(b.route_id(), forward(b, flight), NOW)
                .unwrap(),
        );
    }
    for flight in to_b {
        assert!(
            initiator
                .receive(a.route_id(), forward(a, flight), NOW)
                .unwrap()
                .is_empty()
        );
    }
    (responder, initiator)
}

fn approve(b: &Endpoint, responder: &mut Pairing<'_>, initiator: &mut Pairing<'_>) -> RoutedFrame {
    let code = responder.verification_code().unwrap().as_str().to_owned();
    assert_eq!(code, initiator.verification_code().unwrap().as_str());
    assert_eq!(code.len(), 19);
    assert!(responder.approve(&code, NOW).unwrap().is_empty());
    let approve = initiator.approve(&code, NOW).unwrap().pop().unwrap();
    responder
        .receive(b.route_id(), forward(b, approve), NOW)
        .unwrap()
        .pop()
        .unwrap()
}

fn pair(a: &Endpoint, b: &Endpoint) {
    let (mut responder, mut initiator) = open_pair(a, b);
    let commit = approve(b, &mut responder, &mut initiator);
    let stored = initiator
        .receive(a.route_id(), forward(a, commit), NOW)
        .unwrap()
        .pop()
        .unwrap();
    assert!(!a.is_trusted(b.route_id()).unwrap());
    assert!(!b.is_trusted(a.route_id()).unwrap());
    let done = responder
        .receive(b.route_id(), forward(b, stored), NOW)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(responder.status(), PairingStatus::LocalTrustSaved);
    assert!(!b.is_trusted(a.route_id()).unwrap());
    assert!(
        initiator
            .receive(a.route_id(), forward(a, done), NOW)
            .unwrap()
            .is_empty()
    );
    assert_eq!(initiator.status(), PairingStatus::Complete);
    assert!(a.is_trusted(b.route_id()).unwrap());
    assert!(b.is_trusted(a.route_id()).unwrap());
}

fn ready<'a>(a: &'a Endpoint, b: &'a Endpoint, path: Path) -> (Session<'a>, Session<'a>) {
    let (mut left, one) = a.begin_session(b.route_id(), path, NOW).unwrap();
    assert!(left.send_text([1; 16], "too early", NOW).is_err());
    let (mut right, two) = b
        .accept_session(a.route_id(), path, forward(a, one), NOW)
        .unwrap();
    assert!(right.send_text([1; 16], "too early", NOW).is_err());
    let SessionEvent::Send(init_ready) = left.receive(b.route_id(), forward(b, two), NOW).unwrap()
    else {
        panic!("expected initiator Ready")
    };
    let SessionEvent::Send(resp_ready) = right
        .receive(a.route_id(), forward(a, init_ready), NOW)
        .unwrap()
    else {
        panic!("expected responder Ready")
    };
    assert!(left.send_text([1; 16], "still too early", NOW).is_err());
    assert!(matches!(
        left.receive(b.route_id(), forward(b, resp_ready), NOW)
            .unwrap(),
        SessionEvent::Ready
    ));
    (left, right)
}

#[test]
fn invitation_expiry_and_wrong_pin_or_psk_leave_valid_invite_unused() {
    let a = endpoint();
    let b = endpoint();
    let qr = a.create_invite(NOW).unwrap();
    let payload = Invite::decode_qr(qr.as_str()).unwrap();
    assert_eq!(payload.expires_at, NOW.unix_seconds + 300);
    for wrong_psk in [true, false] {
        let mut modified = Invite::decode_qr(qr.as_str()).unwrap();
        if wrong_psk {
            modified = Invite::new(
                modified.origin,
                modified.id,
                zeroize::Zeroizing::new([42; 32]),
                modified.noise_key,
                modified.route,
                modified.expires_at,
            );
        } else {
            modified.noise_key = DeviceIdentity::generate().unwrap().noise_key();
        }
        let (_, frame) = b
            .begin_pairing(modified.encode_qr().as_str(), "B", NOW)
            .unwrap();
        assert!(a.accept_pairing(b.route_id(), frame, "A", NOW).is_err());
    }
    let (_, frame) = b.begin_pairing(qr.as_str(), "B", NOW).unwrap();
    assert!(a.accept_pairing(b.route_id(), frame, "A", NOW).is_ok());
    let expired = a.create_invite(after(301)).unwrap();
    assert!(b.begin_pairing(expired.as_str(), "B", after(662)).is_err());
}

#[test]
fn invitation_reservation_is_atomic_and_never_reopens() {
    let a = Arc::new(endpoint());
    let b = endpoint();
    let qr = a.create_invite(NOW).unwrap();
    let (_attempt, frame) = b.begin_pairing(qr.as_str(), "B", NOW).unwrap();
    let from = b.route_id();
    let barrier = Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let left = scope.spawn(|| {
            barrier.wait();
            a.accept_pairing(from, frame.clone(), "A", NOW).is_ok()
        });
        let right = scope.spawn(|| {
            barrier.wait();
            a.accept_pairing(from, frame.clone(), "A", NOW).is_ok()
        });
        [left.join().unwrap(), right.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|&&success| success).count(), 1);
    assert!(a.accept_pairing(from, frame, "A", NOW).is_err());
}

#[test]
fn mutual_pairing_and_confirmed_kk_protect_text_in_both_directions() {
    let a = endpoint();
    let b = endpoint();
    pair(&a, &b);
    let (mut left, mut right) = ready(&a, &b, Path::Relay);
    let msg = left.send_text([9; 16], "synthetic payload", NOW).unwrap();
    assert!(!msg.body.windows(17).any(|w| w == b"synthetic payload"));
    assert!(
        matches!(right.receive(a.route_id(), forward(&a, msg), NOW).unwrap(), SessionEvent::Text { id, text } if id == [9;16] && text == "synthetic payload")
    );
    let response = right.send_text([8; 16], "reverse", NOW).unwrap();
    assert!(
        matches!(left.receive(b.route_id(), response, NOW).unwrap(), SessionEvent::Text { text, .. } if text == "reverse")
    );
}

#[test]
fn interrupted_final_pairing_stays_one_sided_and_requires_explicit_recovery() {
    let a = endpoint();
    let b = endpoint();
    let (mut responder, mut initiator) = open_pair(&a, &b);
    let commit = approve(&b, &mut responder, &mut initiator);
    let stored = initiator
        .receive(a.route_id(), commit, NOW)
        .unwrap()
        .pop()
        .unwrap();
    let _undelivered = responder.receive(b.route_id(), stored, NOW).unwrap();
    assert_eq!(responder.status(), PairingStatus::LocalTrustSaved);
    drop(initiator);
    drop(responder);
    let (_, frame) = a.begin_session(b.route_id(), Path::Relay, NOW).unwrap();
    assert!(matches!(
        b.accept_session(a.route_id(), Path::Relay, frame, NOW),
        Err(Error::Untrusted)
    ));
    a.forget(b.route_id()).unwrap();
    pair(&a, &b);
}

#[test]
fn missing_rejected_and_tampered_confirmations_never_activate_pending_trust() {
    for scenario in 0..3 {
        let a = endpoint();
        let b = endpoint();
        let (mut responder, mut initiator) = open_pair(&a, &b);
        let code = initiator.verification_code().unwrap().as_str().to_owned();
        match scenario {
            0 => {
                let approve = initiator.approve(&code, NOW).unwrap().pop().unwrap();
                assert!(
                    responder
                        .receive(b.route_id(), approve, NOW)
                        .unwrap()
                        .is_empty()
                );
                assert!(responder.approve(&code, after(300)).is_err());
            }
            1 => {
                let reject = initiator.reject(NOW).unwrap().unwrap();
                assert!(responder.receive(b.route_id(), reject, NOW).is_err());
            }
            _ => {
                let mut commit = approve(&b, &mut responder, &mut initiator);
                commit.body[0] ^= 1;
                assert!(initiator.receive(a.route_id(), commit, NOW).is_err());
            }
        }
        drop(responder);
        drop(initiator);
        assert!(!a.is_trusted(b.route_id()).unwrap());
        assert!(!b.is_trusted(a.route_id()).unwrap());
        pair(&a, &b);
    }
}

#[test]
fn three_devices_keep_pins_separate_and_revocation_invalidates_live_sessions() {
    let a = endpoint();
    let b = endpoint();
    let c = endpoint();
    assert_ne!(a.route_id(), b.route_id());
    assert_ne!(a.route_id(), c.route_id());
    assert_ne!(b.route_id(), c.route_id());
    pair(&a, &b);
    pair(&a, &c);
    assert!(matches!(
        b.begin_session(c.route_id(), Path::Relay, NOW),
        Err(Error::Untrusted)
    ));
    let (mut ab, mut ba) = ready(&a, &b, Path::Lan);
    let (mut ac, mut ca) = ready(&a, &c, Path::Relay);
    a.revoke(b.route_id()).unwrap();
    assert!(a.begin_session(b.route_id(), Path::Relay, NOW).is_err());
    let (_, incoming) = b.begin_session(a.route_id(), Path::Relay, NOW).unwrap();
    assert!(matches!(
        a.accept_session(b.route_id(), Path::Relay, incoming, NOW),
        Err(Error::Untrusted)
    ));
    assert!(ab.send_text([1; 16], "blocked", NOW).is_err());
    assert!(
        ab.receive(
            b.route_id(),
            ba.send_text([2; 16], "also blocked", NOW).unwrap(),
            NOW
        )
        .is_err()
    );
    assert!(matches!(
        ca.receive(
            a.route_id(),
            ac.send_text([3; 16], "C still trusted", NOW).unwrap(),
            NOW
        )
        .unwrap(),
        SessionEvent::Text { .. }
    ));
}

#[test]
fn replay_tamper_and_reordering_close_the_channel() {
    let a = endpoint();
    let b = endpoint();
    pair(&a, &b);
    for scenario in 0..3 {
        let (mut tx, mut rx) = ready(&a, &b, Path::Relay);
        let first = tx.send_text([0; 16], "first", NOW).unwrap();
        let second = tx.send_text([1; 16], "second", NOW).unwrap();
        let invalid = match scenario {
            0 => {
                rx.receive(a.route_id(), first.clone(), NOW).unwrap();
                first
            }
            1 => {
                let mut frame = first;
                frame.body[0] ^= 1;
                frame
            }
            _ => second,
        };
        assert!(rx.receive(a.route_id(), invalid, NOW).is_err());
        assert!(rx.send_text([9; 16], "closed", NOW).is_err());
    }
}

#[test]
fn reconnect_uses_fresh_ciphertext_and_retires_the_replaced_channel() {
    let a = endpoint();
    let b = endpoint();
    pair(&a, &b);
    let (mut old, _old_rx) = ready(&a, &b, Path::Relay);
    let old_ciphertext = old.send_text([7; 16], "same data", NOW).unwrap();
    let (mut new, mut new_rx) = ready(&a, &b, Path::Relay);
    let new_ciphertext = new.send_text([7; 16], "same data", NOW).unwrap();
    assert_ne!(old_ciphertext.body, new_ciphertext.body);
    assert!(old.send_text([7; 16], "closed", NOW).is_err());
    let mut transplant = old_ciphertext;
    transplant.channel = new_ciphertext.channel;
    assert!(new_rx.receive(a.route_id(), transplant, NOW).is_err());
}

#[test]
fn cancelled_invite_and_elapsed_deadlines_discard_tentative_state() {
    let a = endpoint();
    let b = endpoint();
    let (mut responder, mut initiator) = open_pair(&a, &b);
    let commit = approve(&b, &mut responder, &mut initiator);
    let stored = initiator
        .receive(a.route_id(), commit, NOW)
        .unwrap()
        .pop()
        .unwrap();
    a.cancel_invite().unwrap();
    assert!(responder.receive(b.route_id(), stored, NOW).is_err());
    initiator.cancel();
    assert!(!b.is_trusted(a.route_id()).unwrap());
    assert!(!a.is_trusted(b.route_id()).unwrap());

    let qr = a.create_invite(NOW).unwrap();
    let (_, open) = b.begin_pairing(qr.as_str(), "B", NOW).unwrap();
    let late = Time {
        unix_seconds: NOW.unix_seconds,
        monotonic: after(300).monotonic,
    };
    assert!(a.accept_pairing(b.route_id(), open, "A", late).is_err());
    assert!(a.create_invite(late).is_ok());
}

#[test]
fn pending_sessions_time_out_and_path_substitution_is_rejected() {
    let a = endpoint();
    let b = endpoint();
    pair(&a, &b);
    let (mut stale, first) = a.begin_session(b.route_id(), Path::Relay, NOW).unwrap();
    assert!(
        b.accept_session(a.route_id(), Path::Lan, first.clone(), NOW)
            .is_err()
    );
    let (_responder, second) = b
        .accept_session(a.route_id(), Path::Relay, first, NOW)
        .unwrap();
    assert!(matches!(
        a.begin_session(b.route_id(), Path::Relay, NOW),
        Err(Error::Limit)
    ));
    let (_replacement, new_first) = a
        .begin_session(b.route_id(), Path::Relay, after(10))
        .unwrap();
    assert!(stale.receive(b.route_id(), second, after(10)).is_err());
    assert!(
        b.accept_session(a.route_id(), Path::Relay, new_first, after(10))
            .is_ok()
    );
}

#[test]
fn established_session_expires_and_authenticated_close_retires_the_peer() {
    let a = endpoint();
    let b = endpoint();
    pair(&a, &b);
    let (mut left, mut right) = ready(&a, &b, Path::Relay);
    let close = left.close(NOW).unwrap();
    assert!(matches!(
        right.receive(a.route_id(), close, NOW).unwrap(),
        SessionEvent::Closed
    ));
    assert!(right.send_text([2; 16], "closed", NOW).is_err());
    let (mut left, mut right) = ready(&a, &b, Path::Relay);
    let payload = left.send_text([1; 16], "queued", NOW).unwrap();
    assert!(matches!(
        left.send_text([1; 16], "expired", after(3600)),
        Err(Error::Expired)
    ));
    assert!(matches!(
        right.receive(a.route_id(), payload, after(3600)),
        Err(Error::Expired)
    ));
}

#[test]
fn invalid_label_and_older_sweep_cannot_consume_a_fresh_invitation() {
    let a = endpoint();
    let b = endpoint();
    let qr = a.create_invite(after(1)).unwrap();
    a.expire_pending(NOW).unwrap();
    let (_scanner, open) = b.begin_pairing(qr.as_str(), "B", after(1)).unwrap();
    assert!(
        a.accept_pairing(b.route_id(), open.clone(), "bad\nlabel", after(1))
            .is_err()
    );
    assert!(a.accept_pairing(b.route_id(), open, "A", after(1)).is_ok());
}

#[test]
fn scanner_wall_deadline_releases_provisional_trust() {
    let a = endpoint();
    let b = endpoint();
    let (mut responder, mut initiator) = open_pair(&a, &b);
    let commit = approve(&b, &mut responder, &mut initiator);
    let stored = initiator
        .receive(a.route_id(), commit, NOW)
        .unwrap()
        .pop()
        .unwrap();
    let done = responder
        .receive(b.route_id(), stored, NOW)
        .unwrap()
        .pop()
        .unwrap();
    let wall_expired = Time {
        unix_seconds: after(361).unix_seconds,
        monotonic: after(1).monotonic,
    };
    b.expire_pending(wall_expired).unwrap();
    assert!(matches!(
        initiator.receive(a.route_id(), done, wall_expired),
        Err(Error::Closed)
    ));
    assert!(!b.is_trusted(a.route_id()).unwrap());
}
