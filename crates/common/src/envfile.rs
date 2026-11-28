//! A function's env vars when they don't fit in Lambda's environment, which holds 4 KB in all
//! (Vercel allows 64 KB a deployment).
//!
//! Publishing seals the project's variables into one file, encrypted with a key made for that
//! deployment (AES-256-GCM), and ships it as a Lambda layer, which Lambda puts at `FILE`. The key
//! goes into the function's environment as `KEY_VAR`, encrypted with the install's own KMS key like
//! every other env var there: reading it takes `kms:Decrypt` on that key, and the file alone is
//! noise. The layer version is deleted as soon as the function version that uses it is published
//! (Lambda keeps a deleted version's content for the functions already using it), so nothing can
//! fetch the file again. The runtime opens the file when its environment starts and hands the
//! variables to the app.
//!
//! The file: `MAGIC`, a 12-byte nonce, then the sealed JSON object of name → value (the AEAD's
//! associated data is `MAGIC`).

use std::collections::BTreeMap;

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};

/// The function's env var holding the file's key, as 64 hex digits.
pub const KEY_VAR: &str = "FORKRAIL_ENV_KEY";
/// Where the runtime finds the file: Lambda unpacks a layer into /opt.
pub const FILE: &str = "/opt/forkrail/env";
/// The file's path inside the layer's zip.
pub const LAYER_ENTRY: &str = "forkrail/env";
/// A project's variables may take this much in all, counting names and values: Vercel's limit.
pub const MAX_BYTES: usize = 64 * 1024;
/// What every file starts with; a newer format would change it.
pub const MAGIC: &[u8; 8] = b"FRKENV1\n";
/// Written into every runtime binary that opens these files: packaging looks for it, so a sealed
/// file never goes to a runtime that would start the app without its variables.
pub const RUNTIME_MARKER: &str = "forkrail-runtime opens sealed env files: FRKENV1";

/// Written into every runtime binary that gives the app the project's own AWS credentials apart
/// from the function role's (dropping the role's session token when the project sets its own keys
/// without one): only then may a sealed file carry `AWS_ACCESS_KEY_ID` and the like.
pub const RUNTIME_AWS_KEYS_MARKER: &str = "forkrail-runtime hands the app its own AWS keys: 1";

/// Whether a runtime binary's bytes say it opens sealed env files.
pub fn runtime_opens(binary: &[u8]) -> bool {
    runtime_has(binary, RUNTIME_MARKER)
}

/// Whether a runtime binary's bytes carry `marker`.
pub fn runtime_has(binary: &[u8], marker: &str) -> bool {
    let marker = marker.as_bytes();
    binary.windows(marker.len()).any(|w| w == marker)
}

/// The layer a project's sealed files are published as, one version a deployment.
pub fn layer_name(project: &str) -> String {
    format!("forkrail-env-{project}")
}

/// How much `vars` take, as `MAX_BYTES` counts it.
pub fn size(vars: &BTreeMap<String, String>) -> usize {
    vars.iter().map(|(k, v)| k.len() + v.len()).sum()
}

/// Seal `vars` with a new key: the key as hex, and the file.
pub fn seal(vars: &BTreeMap<String, String>) -> Result<(String, Vec<u8>), String> {
    let rng = SystemRandom::new();
    let mut key = [0u8; 32];
    let mut nonce = [0u8; NONCE_LEN];
    rng.fill(&mut key).map_err(|_| "no randomness for the env file's key".to_owned())?;
    rng.fill(&mut nonce).map_err(|_| "no randomness for the env file's nonce".to_owned())?;
    let sealer = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).map_err(|_| "bad key length".to_owned())?);
    let mut body = serde_json::to_vec(vars).map_err(|e| e.to_string())?;
    sealer.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(MAGIC), &mut body).map_err(|_| "sealing the env file failed".to_owned())?;
    let mut file = Vec::with_capacity(MAGIC.len() + NONCE_LEN + body.len());
    file.extend_from_slice(MAGIC);
    file.extend_from_slice(&nonce);
    file.extend_from_slice(&body);
    Ok((hex(&key), file))
}

/// The variables in `file`, opened with `key` (hex).
pub fn open(key: &str, file: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let key = unhex(key).filter(|k| k.len() == 32).ok_or_else(|| format!("{KEY_VAR} isn't a key"))?;
    let rest = file.strip_prefix(MAGIC.as_slice()).ok_or("not a forkrail env file, or a newer one than this runtime reads")?;
    if rest.len() < NONCE_LEN {
        return Err("the env file is cut short".to_owned());
    }
    let (nonce, sealed) = rest.split_at(NONCE_LEN);
    let opener = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).map_err(|_| "bad key length".to_owned())?);
    let mut body = sealed.to_vec();
    let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| "bad nonce".to_owned())?;
    let plain = opener.open_in_place(nonce, Aad::from(MAGIC), &mut body).map_err(|_| "the env file doesn't open with this deployment's key".to_owned())?;
    serde_json::from_slice(plain).map_err(|e| format!("the env file's contents: {e}"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_file_opens_with_its_key_only() {
        let vars: BTreeMap<String, String> = [("PEM".to_owned(), "-----BEGIN-----\nabc\n-----END-----".to_owned()), ("BIG".to_owned(), "x".repeat(60_000))].into();
        let (key, file) = seal(&vars).unwrap();
        assert_eq!(key.len(), 64);
        assert!(file.starts_with(MAGIC));
        assert_eq!(open(&key, &file).unwrap(), vars);
        // Nothing of a value shows in the file.
        assert!(!file.windows(12).any(|w| w == b"-----BEGIN--"));
        // Another deployment's key, a changed byte, a cut file, an older or newer format: refused.
        let (other, _) = seal(&vars).unwrap();
        assert!(open(&other, &file).is_err());
        let mut changed = file.clone();
        *changed.last_mut().unwrap() ^= 1;
        assert!(open(&key, &changed).is_err());
        assert!(open(&key, &file[..10]).is_err());
        assert!(open(&key, &[b"FRKENV2\n".as_slice(), &file[8..]].concat()).is_err());
        assert!(open("zz", &file).is_err() && open(&key[..62], &file).is_err());
        // Two seals of the same variables differ (a new key and nonce each time).
        assert_ne!(seal(&vars).unwrap().1, file);
        assert_eq!(size(&vars), 3 + vars["PEM"].len() + 3 + 60_000);
    }
}
