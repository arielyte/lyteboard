use std::{io, time::Duration};

use lyteboard_core::peer::{
    DeviceIdentity, Endpoint, Error as PeerError, PairingStatus, SessionEvent,
    lan::{self, Error, Incoming, Listener, TcpSession},
};
use lyteboard_protocol::peer::{
    LanInvite, MAX_ENVELOPE_BYTES, MAX_TEXT_BYTES, Record, RoutedFrame, WireError,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use zeroize::Zeroizing;

fn endpoint() -> Endpoint {
    Endpoint::new_lan(DeviceIdentity::generate().unwrap())
}

async fn pair(listener: &mut Listener, inviter: &Endpoint, scanner: &Endpoint, qr: &str) {
    let (outgoing, incoming) = tokio::join!(
        lan::connect_pairing(scanner, qr, "scanner"),
        listener.accept(inviter, "inviter"),
    );
    let outgoing = outgoing.unwrap();
    let Incoming::Pairing(incoming) = incoming.unwrap() else {
        panic!("expected pairing");
    };
    assert_eq!(outgoing.peer_key(), inviter.noise_key());
    assert_eq!(incoming.peer_key(), scanner.noise_key());
    assert!(!inviter.is_trusted(scanner.noise_key()).unwrap());
    assert!(!scanner.is_trusted(inviter.noise_key()).unwrap());
    let code = Zeroizing::new(outgoing.verification_code().as_str().to_owned());
    assert_eq!(code.as_str(), incoming.verification_code().as_str());
    let (outgoing, incoming) = tokio::join!(outgoing.approve(&code), incoming.approve(&code));
    assert_eq!(outgoing.unwrap(), PairingStatus::Complete);
    assert_eq!(incoming.unwrap(), PairingStatus::LocalTrustSaved);
    assert!(inviter.is_trusted(scanner.noise_key()).unwrap());
    assert!(scanner.is_trusted(inviter.noise_key()).unwrap());
}

async fn paired() -> (Listener, Endpoint, Endpoint) {
    let mut listener = Listener::bind(None).await.unwrap();
    let inviter = endpoint();
    let scanner = endpoint();
    let qr = listener.invite(&inviter).unwrap();
    pair(&mut listener, &inviter, &scanner, &qr).await;
    (listener, inviter, scanner)
}

async fn session<'a>(
    listener: &mut Listener,
    inviter: &'a Endpoint,
    scanner: &'a Endpoint,
) -> (TcpSession<'a>, TcpSession<'a>) {
    let address = listener.local_addr().unwrap();
    let (outgoing, incoming) = tokio::join!(
        lan::connect_session(scanner, address, inviter.noise_key()),
        listener.accept(inviter, "inviter"),
    );
    let Incoming::Session(incoming) = incoming.unwrap() else {
        panic!("expected session");
    };
    (outgoing.unwrap(), incoming)
}

async fn exchange(sender: &mut TcpSession<'_>, receiver: &mut TcpSession<'_>, text: &str) {
    let sent = sender.send_text(text).await.unwrap();
    let SessionEvent::Text { id, text: received } = receiver.receive().await.unwrap() else {
        panic!("expected text");
    };
    assert_eq!(sent, id);
    assert_eq!(received, text);
    receiver.acknowledge(id).await.unwrap();
    assert!(matches!(
        sender.receive().await.unwrap(),
        SessionEvent::DeliveryAck { id, duplicate: false } if id == sent
    ));
}

