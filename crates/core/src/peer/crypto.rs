use lyteboard_protocol::peer::{MAX_CIPHERTEXT_BYTES, MAX_HANDSHAKE_BYTES};
use snow::{
    Builder, Error as NoiseError, HandshakeState, TransportState,
    params::{CipherChoice, DHChoice, HashChoice},
    resolvers::{CryptoResolver, DefaultResolver},
    types::{Cipher, Dh, Hash, Random},
};
use thiserror::Error;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use super::identity::DeviceIdentity;

const XK_PSK: &str = "Noise_XKpsk0_25519_ChaChaPoly_SHA256";
const KK: &str = "Noise_KK_25519_ChaChaPoly_SHA256";
const TAG_BYTES: usize = 16;
const RECORD_LIMIT: u64 = 1 << 20;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("Noise setup failed")]
    Configuration,
    #[error("system randomness is unavailable")]
    EntropyUnavailable,
    #[error("X25519 key agreement failed")]
    Dh,
    #[error("Noise rejected the message")]
    Rejected,
    #[error("Noise messages are out of order")]
    Incomplete,
    #[error("handshake payloads must be empty")]
    HandshakePayload,
    #[error("message length is out of bounds")]
    Size,
    #[error("session record limit reached")]
    RecordLimit,
    #[error("cryptographic state is closed")]
    Closed,
}

impl From<NoiseError> for CryptoError {
    fn from(error: NoiseError) -> Self {
        match error {
            NoiseError::Dh => Self::Dh,
            NoiseError::Rng => Self::EntropyUnavailable,
            _ => Self::Rejected,
        }
    }
}

struct OsRandom;

impl Random for OsRandom {
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), NoiseError> {
        getrandom::fill(dest).map_err(|_| NoiseError::Rng)
    }
}

#[derive(Default)]
struct CheckedX25519 {
    secret: Option<StaticSecret>,
    public: [u8; 32],
}

impl Dh for CheckedX25519 {
    fn name(&self) -> &'static str {
        "25519"
    }
    fn pub_len(&self) -> usize {
        32
    }
    fn priv_len(&self) -> usize {
        32
    }

    fn set(&mut self, private: &[u8]) {
        self.secret = <[u8; 32]>::try_from(private).ok().map(StaticSecret::from);
        self.public = self
            .secret
            .as_ref()
            .map_or([0; 32], |key| PublicKey::from(key).to_bytes());
    }

    fn generate(&mut self, rng: &mut dyn Random) -> Result<(), NoiseError> {
        let mut seed = Zeroizing::new([0u8; 32]);
        rng.try_fill_bytes(&mut seed[..])?;
        self.set(&seed[..]);
        Ok(())
    }

    fn pubkey(&self) -> &[u8] {
        &self.public
    }
    fn privkey(&self) -> &[u8] {
        self.secret.as_ref().map_or(&[], |key| &key.as_bytes()[..])
    }

    fn dh(&self, peer: &[u8], out: &mut [u8]) -> Result<(), NoiseError> {
        let remote = peer.get(..32).ok_or(NoiseError::Dh)?;
        let public = PublicKey::from(<[u8; 32]>::try_from(remote).map_err(|_| NoiseError::Dh)?);
        let shared = self
            .secret
            .as_ref()
            .ok_or(NoiseError::Dh)?
            .diffie_hellman(&public);
        if !shared.was_contributory() {
            return Err(NoiseError::Dh);
        }
        out.get_mut(..32)
            .ok_or(NoiseError::Dh)?
            .copy_from_slice(shared.as_bytes());
        Ok(())
    }
}

struct CheckedResolver;

impl CryptoResolver for CheckedResolver {
    fn resolve_rng(&self) -> Option<Box<dyn Random>> {
        Some(Box::new(OsRandom))
    }

    fn resolve_dh(&self, choice: &DHChoice) -> Option<Box<dyn Dh>> {
        match choice {
            DHChoice::Curve25519 => Some(Box::<CheckedX25519>::default()),
            _ => None,
        }
    }

    fn resolve_hash(&self, choice: &HashChoice) -> Option<Box<dyn Hash>> {
        match choice {
            HashChoice::SHA256 => DefaultResolver.resolve_hash(choice),
            _ => None,
        }
    }

    fn resolve_cipher(&self, choice: &CipherChoice) -> Option<Box<dyn Cipher>> {
        match choice {
            CipherChoice::ChaChaPoly => DefaultResolver.resolve_cipher(choice),
            _ => None,
        }
    }
}

