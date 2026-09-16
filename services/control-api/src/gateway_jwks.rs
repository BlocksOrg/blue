//! Gateway inference JWT signing keys.
//!
//! Operators supply only RSA private keys. Every published verification key,
//! and its `kid`, is derived here, so there is no hand-written JWKS that can
//! drift out of sync with the key that actually signs.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use jsonwebtoken::jwk::{
    AlgorithmParameters, CommonParameters, Jwk, JwkSet, KeyAlgorithm, PublicKeyUse,
    RSAKeyParameters, RSAKeyType,
};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use pkcs8::der::SecretDocument;
use pkcs8::ObjectIdentifier;
use sha2::{Digest, Sha256};

const MIN_RSA_MODULUS_BITS: usize = 2048;
const RSA_ENCRYPTION_OID: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");

pub(crate) struct GatewaySigningKeys {
    pub(crate) active_kid: String,
    pub(crate) signing_key: EncodingKey,
    /// The active key first, then the previous key when one is configured.
    pub(crate) public_jwks: JwkSet,
}

impl GatewaySigningKeys {
    /// The active key signs. The previous key is only published, so tokens it
    /// signed before a rotation keep verifying until they expire.
    pub(crate) fn derive(active_pem: &str, previous_pem: Option<&str>) -> Result<Self, String> {
        let active = public_jwk_from_private_pem(active_pem)
            .map_err(|error| format!("decoding gateway JWT private key: {error}"))?;
        let active_kid = active
            .common
            .key_id
            .clone()
            .ok_or("derived gateway JWT key has no kid")?;
        let signing_key = EncodingKey::from_rsa_pem(active_pem.as_bytes())
            .map_err(|error| format!("decoding gateway JWT private key: {error}"))?;
        verify_signing_key_matches(&signing_key, &active)?;

        let mut keys = vec![active];
        if let Some(previous_pem) = previous_pem {
            let previous = public_jwk_from_private_pem(previous_pem)
                .map_err(|error| format!("decoding previous gateway JWT private key: {error}"))?;
            if previous.common.key_id.as_deref() == Some(active_kid.as_str()) {
                return Err("previous gateway JWT key is the same as the active key".into());
            }
            keys.push(previous);
        }

        Ok(Self {
            active_kid,
            signing_key,
            public_jwks: JwkSet { keys },
        })
    }
}

/// Sign a probe with the key ring's signing key and check it against the
/// derived public key, so a derivation bug fails at startup, not per request.
fn verify_signing_key_matches(signing_key: &EncodingKey, active: &Jwk) -> Result<(), String> {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = active.common.key_id.clone();
    let probe = encode(
        &header,
        &serde_json::json!({ "sub": "gateway-key-probe" }),
        signing_key,
    )
    .map_err(|error| format!("testing gateway JWT signing key: {error}"))?;
    let verification_key = DecodingKey::from_jwk(active)
        .map_err(|error| format!("decoding derived gateway JWT verification key: {error}"))?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.required_spec_claims.clear();
    validation.validate_exp = false;
    decode::<serde_json::Value>(&probe, &verification_key, &validation)
        .map(|_| ())
        .map_err(|_| {
            "gateway JWT private key does not verify against its derived public key".into()
        })
}

/// Derive the RS256 public JWK of an RSA private key given as PKCS#8
/// (`PRIVATE KEY`) or PKCS#1 (`RSA PRIVATE KEY`) PEM. The `kid` is the
/// key's RFC 7638 thumbprint, so it changes whenever the key does.
pub(crate) fn public_jwk_from_private_pem(pem: &str) -> Result<Jwk, String> {
    let (label, document) =
        SecretDocument::from_pem(pem.trim()).map_err(|error| format!("invalid PEM: {error}"))?;
    let pkcs8_info;
    let rsa_der = match label {
        "PRIVATE KEY" => {
            pkcs8_info = pkcs8::PrivateKeyInfo::try_from(document.as_bytes())
                .map_err(|error| format!("invalid PKCS#8 private key: {error}"))?;
            if pkcs8_info.algorithm.oid != RSA_ENCRYPTION_OID {
                return Err(format!(
                    "unsupported key algorithm {}; gateway JWTs require an RSA key",
                    pkcs8_info.algorithm.oid
                ));
            }
            pkcs8_info.private_key
        }
        "RSA PRIVATE KEY" => document.as_bytes(),
        other => {
            return Err(format!(
                "unsupported PEM label \"{other}\"; gateway JWTs require an RSA key \
                 (PRIVATE KEY or RSA PRIVATE KEY)"
            ))
        }
    };
    let key = pkcs1::RsaPrivateKey::try_from(rsa_der)
        .map_err(|error| format!("invalid RSA private key: {error}"))?;
    let modulus = strip_leading_zeros(key.modulus.as_bytes());
    let exponent = strip_leading_zeros(key.public_exponent.as_bytes());
    let bits = bit_length(modulus);
    if bits < MIN_RSA_MODULUS_BITS {
        return Err(format!(
            "RSA modulus is {bits} bits; gateway JWTs require at least {MIN_RSA_MODULUS_BITS}"
        ));
    }
    let n = URL_SAFE_NO_PAD.encode(modulus);
    let e = URL_SAFE_NO_PAD.encode(exponent);
    let kid = rsa_thumbprint(&n, &e);
    Ok(Jwk {
        common: CommonParameters {
            public_key_use: Some(PublicKeyUse::Signature),
            key_algorithm: Some(KeyAlgorithm::RS256),
            key_id: Some(kid),
            ..Default::default()
        },
        algorithm: AlgorithmParameters::RSA(RSAKeyParameters {
            key_type: RSAKeyType::RSA,
            n,
            e,
        }),
    })
}

