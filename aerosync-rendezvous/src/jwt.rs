//! RS256 JWT issuance / verification (RFC-004 §8.2).

use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// JWT claims issued by the rendezvous server.
#[derive(Debug, Serialize, Deserialize)]
pub struct RendezvousClaims {
    pub iss: String,
    pub sub: String,
    pub iat: u64,
    pub exp: u64,
    pub scope: Vec<String>,
    pub name: String,
    /// Multitenant registry partition; empty = default (legacy single-tenant / open registration).
    #[serde(default)]
    pub ns: String,
}

pub fn issue_token(
    encoding_key: &EncodingKey,
    issuer: &str,
    peer_id: &str,
    name: &str,
    namespace: &str,
    ttl_secs: u64,
) -> anyhow::Result<(String, u64)> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let exp = now + ttl_secs;
    let claims = RendezvousClaims {
        iss: issuer.to_string(),
        sub: peer_id.to_string(),
        iat: now,
        exp,
        scope: vec![
            "lookup".to_string(),
            "session".to_string(),
            "relay".to_string(),
        ],
        name: name.to_string(),
        ns: namespace.to_string(),
    };
    let header = Header::new(Algorithm::RS256);
    let token = encode(&header, &claims, encoding_key)?;
    Ok((token, exp))
}

pub fn verify_bearer(
    token: &str,
    decoding_key: &DecodingKey,
    issuer: &str,
) -> anyhow::Result<RendezvousClaims> {
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[issuer]);
    let data = decode::<RendezvousClaims>(token, decoding_key, &validation)?;
    Ok(data.claims)
}

/// Load RSA PEM for signing (private key).
pub fn encoding_key_from_rsa_pem(pem: &[u8]) -> anyhow::Result<EncodingKey> {
    Ok(EncodingKey::from_rsa_pem(pem)?)
}

/// Load RSA public key PEM for verification (SPKI subjectPublicKeyInfo).
pub fn decoding_key_from_rsa_pem(pem: &[u8]) -> anyhow::Result<DecodingKey> {
    Ok(DecodingKey::from_rsa_pem(pem)?)
}

/// Derive verification key from a PKCS#8 **private** RSA PEM (same file as signing key).
pub fn decoding_key_from_private_pkcs8_pem(pem: &[u8]) -> anyhow::Result<DecodingKey> {
    use rsa::pkcs8::{DecodePrivateKey, EncodePublicKey, LineEnding};
    use rsa::{RsaPrivateKey, RsaPublicKey};
    let s = std::str::from_utf8(pem)?;
    let priv_k = RsaPrivateKey::from_pkcs8_pem(s)?;
    let pub_k = RsaPublicKey::from(&priv_k);
    let pkcs8_doc = pub_k.to_public_key_pem(LineEnding::LF)?;
    DecodingKey::from_rsa_pem(pkcs8_doc.as_bytes()).map_err(Into::into)
}