pub(super) struct Handshake(Option<Box<HandshakeState>>);

impl Handshake {
    pub(super) fn new(
        identity: &DeviceIdentity,
        initiator: bool,
        prologue: &[u8],
        remote: Option<&[u8; 32]>,
        pairing_psk: Option<&[u8; 32]>,
    ) -> Result<Self, CryptoError> {
        if remote.is_some() != (initiator || pairing_psk.is_none()) {
            return Err(CryptoError::Configuration);
        }
        let params = if pairing_psk.is_some() { XK_PSK } else { KK }
            .parse()
            .map_err(|_| CryptoError::Configuration)?;
        let mut builder = Builder::with_resolver(params, Box::new(CheckedResolver))
            .local_private_key(identity.noise_secret())
            .map_err(|_| CryptoError::Configuration)?
            .prologue(prologue)
            .map_err(|_| CryptoError::Configuration)?;
        if let Some(pinned) = remote {
            builder = builder
                .remote_public_key(pinned)
                .map_err(|_| CryptoError::Configuration)?;
        }
        if let Some(psk) = pairing_psk {
            builder = builder
                .psk(0, psk)
                .map_err(|_| CryptoError::Configuration)?;
        }
        let state = if initiator {
            builder.build_initiator()
        } else {
            builder.build_responder()
        }
        .map_err(|_| CryptoError::Configuration)?;
        Ok(Self(Some(Box::new(state))))
    }

    pub(super) fn write(&mut self) -> Result<Vec<u8>, CryptoError> {
        let mut state = self.0.take().ok_or(CryptoError::Closed)?;
        let mut message = [0; MAX_HANDSHAKE_BYTES];
        let size = state.write_message(&[], &mut message)?;
        self.0 = Some(state);
        Ok(message[..size].to_vec())
    }

    pub(super) fn read(&mut self, bytes: &[u8]) -> Result<(), CryptoError> {
        let mut state = self.0.take().ok_or(CryptoError::Closed)?;
        if bytes.is_empty() || bytes.len() > MAX_HANDSHAKE_BYTES {
            return Err(CryptoError::Size);
        }
        let mut payload = Zeroizing::new([0; MAX_HANDSHAKE_BYTES]);
        if state.read_message(bytes, &mut payload[..])? != 0 {
            return Err(CryptoError::HandshakePayload);
        }
        self.0 = Some(state);
        Ok(())
    }

    pub(super) fn finish(self) -> Result<([u8; 32], [u8; 32], Transport), CryptoError> {
        let state = self.0.ok_or(CryptoError::Closed)?;
        if !state.is_handshake_finished() {
            return Err(CryptoError::Incomplete);
        }
        let hash = state
            .get_handshake_hash()
            .try_into()
            .map_err(|_| CryptoError::Configuration)?;
        let peer = state
            .get_remote_static()
            .ok_or(CryptoError::Incomplete)?
            .try_into()
            .map_err(|_| CryptoError::Configuration)?;
        Ok((hash, peer, Transport(Some(state.into_transport_mode()?))))
    }
}

pub(super) struct Transport(Option<TransportState>);

impl Transport {
    pub(super) fn seal(&mut self, clear: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let mut state = self.0.take().ok_or(CryptoError::Closed)?;
        if clear.len() > MAX_CIPHERTEXT_BYTES - TAG_BYTES {
            return Err(CryptoError::Size);
        }
        if at_record_limit(&state) {
            return Err(CryptoError::RecordLimit);
        }
        let mut ciphertext = vec![0; clear.len() + TAG_BYTES];
        let len = state.write_message(clear, &mut ciphertext)?;
        ciphertext.truncate(len);
        self.0 = (!at_record_limit(&state)).then_some(state);
        Ok(ciphertext)
    }

    pub(super) fn open(&mut self, ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
        let mut state = self.0.take().ok_or(CryptoError::Closed)?;
        if !(TAG_BYTES..=MAX_CIPHERTEXT_BYTES).contains(&ciphertext.len()) {
            return Err(CryptoError::Size);
        }
        if at_record_limit(&state) {
            return Err(CryptoError::RecordLimit);
        }
        let mut plaintext = Zeroizing::new(vec![0; ciphertext.len() - TAG_BYTES]);
        let len = state.read_message(ciphertext, &mut plaintext)?;
        plaintext.truncate(len);
        self.0 = (!at_record_limit(&state)).then_some(state);
        Ok(plaintext)
    }
}