#[tokio::test]
async fn two_endpoints_pair_and_exchange_text() {
    let mut listener = Listener::bind(Some("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let inviter = endpoint();
    let scanner = endpoint();
    let qr = listener.invite(&inviter).unwrap();
    let invite = LanInvite::decode_qr(&qr).unwrap();
    assert_eq!(invite.address, listener.local_addr().unwrap());
    assert_eq!(invite.noise_key, inviter.noise_key());
    pair(&mut listener, &inviter, &scanner, &qr).await;

    let (mut outgoing, mut incoming) = session(&mut listener, &inviter, &scanner).await;
    exchange(&mut outgoing, &mut incoming, "Hello, שלום").await;
    exchange(&mut incoming, &mut outgoing, "Reply: café ✓").await;
    outgoing.close().await.unwrap();
    assert!(matches!(
        incoming.receive().await.unwrap(),
        SessionEvent::Closed
    ));
    println!(
        "Paired two loopback endpoints, confirmed a fresh KK session, and acknowledged UTF-8 text in both directions."
    );
}

#[tokio::test]
async fn invalid_pairing_credentials_do_not_consume_a_valid_invite() {
    let mut listener = Listener::bind(None).await.unwrap();
    let inviter = endpoint();
    let scanner = endpoint();
    let qr = listener.invite(&inviter).unwrap();
    let invite = LanInvite::decode_qr(&qr).unwrap();
    let other_key = endpoint().noise_key();
    for (key, psk) in [(other_key, *invite.psk()), (invite.noise_key, [0; 32])] {
        let invalid = LanInvite::new(
            invite.address,
            invite.id,
            Zeroizing::new(psk),
            key,
            invite.expires_at,
        )
        .unwrap()
        .encode_qr();
        let (outgoing, incoming) = tokio::join!(
            lan::connect_pairing(&scanner, &invalid, "scanner"),
            listener.accept(&inviter, "inviter"),
        );
        assert!(matches!(outgoing, Err(Error::Io(_))));
        assert!(matches!(incoming, Err(Error::Peer(_))));
    }
    let expired = LanInvite::new(
        invite.address,
        invite.id,
        Zeroizing::new(*invite.psk()),
        invite.noise_key,
        0,
    )
    .unwrap()
    .encode_qr();
    assert!(matches!(
        lan::connect_pairing(&scanner, &expired, "scanner").await,
        Err(Error::Peer(PeerError::Expired))
    ));
    assert!(inviter.trusted_devices().unwrap().is_empty());

    pair(&mut listener, &inviter, &scanner, &qr).await;
    let stranger = endpoint();
    let (outgoing, incoming) = tokio::join!(
        lan::connect_pairing(&stranger, &qr, "stranger"),
        listener.accept(&inviter, "inviter"),
    );
    assert!(outgoing.is_err());
    assert!(matches!(incoming, Err(Error::Peer(PeerError::Invitation))));
}

#[tokio::test]
async fn mismatched_comparison_closes_pairing_without_trust() {
    let mut listener = Listener::bind(None).await.unwrap();
    let inviter = endpoint();
    let scanner = endpoint();
    let qr = listener.invite(&inviter).unwrap();
    let (outgoing, incoming) = tokio::join!(
        lan::connect_pairing(&scanner, &qr, "scanner"),
        listener.accept(&inviter, "inviter"),
    );
    let Incoming::Pairing(incoming) = incoming.unwrap() else {
        panic!("expected pairing");
    };
    let outgoing = outgoing.unwrap();
    let code = Zeroizing::new(incoming.verification_code().as_str().to_owned());
    let (bad, other) = tokio::join!(outgoing.approve("wrong"), incoming.approve(&code));
    assert!(matches!(bad, Err(Error::Peer(PeerError::Verification))));
    assert!(matches!(other, Err(Error::Io(_))));
    assert!(inviter.trusted_devices().unwrap().is_empty());
    assert!(scanner.trusted_devices().unwrap().is_empty());
}

#[tokio::test]
async fn revoked_peer_cannot_send_or_open_another_session() {
    let (mut listener, inviter, scanner) = paired().await;
    let (mut outgoing, mut incoming) = session(&mut listener, &inviter, &scanner).await;
    inviter.revoke(scanner.noise_key()).unwrap();
    outgoing.send_text("reject after revocation").await.unwrap();
    assert!(matches!(
        incoming.receive().await,
        Err(Error::Peer(PeerError::Untrusted))
    ));
    let address = listener.local_addr().unwrap();
    let (outgoing, incoming) = tokio::join!(
        lan::connect_session(&scanner, address, inviter.noise_key()),
        listener.accept(&inviter, "inviter"),
    );
    assert!(outgoing.is_err());
    assert!(matches!(incoming, Err(Error::Peer(PeerError::Untrusted))));
}

async fn write_raw(stream: &mut TcpStream, bytes: &[u8]) {
    stream
        .write_u32(bytes.len().try_into().unwrap())
        .await
        .unwrap();
    stream.write_all(bytes).await.unwrap();
}

async fn read_raw(stream: &mut TcpStream) -> Vec<u8> {
    let length = stream.read_u32().await.unwrap() as usize;
    assert!(length <= MAX_ENVELOPE_BYTES);
    let mut data = vec![0; length];
    stream.read_exact(&mut data).await.unwrap();
    data
}

#[tokio::test]
async fn modified_ciphertext_and_plaintext_are_rejected_over_tcp() {
    let (mut listener, inviter, scanner) = paired().await;
    for plaintext in [false, true] {
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = listener.local_addr().unwrap();
        let attack = async {
            let (mut client, _) = proxy.accept().await.unwrap();
            let mut server = TcpStream::connect(origin).await.unwrap();
            for _ in 0..2 {
                write_raw(&mut server, &read_raw(&mut client).await).await;
                write_raw(&mut client, &read_raw(&mut server).await).await;
            }
            let (from, mut frame) =
                RoutedFrame::decode_forwarded(&read_raw(&mut client).await).unwrap();
            let text = b"do not expose this payload";
            assert!(!frame.body.windows(text.len()).any(|window| window == text));
            if plaintext {
                frame.body = Record::Text {
                    id: [9; 16],
                    text: String::from_utf8(text.to_vec()).unwrap(),
                }
                .encode()
                .unwrap();
            } else {
                frame.body[0] ^= 1;
            }
            write_raw(&mut server, &frame.encode_forwarded(&from).unwrap()).await;
        };
        let send = async {
            let mut session =
                lan::connect_session(&scanner, proxy.local_addr().unwrap(), inviter.noise_key())
                    .await
                    .unwrap();
            session
                .send_text("do not expose this payload")
                .await
                .unwrap();
        };
        let receive = async {
            let Incoming::Session(mut session) =
                listener.accept(&inviter, "inviter").await.unwrap()
            else {
                panic!("expected session");
            };
            assert!(matches!(
                session.receive().await,
                Err(Error::Peer(PeerError::Crypto(_)))
            ));
            assert!(matches!(session.receive().await, Err(Error::Closed)));
        };
        tokio::join!(attack, send, receive);
    }
}

#[tokio::test]
async fn invalid_lengths_truncated_frames_and_pre_handshake_data_are_rejected() {
    let mut listener = Listener::bind(None).await.unwrap();
    let address = listener.local_addr().unwrap();
    let inviter = endpoint();
    for length in [0, MAX_ENVELOPE_BYTES as u32 + 1, 8, 1] {
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_u32(length).await.unwrap();
        if length == 8 || length == 1 {
            client.write_u8(0).await.unwrap();
        }
        client.shutdown().await.unwrap();
        let result = listener.accept(&inviter, "inviter").await;
        match length {
            0 | 1 => assert!(matches!(result, Err(Error::Wire(WireError::Malformed)))),
            8 => assert!(
                matches!(result, Err(Error::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof)
            ),
            _ => assert!(matches!(result, Err(Error::Wire(WireError::TooLarge)))),
        }
    }
    let mut client = TcpStream::connect(address).await.unwrap();
    let data = RoutedFrame {
        to: inviter.noise_key(),
        channel: [0; 16],
        kind: lyteboard_protocol::peer::FrameKind::Transport,
        selector: None,
        body: vec![0; 16],
    }
    .encode_forwarded(&[1; 32])
    .unwrap();
    write_raw(&mut client, &data).await;
    assert!(matches!(
        listener.accept(&inviter, "inviter").await,
        Err(Error::Unexpected)
    ));
}

#[tokio::test]
async fn a_stalled_frame_times_out_and_the_listener_can_pair_again() {
    let mut listener = Listener::bind(None).await.unwrap();
    let address = listener.local_addr().unwrap();
    assert!(address.ip().is_loopback());
    let inviter = endpoint();
    let scanner = endpoint();
    let mut client = TcpStream::connect(address).await.unwrap();
    client.write_u8(0).await.unwrap();
    assert!(matches!(
        listener.accept(&inviter, "inviter").await,
        Err(Error::Timeout(_))
    ));
    assert_eq!(
        client.read_u8().await.unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );

    let qr = listener.invite(&inviter).unwrap();
    pair(&mut listener, &inviter, &scanner, &qr).await;
}

#[tokio::test]
async fn a_different_endpoint_at_the_session_address_cannot_replace_the_pin() {
    let (_, inviter, scanner) = paired().await;
    let mut impostor_address = Listener::bind(None).await.unwrap();
    let impostor = endpoint();
    let (outgoing, incoming) = tokio::join!(
        lan::connect_session(
            &scanner,
            impostor_address.local_addr().unwrap(),
            inviter.noise_key()
        ),
        impostor_address.accept(&impostor, "impostor"),
    );
    assert!(matches!(outgoing, Err(Error::Io(_))));
    assert!(matches!(incoming, Err(Error::Peer(PeerError::Untrusted))));
    assert!(scanner.is_trusted(inviter.noise_key()).unwrap());
    assert!(!scanner.is_trusted(impostor.noise_key()).unwrap());
}

#[tokio::test]
async fn oversized_text_disconnect_and_cancel_release_channels() {
    let (mut listener, inviter, scanner) = paired().await;
    let (mut outgoing, mut incoming) = session(&mut listener, &inviter, &scanner).await;
    assert!(matches!(
        outgoing.send_text(&"x".repeat(MAX_TEXT_BYTES + 1)).await,
        Err(Error::Peer(PeerError::Limit))
    ));
    assert!(matches!(incoming.receive().await, Err(Error::Io(_))));

    let (mut outgoing, mut incoming) = session(&mut listener, &inviter, &scanner).await;
    assert!(
        timeout(Duration::from_millis(20), incoming.receive())
            .await
            .is_err()
    );
    assert!(matches!(incoming.receive().await, Err(Error::Closed)));
    assert!(matches!(outgoing.receive().await, Err(Error::Io(_))));

    let (mut outgoing, mut incoming) = session(&mut listener, &inviter, &scanner).await;
    exchange(&mut outgoing, &mut incoming, "connected with fresh keys").await;
}

#[tokio::test]
async fn tampered_bulk_chunk_never_reaches_the_application() {
    use lyteboard_core::bulk::Payload;
    use lyteboard_protocol::peer::{BulkRecord, CHUNK_BYTES};

    let (mut listener, inviter, scanner) = paired().await;
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = proxy.local_addr().unwrap();
    let origin = listener.local_addr().unwrap();
    let id = [3; 16];
    let attack = async {
        let (mut client, _) = proxy.accept().await.unwrap();
        let mut server = TcpStream::connect(origin).await.unwrap();
        // Handshake, mutual readiness, then the encrypted manifest and first credit.
        for _ in 0..3 {
            write_raw(&mut server, &read_raw(&mut client).await).await;
            write_raw(&mut client, &read_raw(&mut server).await).await;
        }
        let (from, mut frame) =
            RoutedFrame::decode_forwarded(&read_raw(&mut client).await).unwrap();
        frame.body[100] ^= 1;
        write_raw(&mut server, &frame.encode_forwarded(&from).unwrap()).await;
    };
    let sender = async {
        let mut session = lan::connect_session(&scanner, address, inviter.noise_key())
            .await
            .unwrap();
        let payload = Payload::text("a".repeat(CHUNK_BYTES)).unwrap();
        session
            .send_bulk(BulkRecord::Begin(payload.manifest(id)))
            .await
            .unwrap();
        assert!(matches!(session.receive().await.unwrap(),
            SessionEvent::Bulk(BulkRecord::Credit {id: credit_id, next: 0}) if credit_id == id));
        session
            .send_bulk(BulkRecord::Chunk {
                id,
                index: 0,
                bytes: Zeroizing::new(payload.bytes().to_vec()),
            })
            .await
            .unwrap();
        assert!(session.receive().await.is_err());
    };
    let recipient = async {
        let Incoming::Session(mut session) = listener.accept(&inviter, "inviter").await.unwrap()
        else {
            panic!("expected session");
        };
        assert!(matches!(session.receive().await.unwrap(),
            SessionEvent::Bulk(BulkRecord::Begin(manifest)) if manifest.id == id));
        session
            .send_bulk(BulkRecord::Credit { id, next: 0 })
            .await
            .unwrap();
        assert!(matches!(
            session.receive().await,
            Err(Error::Peer(PeerError::Crypto(_)))
        ));
        assert!(matches!(session.receive().await, Err(Error::Closed)));
    };
    tokio::join!(attack, sender, recipient);
}

#[tokio::test]
async fn disconnect_during_bulk_never_completes_and_peer_can_reconnect() {
    use lyteboard_core::bulk::{Payload, Receiver};
    use lyteboard_protocol::peer::{BulkRecord, CHUNK_BYTES};

    let (mut listener, inviter, scanner) = paired().await;
    let (mut sender, mut recipient) = session(&mut listener, &inviter, &scanner).await;
    let payload = Payload::text("a".repeat(CHUNK_BYTES + 1)).unwrap();
    let id = [5; 16];
    sender
        .send_bulk(BulkRecord::Begin(payload.manifest(id)))
        .await
        .unwrap();
    let SessionEvent::Bulk(BulkRecord::Begin(manifest)) = recipient.receive().await.unwrap() else {
        panic!("expected bulk manifest");
    };
    let mut receiver = Receiver::new(manifest).unwrap();
    recipient
        .send_bulk(BulkRecord::Credit { id, next: 0 })
        .await
        .unwrap();
    assert!(matches!(sender.receive().await.unwrap(),
        SessionEvent::Bulk(BulkRecord::Credit {id: credit_id, next: 0}) if credit_id == id));
    sender
        .send_bulk(BulkRecord::Chunk {
            id,
            index: 0,
            bytes: Zeroizing::new(payload.bytes()[..CHUNK_BYTES].to_vec()),
        })
        .await
        .unwrap();
    let SessionEvent::Bulk(BulkRecord::Chunk { id, index, bytes }) =
        recipient.receive().await.unwrap()
    else {
        panic!("expected bulk chunk");
    };
    receiver.chunk(id, index, &bytes).unwrap();
    drop(sender);
    assert!(recipient.receive().await.is_err());
    assert!(receiver.finish(id).await.is_err());
    drop(recipient);

    let (mut sender, mut recipient) = session(&mut listener, &inviter, &scanner).await;
    exchange(&mut sender, &mut recipient, "fresh session").await;
}
