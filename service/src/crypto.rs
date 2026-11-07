//! Random secrets, hashes and the HMAC that signs cookies.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// `N` bytes from the operating system's CSPRNG.
pub fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("the operating system's random source failed");
    bytes
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

pub fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn unb64(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).ok()
}

fn mac(key: &[u8; 32], purpose: &str, payload: &[u8]) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(purpose.as_bytes());
    mac.update(&[0]);
    mac.update(payload);
    mac
}

/// `payload` signed for `purpose`: `<b64 payload>.<b64 HMAC-SHA256>`.
pub fn sign(key: &[u8; 32], purpose: &str, payload: &[u8]) -> String {
    let tag = mac(key, purpose, payload).finalize().into_bytes();
    format!("{}.{}", b64(payload), b64(&tag))
}

/// The payload of a [`sign`]ed value, or `None` if it was not signed by
/// `key` for `purpose` (checked in constant time).
pub fn verify(key: &[u8; 32], purpose: &str, signed: &str) -> Option<Vec<u8>> {
    let (payload, tag) = signed.split_once('.')?;
    let payload = unb64(payload)?;
    let tag = unb64(tag)?;
    mac(key, purpose, &payload).verify_slice(&tag).ok()?;
    Some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_bind_key_purpose_and_payload() {
        let key = [7u8; 32];
        let signed = sign(&key, "session", b"abc");
        assert_eq!(
            verify(&key, "session", &signed).as_deref(),
            Some(&b"abc"[..])
        );
        assert_eq!(verify(&[8u8; 32], "session", &signed), None);
        assert_eq!(verify(&key, "oauth", &signed), None);
        let forged = format!("{}.{}", b64(b"abd"), signed.split_once('.').unwrap().1);
        assert_eq!(verify(&key, "session", &forged), None);
        assert_eq!(verify(&key, "session", "garbage"), None);
    }
}