fn at_record_limit(state: &TransportState) -> bool {
    state.sending_nonce() >= RECORD_LIMIT || state.receiving_nonce() >= RECORD_LIMIT
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex<const N: usize>(value: &str) -> [u8; N] {
        assert_eq!(value.len(), N * 2);
        std::array::from_fn(|i| u8::from_str_radix(&value[i * 2..][..2], 16).unwrap())
    }

    #[test]
    fn checked_provider_matches_rfc_7748_section_6_1() {
        let mut alice = CheckedX25519::default();
        let mut bob = CheckedX25519::default();
        alice.set(&hex::<32>(
            "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a",
        ));
        bob.set(&hex::<32>(
            "5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb",
        ));
        assert_eq!(
            alice.pubkey(),
            hex::<32>("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a")
        );
        assert_eq!(
            bob.pubkey(),
            hex::<32>("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f")
        );
        let expected =
            hex::<32>("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");
        let mut shared = [0; 32];
        alice.dh(bob.pubkey(), &mut shared).unwrap();
        assert!(shared == expected);
        bob.dh(alice.pubkey(), &mut shared).unwrap();
        assert!(shared == expected);
    }

    #[test]
    fn noise_rejects_low_order_ephemeral_and_static_keys() {
        let alice = DeviceIdentity::generate().unwrap();
        let bob = DeviceIdentity::generate().unwrap();
        let alice_pk = alice.noise_key();
        let bob_pk = bob.noise_key();
        let pairing_psk = [42; 32];
        for psk in [None, Some(&pairing_psk)] {
            for low_order in [[0; 32], std::array::from_fn(|i| u8::from(i == 0))] {
                let mut good = Handshake::new(&alice, true, b"test", Some(&bob_pk), psk).unwrap();
                let mut opening = good.write().unwrap();
                opening[..32].copy_from_slice(&low_order);

                let responder_pin = psk.is_none().then_some(&alice_pk);
                let mut responder =
                    Handshake::new(&bob, false, b"test", responder_pin, psk).unwrap();
                assert!(matches!(responder.read(&opening), Err(CryptoError::Dh)));
                assert!(matches!(responder.read(&opening), Err(CryptoError::Closed)));

                let mut initiator =
                    Handshake::new(&alice, true, b"test", Some(&low_order), psk).unwrap();
                assert!(matches!(initiator.write(), Err(CryptoError::Dh)));
                assert!(matches!(initiator.write(), Err(CryptoError::Closed)));
            }
        }
    }

    #[test]
    fn ephemeral_key_generation_propagates_rng_failure() {
        struct BrokenRandom;
        impl Random for BrokenRandom {
            fn try_fill_bytes(&mut self, _: &mut [u8]) -> Result<(), NoiseError> {
                Err(NoiseError::Rng)
            }
        }

        let mut curve = CheckedX25519::default();
        assert!(matches!(
            curve.generate(&mut BrokenRandom),
            Err(NoiseError::Rng)
        ));
        assert!(curve.privkey().is_empty());
    }

    #[test]
    fn checked_provider_matches_a_cacophony_noise_kk_vector() {
        // https://github.com/mcginty/snow/blob/v0.10.0/tests/vectors/cacophony.txt
        let build = |initiator, local: &[u8; 32], remote: &[u8; 32], ephemeral: &[u8; 32]| {
            let builder =
                snow::Builder::with_resolver(KK.parse().unwrap(), Box::new(CheckedResolver))
                    .local_private_key(local)
                    .unwrap()
                    .remote_public_key(remote)
                    .unwrap()
                    .fixed_ephemeral_key_for_testing_only(ephemeral)
                    .prologue(b"John Galt")
                    .unwrap();
            if initiator {
                builder.build_initiator().unwrap()
            } else {
                builder.build_responder().unwrap()
            }
        };
        let mut alice = build(
            true,
            &hex("e61ef9919cde45dd5f82166404bd08e38bceb5dfdfded0a34c8df7ed542214d1"),
            &hex("31e0303fd6418d2f8c0e78b91f22e8caed0fbe48656dcf4767e4834f701b8f62"),
            &hex("893e28b9dc6ca8d611ab664754b8ceb7bac5117349a4439a6b0569da977c464a"),
        );
        let mut bob = build(
            false,
            &hex("4a3acbfdb163dec651dfa3194dece676d437029c62a408b4c5ea9114246e4893"),
            &hex("6bc3822a2aa7f4e6981d6538692b3cdf3e6df9eea6ed269eb41d93c22757b75a"),
            &hex("bbdb4cdbd309f1a1f2e1456967fe288cadd6f712d65dc7b7793d5e63da6b375b"),
        );
        let mut message = [0; 128];
        let mut output = [0; 128];
        let n = alice
            .write_message(b"Ludwig von Mises", &mut message)
            .unwrap();
        assert_eq!(
            message[..n],
            hex::<64>(
                "ca35def5ae56cec33dc2036731ab14896bc4c75dbb07a61f879f8e3afa4c79440177015efc1fe7a37c629af7120a96274e6ab7afcc9261901d0e09ae32a5bb96"
            )
        );
        bob.read_message(&message[..n], &mut output).unwrap();
        let n = bob.write_message(b"Murray Rothbard", &mut message).unwrap();
        assert_eq!(
            message[..n],
            hex::<63>(
                "95ebc60d2b1fa672c1f46a8aa265ef51bfe38e7ccb39ec5be34069f144808843b274d3429adc47ca093ba63ef90f8da89fda108db471dccfa4894aa7b00003"
            )
        );
        alice.read_message(&message[..n], &mut output).unwrap();
        assert!(
            alice.get_handshake_hash()
                == hex::<32>("24c6b51ecb76277140ca018b5985bc9f03de321dae2d34dcae433dafef0131d9")
        );
        let n = alice
            .into_transport_mode()
            .unwrap()
            .write_message(b"F. A. Hayek", &mut message)
            .unwrap();
        assert_eq!(
            message[..n],
            hex::<27>("966b05bc69ec01b8454d3160a214e6f24a3d884eb31ec2408af63f")
        );
    }

    #[test]
    fn refuses_authenticated_handshake_payload_and_closes_the_state() {
        let alice = DeviceIdentity::generate().unwrap();
        let bob = DeviceIdentity::generate().unwrap();
        let bob_pk = bob.noise_key();
        let alice_pk = alice.noise_key();
        let mut raw = Handshake::new(&alice, true, b"test", Some(&bob_pk), None)
            .unwrap()
            .0
            .unwrap();
        let mut responder = Handshake::new(&bob, false, b"test", Some(&alice_pk), None).unwrap();
        let mut message = [0; 128];
        let n = raw
            .write_message(b"forbidden test payload", &mut message)
            .unwrap();
        assert!(matches!(
            responder.read(&message[..n]),
            Err(CryptoError::HandshakePayload)
        ));
        assert!(matches!(responder.write(), Err(CryptoError::Closed)));
    }

    #[test]
    fn transport_closes_when_its_size_or_record_budget_is_exceeded() {
        let alice = DeviceIdentity::generate().unwrap();
        let bob = DeviceIdentity::generate().unwrap();
        let mut initiator =
            Handshake::new(&alice, true, b"test", Some(&bob.noise_key()), None).unwrap();
        let mut responder =
            Handshake::new(&bob, false, b"test", Some(&alice.noise_key()), None).unwrap();
        responder.read(&initiator.write().unwrap()).unwrap();
        initiator.read(&responder.write().unwrap()).unwrap();
        let (_, _, mut sender) = initiator.finish().unwrap();
        let (_, _, mut receiver) = responder.finish().unwrap();

        let longest = vec![42; MAX_CIPHERTEXT_BYTES - TAG_BYTES];
        let ciphertext = sender.seal(&longest).unwrap();
        assert_eq!(ciphertext.len(), MAX_CIPHERTEXT_BYTES);
        assert!(receiver.open(&ciphertext).unwrap().as_slice() == longest);

        let oversized = vec![42; longest.len() + 1];
        assert!(matches!(sender.seal(&oversized), Err(CryptoError::Size)));
        assert!(matches!(sender.seal(b"retry"), Err(CryptoError::Closed)));

        receiver
            .0
            .as_mut()
            .unwrap()
            .set_receiving_nonce(RECORD_LIMIT);
        assert!(matches!(
            receiver.seal(b"reverse direction"),
            Err(CryptoError::RecordLimit)
        ));
        assert!(matches!(
            receiver.open(&ciphertext),
            Err(CryptoError::Closed)
        ));
    }
}
