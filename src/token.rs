//! Random secrets (session IDs, embed tokens) and comparing them safely.

use std::fmt::Write as _;

/// `n_bytes` from the OS CSPRNG, hex-encoded (`2 * n_bytes` chars).
pub fn random_hex(n_bytes: usize) -> Result<String, getrandom::Error> {
    let mut bytes = vec![0u8; n_bytes];
    getrandom::fill(&mut bytes)?;
    Ok(bytes
        .iter()
        .fold(String::with_capacity(n_bytes * 2), |mut hex, b| {
            // Writing into a String can't fail.
            let _ = write!(hex, "{b:02x}");
            hex
        }))
}

/// Constant-time comparison, so response timing doesn't leak how much of a guessed secret
/// was right. An empty `expected` never matches.
#[must_use]
pub fn secret_matches(expected: &str, candidate: &str) -> bool {
    let (expected, candidate) = (expected.as_bytes(), candidate.as_bytes());
    !expected.is_empty()
        && expected.len() == candidate.len()
        && expected
            .iter()
            .zip(candidate)
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}
