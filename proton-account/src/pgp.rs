//! The few OpenPGP operations Proton Drive and Proton Calendar need, behind an
//! object-safe interface so the rest of the crate doesn't depend on which
//! `proton-crypto` back end is compiled in. Keys stay in memory only.

use crate::error::{Error, Result};
use proton_crypto::crypto::{
    DataEncoding, Decryptor, DecryptorSync, PGPProviderSync, VerifiedData,
};
use std::any::Any;
use std::sync::Arc;

/// An unlocked private key.
#[derive(Clone)]
pub struct Key(Arc<dyn Any + Send + Sync>);

/// A decrypted message session key.
#[derive(Clone)]
pub struct SessionKey(Arc<dyn Any + Send + Sync>);

pub trait Pgp: Send + Sync {
    /// Unlock an armored private key with its passphrase.
    fn unlock(&self, armored_key: &str, passphrase: &[u8]) -> Result<Key>;
    /// Decrypt an armored message with any of `keys`.
    fn decrypt_armored(&self, message: &str, keys: &[Key]) -> Result<Vec<u8>>;
    /// Open the session key held in binary key packets.
    fn session_key(&self, key_packets: &[u8], keys: &[Key]) -> Result<SessionKey>;
    /// Decrypt a binary data packet with a session key.
    fn decrypt_with(&self, data: &[u8], key: &SessionKey) -> Result<Vec<u8>>;
}

struct Backend<P>(P);

fn fail(error: impl std::fmt::Display) -> Error {
    Error::Crypto(error.to_string())
}

impl<P> Pgp for Backend<P>
where
    P: PGPProviderSync + Send + Sync + 'static,
{
    fn unlock(&self, armored_key: &str, passphrase: &[u8]) -> Result<Key> {
        let key = self.0.private_key_import(armored_key, passphrase, DataEncoding::Armor).map_err(fail)?;
        Ok(Key(Arc::new(key)))
    }

    fn decrypt_armored(&self, message: &str, keys: &[Key]) -> Result<Vec<u8>> {
        let keys = unwrap_keys::<P>(keys)?;
        let data = self
            .0
            .new_decryptor()
            .with_decryption_key_refs(&keys)
            .decrypt(message, DataEncoding::Armor)
            .map_err(fail)?;
        Ok(data.into_vec())
    }

    fn session_key(&self, key_packets: &[u8], keys: &[Key]) -> Result<SessionKey> {
        let keys = unwrap_keys::<P>(keys)?;
        let key = self
            .0
            .new_decryptor()
            .with_decryption_key_refs(&keys)
            .decrypt_session_key(key_packets)
            .map_err(fail)?;
        Ok(SessionKey(Arc::new(key)))
    }

    fn decrypt_with(&self, data: &[u8], key: &SessionKey) -> Result<Vec<u8>> {
        let key = key
            .0
            .downcast_ref::<P::SessionKey>()
            .ok_or_else(|| Error::Crypto("session key from another back end".into()))?;
        let data = self
            .0
            .new_decryptor()
            .with_session_key_ref(key)
            .decrypt(data, DataEncoding::Bytes)
            .map_err(fail)?;
        Ok(data.into_vec())
    }
}

fn unwrap_keys<P: PGPProviderSync + 'static>(keys: &[Key]) -> Result<Vec<&P::PrivateKey>> {
    keys.iter()
        .map(|key| key.0.downcast_ref::<P::PrivateKey>().ok_or_else(|| Error::Crypto("key from another back end".into())))
        .collect()
}

/// The OpenPGP implementation compiled into Spotty.
pub fn provider() -> Box<dyn Pgp> {
    Box::new(Backend(proton_crypto::new_pgp_provider()))
}
