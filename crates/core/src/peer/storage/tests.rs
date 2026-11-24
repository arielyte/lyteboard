use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};

use tempfile::TempDir;
use tokio::join;

use super::*;
use crate::peer::{
    Error as PeerError, SessionEvent,
    lan::{self, Incoming, Listener},
};

#[derive(Default)]
struct EphemeralCredentials {
    item: Option<Credentials>,
    failure: Option<fn() -> CredentialError>,
    writes: usize,
}

impl CredentialStore for EphemeralCredentials {
    fn load(&self) -> Result<Option<Credentials>, CredentialError> {
        if let Some(failure) = self.failure {
            return Err(failure());
        }
        Ok(self.item.as_ref().map(copy_credentials))
    }

    fn store(&mut self, credentials: &Credentials) -> Result<(), CredentialError> {
        self.item = Some(copy_credentials(credentials));
        self.writes += 1;
        Ok(())
    }
}

fn copy_credentials(credentials: &Credentials) -> Credentials {
    credentials.with_seeds(|noise, route| Credentials::from_seeds(*noise, *route))
}

fn open(path: &Path, credentials: &mut EphemeralCredentials) -> Result<Endpoint, StorageError> {
    Profile::open(path)?.load(credentials)
}

#[test]
fn creates_and_reloads_an_identity_with_private_metadata() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("profile");
    let mut credentials = EphemeralCredentials::default();
    let endpoint = open(&path, &mut credentials).unwrap();
    let keys = (endpoint.noise_key(), endpoint.route_id());
    assert!(matches!(
        open(&path, &mut credentials),
        Err(StorageError::InUse)
    ));
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
    for file in [IDENTITY_FILE, TRUST_FILE, "profile.lock"] {
        assert_eq!(fs::metadata(path.join(file)).unwrap().mode() & 0o777, 0o600);
    }
    let anchor: Owner =
        serde_json::from_slice(&fs::read(path.join(IDENTITY_FILE)).unwrap()).unwrap();
    assert_eq!((anchor.noise, anchor.route), keys);
    drop(endpoint);

    let endpoint = open(&path, &mut credentials).unwrap();
    assert_eq!((endpoint.noise_key(), endpoint.route_id()), keys);
    assert_eq!(credentials.writes, 1);
}

#[test]
fn locked_unavailable_or_missing_credentials_do_not_create_replacements() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("profile");
    let mut credentials = EphemeralCredentials {
        failure: Some(|| CredentialError::Locked),
        ..EphemeralCredentials::default()
    };
    assert!(matches!(
        open(&path, &mut credentials),
        Err(StorageError::Credential(CredentialError::Locked))
    ));
    assert!(!path.join(IDENTITY_FILE).exists());
    assert!(!path.join(TRUST_FILE).exists());
    assert_eq!(credentials.writes, 0);

    credentials.failure = None;
    drop(open(&path, &mut credentials).unwrap());
    credentials.failure = Some(|| CredentialError::Unavailable);
    assert!(matches!(
        open(&path, &mut credentials),
        Err(StorageError::Credential(CredentialError::Unavailable))
    ));
    credentials.failure = None;
    credentials.item = None;
    assert!(matches!(
        open(&path, &mut credentials),
        Err(StorageError::Credential(CredentialError::Missing))
    ));
    assert_eq!(credentials.writes, 1);
}

#[test]
fn missing_corrupt_inaccessible_or_mismatched_profiles_fail_closed() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("profile");
    let mut credentials = EphemeralCredentials::default();
    drop(open(&path, &mut credentials).unwrap());
    let trust = path.join(TRUST_FILE);
    let anchor = path.join(IDENTITY_FILE);
    let original = fs::read(&trust).unwrap();

    fs::write(&trust, b"{").unwrap();
    assert!(matches!(
        open(&path, &mut credentials),
        Err(StorageError::Invalid)
    ));
    fs::write(&trust, &original).unwrap();
    fs::set_permissions(&trust, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        open(&path, &mut credentials),
        Err(StorageError::UnsafePath)
    ));

    fs::remove_file(&trust).unwrap();
    assert!(matches!(
        open(&path, &mut credentials),
        Err(StorageError::MissingTrust)
    ));
    symlink(&anchor, &trust).unwrap();
    assert!(matches!(
        open(&path, &mut credentials),
        Err(StorageError::Io(_))
    ));
    fs::remove_file(&trust).unwrap();
    let mut other = EphemeralCredentials {
        item: Some(Credentials::generate().unwrap()),
        ..EphemeralCredentials::default()
    };
    let profile = Profile::open(&path).unwrap();
    profile.write(TRUST_FILE, &original, TRUST_LIMIT).unwrap();
    drop(profile);
    assert!(matches!(
        open(&path, &mut other),
        Err(StorageError::WrongIdentity)
    ));
    fs::write(&anchor, b"{").unwrap();
    assert!(matches!(
        open(&path, &mut credentials),
        Err(StorageError::Invalid)
    ));
}

