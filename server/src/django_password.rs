//! Verification of Django password hashes, for the one-time play account
//! claim flow. Play stored passwords with Django's default PBKDF2-SHA256
//! hasher: `pbkdf2_sha256$<iterations>$<salt>$<base64 digest>`.
//!
//! The hash is verified once during claim and never becomes a login
//! credential in arena.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::Sha256;
use subtle::ConstantTimeEq as _;

/// Upper bound on accepted iteration counts. Django has never shipped a
/// default above ~1M; anything larger in a hash string is treated as
/// malformed rather than an invitation to burn CPU.
const MAX_ITERATIONS: u32 = 5_000_000;

/// Check `password` against a Django `pbkdf2_sha256` hash string.
///
/// Returns false for wrong passwords AND for malformed/unsupported hash
/// strings (including Django's unusable-password sentinel `!...` and empty
/// strings) — callers only care whether the claim is proven.
pub fn verify(password: &str, encoded: &str) -> bool {
    let mut parts = encoded.split('$');
    let (Some(algorithm), Some(iterations), Some(salt), Some(digest_b64), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };

    if algorithm != "pbkdf2_sha256" || salt.is_empty() {
        return false;
    }

    let Ok(iterations) = iterations.parse::<u32>() else {
        return false;
    };
    if iterations == 0 || iterations > MAX_ITERATIONS {
        return false;
    }

    let Ok(expected) = BASE64.decode(digest_b64) else {
        return false;
    };
    if expected.len() != 32 {
        return false;
    }

    let mut computed = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(
        password.as_bytes(),
        salt.as_bytes(),
        iterations,
        &mut computed,
    );

    computed.ct_eq(expected.as_slice()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Generated with Python hashlib.pbkdf2_hmac, matching Django's encoding.
    const HASH: &str =
        "pbkdf2_sha256$390000$q0jZwGqpQaJk$H/DnI8TCws/bdaVWHuzPNyr2B/+7L4OeqKliafbj8N0=";
    const HASH_2: &str =
        "pbkdf2_sha256$260000$saltysalt$fdr4GEVFxx0kLHYGvrnFQUyTekgaAA8DbWRR6Z+A7/A=";

    #[test]
    fn correct_password_verifies() {
        assert!(verify("correct-horse-battery", HASH));
        assert!(verify("hunter2", HASH_2));
    }

    #[test]
    fn wrong_password_fails() {
        assert!(!verify("wrong-password", HASH));
        assert!(!verify("", HASH));
        assert!(!verify("correct-horse-battery ", HASH));
    }

    #[test]
    fn malformed_hashes_fail_closed() {
        assert!(!verify("anything", ""));
        // Django's unusable-password sentinel
        assert!(!verify(
            "anything",
            "!aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789012"
        ));
        // Unsupported algorithms
        assert!(!verify(
            "anything",
            "pbkdf2_sha1$260000$saltysalt$fdr4GEVFxx0kLHYGvrnFQUyTekgaAA8DbWRR6Z+A7/A="
        ));
        assert!(!verify("anything", "md5$salt$abcdef"));
        // Structural garbage
        assert!(!verify(
            "anything",
            "pbkdf2_sha256$notanumber$salt$aGVsbG8="
        ));
        assert!(!verify("anything", "pbkdf2_sha256$260000$$aGVsbG8="));
        assert!(!verify(
            "anything",
            "pbkdf2_sha256$260000$salt$not-base64!!"
        ));
        assert!(!verify(
            "anything",
            "pbkdf2_sha256$260000$salt$aGVsbG8=$extra"
        ));
        // Digest wrong length (valid base64, not 32 bytes)
        assert!(!verify("anything", "pbkdf2_sha256$260000$salt$aGVsbG8="));
        // Absurd iteration count is malformed, not a DoS vector
        assert!(!verify(
            "anything",
            "pbkdf2_sha256$4000000000$salt$fdr4GEVFxx0kLHYGvrnFQUyTekgaAA8DbWRR6Z+A7/A="
        ));
    }
}
