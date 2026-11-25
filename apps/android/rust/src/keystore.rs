//! The two seed buffers only cross the private JNI storage adapter, never UniFFI.

use std::{cell::RefCell, path::Path, ptr, sync::Mutex};

use jni::{
    JNIEnv,
    objects::{JByteArray, JObject, JString, JValue},
    sys::jstring,
};
use lyteboard_core::peer::{CredentialError, CredentialStore, Credentials, Endpoint, storage};
use zeroize::Zeroizing;

pub(super) static PREPARED: Mutex<Option<Endpoint>> = Mutex::new(None);

struct AndroidStore<'a, 'local> {
    env: RefCell<&'a mut JNIEnv<'local>>,
    backend: JObject<'local>,
}

impl AndroidStore<'_, '_> {
    fn error(env: &mut JNIEnv<'_>) -> CredentialError {
        // Java exceptions do not cross the FFI boundary or print credential data.
        let _ = env.exception_clear();
        CredentialError::Unavailable
    }
}

impl CredentialStore for AndroidStore<'_, '_> {
    fn load(&self) -> Result<Option<Credentials>, CredentialError> {
        let mut env = self.env.borrow_mut();
        let bytes = env
            .call_method(&self.backend, "loadSeeds", "()[B", &[])
            .and_then(|value| value.l())
            .map_err(|_| Self::error(&mut env))?;
        if bytes.is_null() {
            return Ok(None);
        }
        let bytes = JByteArray::from(bytes);
        let result = env.convert_byte_array(&bytes).map(Zeroizing::new);
        let _ = env.set_byte_array_region(&bytes, 0, &[0; 64]);
        let seeds = result.map_err(|_| Self::error(&mut env))?;
        let seeds: &[u8; 64] = seeds
            .as_slice()
            .try_into()
            .map_err(|_| CredentialError::Invalid)?;
        Ok(Some(Credentials::from_seeds(
            seeds[..32]
                .try_into()
                .map_err(|_| CredentialError::Invalid)?,
            seeds[32..]
                .try_into()
                .map_err(|_| CredentialError::Invalid)?,
        )))
    }

    fn store(&mut self, credentials: &Credentials) -> Result<(), CredentialError> {
        let mut seeds = Zeroizing::new([0u8; 64]);
        credentials.with_seeds(|noise, route| {
            seeds[..32].copy_from_slice(noise);
            seeds[32..].copy_from_slice(route);
        });
        let mut env = self.env.borrow_mut();
        let array = env
            .byte_array_from_slice(&*seeds)
            .map_err(|_| Self::error(&mut env))?;
        env.call_method(
            &self.backend,
            "storeSeeds",
            "([B)V",
            &[JValue::Object(&array)],
        )
        .map_err(|_| Self::error(&mut env))?;
        Ok(())
    }
}

// Export the JVM's fixed symbol for CredentialProtection.prepareProfile.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
extern "system" fn Java_app_lyteboard_android_CredentialProtection_prepareProfile<'local>(
    mut env: JNIEnv<'local>,
    backend: JObject<'local>,
    directory: JString<'local>,
) -> jstring {
    let result: Result<(), String> = (|| {
        let path: String = env
            .get_string(&directory)
            .map_err(|_| "Invalid profile directory.".to_owned())?
            .into();
        let mut slot = PREPARED
            .lock()
            .map_err(|_| "Profile initialization failed.".to_owned())?;
        if slot.is_some() {
            return Err("A peer profile is already prepared.".into());
        }
        let mut store = AndroidStore {
            env: RefCell::new(&mut env),
            backend,
        };
        let endpoint =
            storage::open_android_endpoint(Path::new(&path), &mut store).map_err(|error| {
                format!("Cannot open Android peer profile: {error}. Unlock the device and retry.")
            })?;
        *slot = Some(endpoint);
        Ok(())
    })();
    match result {
        Ok(()) => ptr::null_mut(),
        Err(message) => {
            let _ = env.exception_clear();
            env.new_string(message)
                .map_or(ptr::null_mut(), JString::into_raw)
        }
    }
}