fn strip_leading_zeros(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(bytes.len());
    &bytes[start..]
}

fn bit_length(big_endian: &[u8]) -> usize {
    match big_endian.first() {
        Some(first) => big_endian.len() * 8 - first.leading_zeros() as usize,
        None => 0,
    }
}

/// RFC 7638 JWK thumbprint: SHA-256 over the required RSA members in
/// lexicographic order with no whitespace. base64url values need no escaping.
fn rsa_thumbprint(n: &str, e: &str) -> String {
    let canonical = format!(r#"{{"e":"{e}","kty":"RSA","n":"{n}"}}"#);
    URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pkcs8::der::asn1::UintRef;
    use pkcs8::der::pem::LineEnding;
    use pkcs8::der::Encode;
    use pkcs8::spki::AlgorithmIdentifierRef;

    // The repository's one committed test key. Every other key these tests need
    // is built in code below, so no further key files are checked in.
    const FIXTURE_PKCS8: &str = include_str!("../../../tests/fixtures/jwt/signing-key.pem");
    const FIXTURE_JWKS: &str = include_str!("../../../tests/fixtures/jwt/jwks.json");

    fn pem(label: &str, der: &[u8]) -> String {
        pkcs8::der::pem::encode_string(label, LineEnding::LF, der).unwrap()
    }

    /// The fixture key re-encoded as PKCS#1 (`RSA PRIVATE KEY`).
    fn fixture_pkcs1_pem() -> String {
        let (_, document) = SecretDocument::from_pem(FIXTURE_PKCS8).unwrap();
        let info = pkcs8::PrivateKeyInfo::try_from(document.as_bytes()).unwrap();
        pem("RSA PRIVATE KEY", info.private_key)
    }

    /// A well-formed PKCS#1 structure with a made-up odd modulus of `bits` bits.
    /// It is not a working key pair; derivation only reads `n` and `e`.
    fn synthetic_rsa_pem(bits: usize, fill: u8) -> String {
        let mut modulus = vec![fill; bits / 8];
        modulus[0] |= 0x80;
        *modulus.last_mut().unwrap() |= 0x01;
        let exponent = [0x01, 0x00, 0x01];
        let one = [0x01];
        let key = pkcs1::RsaPrivateKey {
            modulus: UintRef::new(&modulus).unwrap(),
            public_exponent: UintRef::new(&exponent).unwrap(),
            private_exponent: UintRef::new(&one).unwrap(),
            prime1: UintRef::new(&one).unwrap(),
            prime2: UintRef::new(&one).unwrap(),
            exponent1: UintRef::new(&one).unwrap(),
            exponent2: UintRef::new(&one).unwrap(),
            coefficient: UintRef::new(&one).unwrap(),
            other_prime_infos: None,
        };
        pem("RSA PRIVATE KEY", &key.to_der().unwrap())
    }

    /// A PKCS#8 wrapper naming the EC algorithm, around placeholder bytes.
    fn ec_pkcs8_pem() -> String {
        let info = pkcs8::PrivateKeyInfo::new(
            AlgorithmIdentifierRef {
                oid: ObjectIdentifier::new_unwrap("1.2.840.10045.2.1"),
                parameters: None,
            },
            &[0x30, 0x00],
        );
        pem("PRIVATE KEY", &info.to_der().unwrap())
    }

    fn rsa_params(jwk: &Jwk) -> &RSAKeyParameters {
        match &jwk.algorithm {
            AlgorithmParameters::RSA(params) => params,
            other => panic!("expected an RSA JWK, got {other:?}"),
        }
    }

    fn unexpiring_rs256() -> Validation {
        let mut validation = Validation::new(Algorithm::RS256);
        validation.required_spec_claims.clear();
        validation.validate_exp = false;
        validation
    }

    #[test]
    fn derived_public_key_matches_the_committed_fixture_jwks() {
        let derived = public_jwk_from_private_pem(FIXTURE_PKCS8).unwrap();
        let fixture: JwkSet = serde_json::from_str(FIXTURE_JWKS).unwrap();
        assert_eq!(rsa_params(&derived).n, rsa_params(&fixture.keys[0]).n);
        assert_eq!(rsa_params(&derived).e, rsa_params(&fixture.keys[0]).e);
        assert_eq!(derived.common.key_algorithm, Some(KeyAlgorithm::RS256));
        assert_eq!(derived.common.public_key_use, Some(PublicKeyUse::Signature));
    }

    #[test]
    fn pkcs1_and_pkcs8_encodings_of_one_key_derive_the_same_jwk() {
        assert_eq!(
            public_jwk_from_private_pem(FIXTURE_PKCS8).unwrap(),
            public_jwk_from_private_pem(&fixture_pkcs1_pem()).unwrap()
        );
    }

    #[test]
    fn thumbprint_matches_the_rfc_7638_example() {
        let n = "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw";
        assert_eq!(
            rsa_thumbprint(n, "AQAB"),
            "NzbLsXh8uDCcd-6MNwXF4W_7noWXFZAfHkxZsRGC9Xs"
        );
    }

    #[test]
    fn kid_is_the_thumbprint_of_the_derived_key() {
        let derived = public_jwk_from_private_pem(FIXTURE_PKCS8).unwrap();
        let params = rsa_params(&derived);
        assert_eq!(
            derived.common.key_id.as_deref(),
            Some(rsa_thumbprint(&params.n, &params.e).as_str())
        );
    }

    #[test]
    fn a_synthetic_2048_bit_key_is_accepted() {
        // Guards the builder below: the rejections must come from key size, not
        // from a malformed structure.
        assert!(public_jwk_from_private_pem(&synthetic_rsa_pem(2048, 0x5a)).is_ok());
    }

    #[test]
    fn small_non_rsa_and_malformed_keys_are_rejected() {
        let small_pem = synthetic_rsa_pem(1024, 0x5a);
        let small = public_jwk_from_private_pem(&small_pem).unwrap_err();
        assert!(small.contains("1024 bits"), "{small}");
        let sec1 = public_jwk_from_private_pem(&pem("EC PRIVATE KEY", &[0x30, 0x00])).unwrap_err();
        assert!(sec1.contains("EC PRIVATE KEY"), "{sec1}");
        let pkcs8_ec = public_jwk_from_private_pem(&ec_pkcs8_pem()).unwrap_err();
        assert!(pkcs8_ec.contains("require an RSA key"), "{pkcs8_ec}");
        assert!(public_jwk_from_private_pem("not a pem").is_err());
        assert!(GatewaySigningKeys::derive(&small_pem, None).is_err());
    }

    #[test]
    fn the_previous_key_is_published_but_never_signs() {
        let previous_pem = synthetic_rsa_pem(2048, 0xa5);
        let keys = GatewaySigningKeys::derive(FIXTURE_PKCS8, Some(&previous_pem)).unwrap();
        assert_eq!(keys.public_jwks.keys.len(), 2);
        assert_eq!(
            keys.public_jwks.keys[0].common.key_id.as_deref(),
            Some(keys.active_kid.as_str())
        );
        assert_ne!(
            keys.public_jwks.keys[1].common.key_id.as_deref(),
            Some(keys.active_kid.as_str())
        );

        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(keys.active_kid.clone());
        let token = encode(
            &header,
            &serde_json::json!({ "sub": "x" }),
            &keys.signing_key,
        )
        .unwrap();
        let active = DecodingKey::from_jwk(&keys.public_jwks.keys[0]).unwrap();
        let previous = DecodingKey::from_jwk(&keys.public_jwks.keys[1]).unwrap();
        assert!(decode::<serde_json::Value>(&token, &active, &unexpiring_rs256()).is_ok());
        assert!(decode::<serde_json::Value>(&token, &previous, &unexpiring_rs256()).is_err());
    }

    #[test]
    fn a_single_key_ring_publishes_only_the_active_key() {
        let keys = GatewaySigningKeys::derive(FIXTURE_PKCS8, None).unwrap();
        assert_eq!(keys.public_jwks.keys.len(), 1);
    }

    #[test]
    fn a_previous_key_identical_to_the_active_key_is_rejected() {
        let error = GatewaySigningKeys::derive(FIXTURE_PKCS8, Some(&fixture_pkcs1_pem()))
            .err()
            .expect("identical keys must be rejected");
        assert!(error.contains("same as the active key"), "{error}");
    }
}
