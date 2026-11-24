use std::fmt;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use thiserror::Error;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

/// Private key material exchanged only with an authorized credential backend.
pub struct Credentials {
    noise: Zeroizing<[u8; 32]>,
    route: Zeroizing<[u8; 32]>,
}

impl Credentials {
    pub fn generate() -> Result<Self, CredentialError> {
        let mut noise = Zeroizing::new([0; 32]);
        let mut route = Zeroizing::new([0; 32]);
        getrandom::fill(&mut noise[..]).map_err(|_| CredentialError::EntropyUnavailable)?;
        getrandom::fill(&mut route[..]).map_err(|_| CredentialError::EntropyUnavailable)?;
        Ok(Self { noise, route })
    }

    /// Restore the two independent seeds returned by a protected credential backend.
    pub fn from_seeds(noise: [u8; 32], route: [u8; 32]) -> Self {
        Self {
            noise: Zeroizing::new(noise),
            route: Zeroizing::new(route),
        }
    }

    /// Expose borrowed seeds only for protected storage. Keep them out of logs and errors.
    pub fn with_seeds<T>(&self, store: impl FnOnce(&[u8; 32], &[u8; 32]) -> T) -> T {
        store(&self.noise, &self.route)
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials").finish_non_exhaustive()
    }
}

/// A platform credential backend. A locked or failed backend returns an error.
pub trait CredentialStore {
    fn load(&self) -> Result<Option<Credentials>, CredentialError>;
    fn store(&mut self, credentials: &Credentials) -> Result<(), CredentialError>;
}

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("system randomness is unavailable")]
    EntropyUnavailable,
    #[error("device credentials have not been provisioned")]
    Missing,
    #[error("the credential store is locked")]
    Locked,
    #[error("the credential store is unavailable")]
    Unavailable,
    #[error("the device credentials are invalid")]
    Invalid,
}

/// Independently keyed Noise and relay identities for one device.
pub struct DeviceIdentity {
    noise: StaticSecret,
    route: SigningKey,
}

impl DeviceIdentity {
    /// Generate an in-memory identity. Provision protected storage explicitly for durability.
    pub fn generate() -> Result<Self, CredentialError> {
        Self::from_credentials(Credentials::generate()?)
    }

    /// Load an identity without generating a replacement when the backend is empty or locked.
    pub fn load(store: &impl CredentialStore) -> Result<Self, CredentialError> {
        Self::from_credentials(store.load()?.ok_or(CredentialError::Missing)?)
    }

    pub fn from_credentials(credentials: Credentials) -> Result<Self, CredentialError> {
        if *credentials.noise == [0; 32]
            || *credentials.route == [0; 32]
            || *credentials.noise == *credentials.route
        {
            return Err(CredentialError::Invalid);
        }
        Ok(Self {
            noise: StaticSecret::from(*credentials.noise),
            route: SigningKey::from_bytes(&credentials.route),
        })
    }

    pub fn noise_key(&self) -> [u8; 32] {
        PublicKey::from(&self.noise).to_bytes()
    }

    pub fn route_id(&self) -> [u8; 32] {
        self.route.verifying_key().to_bytes()
    }

    pub(super) fn noise_secret(&self) -> &[u8; 32] {
        self.noise.as_bytes()
    }

    pub(crate) fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.route.sign(message).to_bytes()
    }
}

impl fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceIdentity").finish_non_exhaustive()
    }
}

pub(super) fn valid_route(route: &[u8; 32]) -> bool {
    VerifyingKey::from_bytes(route).is_ok_and(|key| !key.is_weak())
}

pub(super) fn verify_route(route: &[u8; 32], message: &[u8], proof: &[u8; 64]) -> bool {
    VerifyingKey::from_bytes(route).is_ok_and(|key| {
        key.verify_strict(message, &Signature::from_bytes(proof))
            .is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_routes_use_only_the_ed25519_public_key() {
        let first =
            DeviceIdentity::from_credentials(Credentials::from_seeds([7; 32], [8; 32])).unwrap();
        let changed_noise =
            DeviceIdentity::from_credentials(Credentials::from_seeds([9; 32], [8; 32])).unwrap();
        let changed_route =
            DeviceIdentity::from_credentials(Credentials::from_seeds([7; 32], [9; 32])).unwrap();

        assert_eq!(first.route_id(), changed_noise.route_id());
        assert_ne!(first.noise_key(), changed_noise.noise_key());
        assert_eq!(first.noise_key(), changed_route.noise_key());
        assert_ne!(first.route_id(), changed_route.route_id());

        let message = b"a transcript-bound route proof";
        let proof = first.sign(message);
        assert!(verify_route(&first.route_id(), message, &proof));
        assert!(!verify_route(&changed_route.route_id(), message, &proof));
        assert!(!verify_route(
            &first.route_id(),
            b"another transcript",
            &proof
        ));
        assert!(!valid_route(&[0; 32]));
    }

    #[test]
    fn explicit_credential_loading_fails_closed_and_keeps_identity_stable() {
        struct EphemeralStore(Option<Credentials>);
        impl CredentialStore for EphemeralStore {
            fn load(&self) -> Result<Option<Credentials>, CredentialError> {
                Ok(self.0.as_ref().map(|value| {
                    value.with_seeds(|noise, route| Credentials::from_seeds(*noise, *route))
                }))
            }

            fn store(&mut self, value: &Credentials) -> Result<(), CredentialError> {
                self.0 =
                    Some(value.with_seeds(|noise, route| Credentials::from_seeds(*noise, *route)));
                Ok(())
            }
        }

        let mut store = EphemeralStore(None);
        assert!(matches!(
            DeviceIdentity::load(&store),
            Err(CredentialError::Missing)
        ));
        let credentials = Credentials::generate().unwrap();
        assert_eq!(format!("{credentials:?}"), "Credentials { .. }");
        store.store(&credentials).unwrap();
        let first = DeviceIdentity::load(&store).unwrap();
        let second = DeviceIdentity::load(&store).unwrap();
        assert_eq!(first.route_id(), second.route_id());
        assert_eq!(first.noise_key(), second.noise_key());
        assert_eq!(format!("{first:?}"), "DeviceIdentity { .. }");
    }

    #[test]
    fn locked_or_invalid_credentials_never_generate_a_replacement() {
        struct LockedStore;
        impl CredentialStore for LockedStore {
            fn load(&self) -> Result<Option<Credentials>, CredentialError> {
                Err(CredentialError::Unavailable)
            }

            fn store(&mut self, _: &Credentials) -> Result<(), CredentialError> {
                Err(CredentialError::Unavailable)
            }
        }

        assert!(matches!(
            DeviceIdentity::load(&LockedStore),
            Err(CredentialError::Unavailable)
        ));
        for (noise, route) in [([0; 32], [1; 32]), ([1; 32], [0; 32]), ([1; 32], [1; 32])] {
            assert!(matches!(
                DeviceIdentity::from_credentials(Credentials::from_seeds(noise, route)),
                Err(CredentialError::Invalid)
            ));
        }
    }
}
