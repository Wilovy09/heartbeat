//! Random secrets (session IDs, embed tokens).

/// `n_bytes` from the OS CSPRNG, hex-encoded (`2 * n_bytes` chars).
pub fn random_hex(n_bytes: usize) -> Result<String, getrandom::Error> {
    let mut bytes = vec![0u8; n_bytes];
    getrandom::fill(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
