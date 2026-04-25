use hmac::Hmac;
use hmac::Mac;
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Verify `sha256=<hex>` or raw 64-hex (case-insensitive) against HMAC-SHA256 of `body`.
pub fn verify_hmac_sha256(key: &[u8], body: &[u8], header_value: &str) -> bool {
    let s = header_value.trim();
    let their = s
        .strip_prefix("sha256=")
        .or_else(|| s.strip_prefix("SHA256="))
        .unwrap_or(s)
        .trim();
    if their.len() != 64 {
        return false;
    }
    let theirs = match hex::decode(their) {
        Ok(b) if b.len() == 32 => b,
        _ => return false,
    };
    let mut mac = match HmacSha256::new_from_slice(key) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    let out = mac.finalize().into_bytes();
    out.as_slice().ct_eq(theirs.as_slice()).unwrap_u8() == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_sha256_prefix() {
        let key = b"secret";
        let body = b"payload";
        let mut mac = HmacSha256::new_from_slice(key).unwrap();
        mac.update(body);
        let h = hex::encode(mac.finalize().into_bytes());
        assert!(verify_hmac_sha256(key, body, &format!("sha256={h}")));
        assert!(!verify_hmac_sha256(b"other", body, &format!("sha256={h}")));
    }
}