#[test]
fn snapshot_rejects_invalid_bindings() {
    let identity = DeviceIdentity::generate().unwrap();
    let peer = DeviceIdentity::generate().unwrap().noise_key();
    let owner = Owner::from_identity(&identity);
    for (key, label, address, duplicate) in [
        (owner.noise, "peer", None, false),
        ([0; 32], "peer", None, false),
        (peer, "bad\nname", None, false),
        (peer, "peer", Some("0.0.0.0:8777".parse().unwrap()), false),
        (peer, "peer", None, true),
    ] {
        let snapshot = TrustSnapshot {
            version: VERSION,
            owner: owner.clone(),
            devices: (0..if duplicate { 2 } else { 1 })
                .map(|_| StoredDevice {
                    noise_key: key,
                    label: label.into(),
                    last_known_address: address,
                    revoked: false,
                })
                .collect(),
        };
        assert!(matches!(
            snapshot.into_records(&owner),
            Err(StorageError::Invalid)
        ));
    }
}

#[test]
fn a_partial_temporary_file_does_not_replace_committed_trust() {
    let root = TempDir::new().unwrap();
    let profile_path = root.path().join("profile");
    let path = profile_path.as_path();
    let mut credentials = EphemeralCredentials::default();
    let endpoint = open(path, &mut credentials).unwrap();
    let peer = DeviceIdentity::generate().unwrap().noise_key();
    let pending = DeviceIdentity::generate().unwrap().noise_key();
    {
        let mut state = endpoint.state().unwrap();
        for (id, key) in [(1, peer), (2, pending)] {
            state
                .trust
                .stage(
                    id,
                    TrustedDevice {
                        route: key,
                        noise_key: key,
                        label: "peer".into(),
                        last_known_address: None,
                    },
                )
                .unwrap();
        }
        state.trust.activate(1).unwrap();
    }
    endpoint.revoke(peer).unwrap();
    drop(endpoint);

    let profile = Profile::open(path).unwrap();
    profile
        .write(
            ".00000000000000000000000000000000.tmp",
            b"{\"partial\"",
            TRUST_LIMIT,
        )
        .unwrap();
    // Also simulate an interruption after the first trust snapshot but before its owner anchor.
    fs::remove_file(path.join(IDENTITY_FILE)).unwrap();
    let endpoint = profile.load(&mut credentials).unwrap();
    assert_eq!(credentials.writes, 1);
    assert!(path.join(IDENTITY_FILE).exists());
    assert!(endpoint.trusted_devices().unwrap().is_empty());
    assert!(endpoint.state().unwrap().trust.revoke(&pending).is_err());
    let persisted: TrustSnapshot =
        serde_json::from_slice(&fs::read(path.join(TRUST_FILE)).unwrap()).unwrap();
    assert_eq!(persisted.devices.len(), 1);
    assert_eq!(persisted.devices[0].noise_key, peer);
    assert!(persisted.devices[0].revoked);
}

#[test]
fn a_failed_replacement_does_not_publish_new_trust() {
    let root = TempDir::new().unwrap();
    let profile_path = root.path().join("profile");
    let path = profile_path.as_path();
    let mut credentials = EphemeralCredentials::default();
    let endpoint = open(path, &mut credentials).unwrap();
    let peer = DeviceIdentity::generate().unwrap().noise_key();
    let trust = path.join(TRUST_FILE);
    let old = path.join("committed-trust");
    fs::rename(&trust, &old).unwrap();
    fs::create_dir(&trust).unwrap(); // rename of a file over a directory must fail
    let mut state = endpoint.state().unwrap();
    state
        .trust
        .stage(
            1,
            TrustedDevice {
                route: peer,
                noise_key: peer,
                label: "peer".into(),
                last_known_address: None,
            },
        )
        .unwrap();
    assert!(matches!(
        state.trust.activate(1),
        Err(PeerError::Storage(StorageError::Io(_)))
    ));
    assert!(state.trust.active(&peer).is_none());
    assert!(!fs::read_dir(path).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")
    }));
    drop(state);
    drop(endpoint);

    fs::remove_dir(&trust).unwrap();
    fs::rename(&old, &trust).unwrap();
    let endpoint = open(path, &mut credentials).unwrap();
    assert!(!endpoint.is_trusted(peer).unwrap());
}

