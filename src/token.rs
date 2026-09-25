//! Random secrets (session IDs, embed tokens).

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
