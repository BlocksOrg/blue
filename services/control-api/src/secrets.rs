use aes_gcm::{
    aead::{rand_core::RngCore, Aead, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use aws_sdk_kms::{primitives::Blob, Client as KmsClient};
use base64::{engine::general_purpose::STANDARD, Engine};
use zeroize::Zeroizing;

use crate::ApiError;

const CONTEXT: &str = "blue-gateway-credential-v1";

#[derive(Clone)]
pub enum SecretProtector {
    Environment { key: [u8; 32], key_id: String },
    AwsKms { client: KmsClient, key_id: String },
}

pub struct Envelope {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
    pub wrapped_key: Vec<u8>,
    pub key_id: String,
}

impl SecretProtector {
    pub async fn environment(reference: &str) -> Result<Self, ApiError> {
        let value = super::resolve_secret_reference(reference)?;
        let decoded = STANDARD
            .decode(value.trim())
            .map_err(|_| ApiError::internal("gateway encryption key must be base64"))?;
        let key: [u8; 32] = decoded.try_into().map_err(|_| {
            ApiError::internal("gateway encryption key must decode to exactly 32 bytes")
        })?;
        Ok(Self::Environment {
            key,
            key_id: "environment:v1".into(),
        })
    }

    pub async fn aws_kms(key_id: String) -> Result<Self, ApiError> {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        Ok(Self::AwsKms {
            client: KmsClient::new(&config),
            key_id,
        })
    }

    pub async fn encrypt(&self, plaintext: &[u8]) -> Result<Envelope, ApiError> {
        let mut dek = Zeroizing::new([0u8; 32]);
        OsRng.fill_bytes(dek.as_mut());
        let cipher = Aes256Gcm::new_from_slice(dek.as_ref())
            .map_err(|_| ApiError::internal("initializing credential encryption"))?;
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|_| ApiError::internal("encrypting gateway credential"))?;
        let (wrapped_key, key_id) = match self {
            Self::Environment { key, key_id } => {
                let wrapper = Aes256Gcm::new_from_slice(key)
                    .map_err(|_| ApiError::internal("initializing key encryption"))?;
                let mut wrap_nonce = [0u8; 12];
                OsRng.fill_bytes(&mut wrap_nonce);
                let encrypted = wrapper
                    .encrypt(Nonce::from_slice(&wrap_nonce), dek.as_ref())
                    .map_err(|_| ApiError::internal("wrapping data encryption key"))?;
                let mut wrapped = wrap_nonce.to_vec();
                wrapped.extend(encrypted);
                (wrapped, key_id.clone())
            }
            Self::AwsKms { client, key_id } => {
                let output = client
                    .encrypt()
                    .key_id(key_id)
                    .plaintext(Blob::new(dek.to_vec()))
                    .encryption_context("purpose", CONTEXT)
                    .send()
                    .await
                    .map_err(|error| {
                        tracing::error!(%error, "AWS KMS encryption failed");
                        ApiError::internal("AWS KMS could not encrypt the gateway data key")
                    })?;
                (
                    output
                        .ciphertext_blob()
                        .ok_or_else(|| ApiError::internal("AWS KMS returned no ciphertext"))?
                        .as_ref()
                        .to_vec(),
                    key_id.clone(),
                )
            }
        };
        Ok(Envelope {
            ciphertext,
            nonce: nonce.to_vec(),
            wrapped_key,
            key_id,
        })
    }

    pub async fn decrypt(&self, envelope: &Envelope) -> Result<Zeroizing<Vec<u8>>, ApiError> {
        let dek = match self {
            Self::Environment { key, key_id } => {
                if envelope.key_id != *key_id || envelope.wrapped_key.len() < 13 {
                    return Err(ApiError::internal(
                        "gateway credential encryption key does not match configured provider",
                    ));
                }
                let wrapper = Aes256Gcm::new_from_slice(key)
                    .map_err(|_| ApiError::internal("initializing key decryption"))?;
                Zeroizing::new(
                    wrapper
                        .decrypt(
                            Nonce::from_slice(&envelope.wrapped_key[..12]),
                            &envelope.wrapped_key[12..],
                        )
                        .map_err(|_| {
                            ApiError::internal("gateway data key authentication failed")
                        })?,
                )
            }
            Self::AwsKms { client, .. } => {
                let output = client
                    .decrypt()
                    .ciphertext_blob(Blob::new(envelope.wrapped_key.clone()))
                    .encryption_context("purpose", CONTEXT)
                    .send()
                    .await
                    .map_err(|error| {
                        tracing::error!(%error, "AWS KMS decryption failed");
                        ApiError::internal("AWS KMS could not decrypt the gateway data key")
                    })?;
                Zeroizing::new(
                    output
                        .plaintext()
                        .ok_or_else(|| ApiError::internal("AWS KMS returned no plaintext"))?
                        .as_ref()
                        .to_vec(),
                )
            }
        };
        let cipher = Aes256Gcm::new_from_slice(dek.as_ref())
            .map_err(|_| ApiError::internal("initializing credential decryption"))?;
        Ok(Zeroizing::new(
            cipher
                .decrypt(
                    Nonce::from_slice(&envelope.nonce),
                    envelope.ciphertext.as_ref(),
                )
                .map_err(|_| ApiError::internal("gateway credential authentication failed"))?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn environment_envelopes_round_trip_and_detect_tampering() {
        let protector = SecretProtector::Environment {
            key: [7; 32],
            key_id: "environment:v1".into(),
        };
        let mut envelope = protector.encrypt(b"sk-sensitive").await.unwrap();
        assert_ne!(envelope.ciphertext, b"sk-sensitive");
        assert_eq!(
            protector.decrypt(&envelope).await.unwrap().as_slice(),
            b"sk-sensitive"
        );
        envelope.ciphertext[0] ^= 1;
        assert!(protector.decrypt(&envelope).await.is_err());
    }

    #[tokio::test]
    async fn every_envelope_uses_fresh_material() {
        let protector = SecretProtector::Environment {
            key: [9; 32],
            key_id: "environment:v1".into(),
        };
        let left = protector.encrypt(b"same").await.unwrap();
        let right = protector.encrypt(b"same").await.unwrap();
        assert_ne!(left.nonce, right.nonce);
        assert_ne!(left.wrapped_key, right.wrapped_key);
        assert_ne!(left.ciphertext, right.ciphertext);
    }
}