async fn pair(listener: &mut Listener, inviter: &Endpoint, scanner: &Endpoint, name: &str) {
    let qr = listener.invite(inviter).unwrap();
    let (outgoing, incoming) = join!(
        lan::connect_pairing(scanner, &qr, name),
        listener.accept(inviter, "inviter"),
    );
    let outgoing = outgoing.unwrap();
    let Incoming::Pairing(incoming) = incoming.unwrap() else {
        panic!("expected pairing");
    };
    let code = Zeroizing::new(outgoing.verification_code().as_str().to_owned());
    assert_eq!(code.as_str(), incoming.verification_code().as_str());
    let (outgoing, incoming) = join!(outgoing.approve(&code), incoming.approve(&code));
    outgoing.unwrap();
    incoming.unwrap();
}

#[tokio::test]
async fn peers_and_revocations_survive_restart_and_a_fresh_tcp_session() {
    let root = TempDir::new().unwrap();
    let a_path = root.path().join("inviter");
    let b_path = root.path().join("scanner");
    let mut a_credentials = EphemeralCredentials::default();
    let mut b_credentials = EphemeralCredentials::default();
    let a = open(&a_path, &mut a_credentials).unwrap();
    let b = open(&b_path, &mut b_credentials).unwrap();
    let c = Endpoint::new_lan(DeviceIdentity::generate().unwrap());
    let mut listener = Listener::bind(None).await.unwrap();
    let original_address = listener.local_addr().unwrap();

    pair(&mut listener, &a, &b, "scanner β").await;
    pair(&mut listener, &a, &c, "second scanner").await;
    let (a_key, b_key, c_key) = (a.noise_key(), b.noise_key(), c.noise_key());
    assert_eq!(a.trusted_devices().unwrap().len(), 2);
    assert!(
        a.trusted_devices()
            .unwrap()
            .iter()
            .all(|d| d.last_known_address.is_none())
    );
    a.remember_address(c_key, "127.0.0.1:53000".parse().unwrap())
        .unwrap();
    drop((a, b));

    let a = open(&a_path, &mut a_credentials).unwrap();
    let b = open(&b_path, &mut b_credentials).unwrap();
    assert_eq!((a.noise_key(), b.noise_key()), (a_key, b_key));
    let devices = a.trusted_devices().unwrap();
    assert_eq!(devices.len(), 2);
    let first = devices.iter().find(|d| d.noise_key == b_key).unwrap();
    let second = devices.iter().find(|d| d.noise_key == c_key).unwrap();
    assert_eq!(first.label, "scanner β");
    assert_eq!(second.label, "second scanner");
    assert_eq!(
        second.last_known_address,
        Some("127.0.0.1:53000".parse().unwrap())
    );
    a.revoke(c_key).unwrap();
    drop(a);
    let a = open(&a_path, &mut a_credentials).unwrap();
    assert!(a.is_trusted(b_key).unwrap());
    assert!(!a.is_trusted(c_key).unwrap());
    assert_eq!(
        b.trusted_devices().unwrap()[0].last_known_address,
        Some(original_address)
    );

    let mut new_listener = Listener::bind(None).await.unwrap();
    let address = new_listener.local_addr().unwrap();
    let (opened, rejected) = join!(
        lan::connect_session(&c, address, a_key),
        new_listener.accept(&a, "inviter"),
    );
    assert!(opened.is_err());
    assert!(matches!(
        rejected,
        Err(lan::Error::Peer(PeerError::Untrusted))
    ));

    {
        let (outgoing, incoming) = join!(
            lan::connect_session(&b, address, a_key),
            new_listener.accept(&a, "inviter"),
        );
        let mut outgoing = outgoing.unwrap();
        let Incoming::Session(mut incoming) = incoming.unwrap() else {
            panic!("expected KK session");
        };
        let sent = outgoing.send_text("after restart").await.unwrap();
        let SessionEvent::Text { id, text } = incoming.receive().await.unwrap() else {
            panic!("expected text");
        };
        assert_eq!(text, "after restart");
        assert_eq!(id, sent);
        incoming.acknowledge(id).await.unwrap();
        assert!(
            matches!(outgoing.receive().await.unwrap(), SessionEvent::DeliveryAck { id, .. } if id == sent)
        );
    }
    drop(b);
    let b = open(&b_path, &mut b_credentials).unwrap();
    assert_eq!(
        b.trusted_devices().unwrap()[0].last_known_address,
        Some(address)
    );
}
