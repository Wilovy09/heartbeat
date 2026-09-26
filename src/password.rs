//! Local admin password hashing (`AuthMode::Password`), argon2id in PHC string format.

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};

#[derive(Debug, thiserror::Error)]
pub enum HashError {
    #[error("could not generate a salt: {0}")]
    Salt(#[from] getrandom::Error),
    #[error("{0}")]
    Argon2(#[from] argon2::password_hash::Error),
}

/// PHC string (`$argon2id$v=19$...`) for `ADMIN_PASSWORD_HASH`.
pub fn hash(password: &str) -> Result<String, HashError> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt)?;
    let salt = SaltString::encode_b64(&salt)?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)?
        .to_string())
}

/// Whether `password` matches `hash`; a malformed hash never matches.
#[must_use]
pub fn verify(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|parsed| {
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify_round_trips() {
        let h = hash("correct horse").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify("correct horse", &h));
        assert!(!verify("wrong", &h));
        assert!(!verify("correct horse", "not-a-hash"));
    }
}
