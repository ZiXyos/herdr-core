//! Key pairs created on the device. Generation, OpenSSH encoding and public-key derivation all
//! happen here (ssh-key over the OS random source), so no key bytes are produced outside the engine.

use getrandom::SysRng;
use getrandom::rand_core::UnwrapErr;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey};
use serde_json::{Value, json};
use zeroize::Zeroizing;

use crate::EngineError;

/// Comments end up in `authorized_keys`: one short token, no whitespace, quotes or shell syntax.
fn checked_comment(comment: &str) -> Result<&str, EngineError> {
    let valid = comment.len() <= 64 && comment.bytes().all(|b| b.is_ascii_alphanumeric() || b"@._-".contains(&b));
    if valid {
        Ok(comment)
    } else {
        Err(EngineError::new(
            "invalid_params",
            "Key comment is not a plain token",
            "use up to 64 of a-z A-Z 0-9 @ . _ -",
        ))
    }
}

fn unusable() -> EngineError {
    EngineError::new(
        "invalid_private_key",
        "The private key could not be parsed",
        "use an unencrypted OpenSSH private key; encrypted keys are not supported yet",
    )
}

fn failed() -> EngineError {
    EngineError::new("internal", "The key pair could not be created", "retry")
}

/// Creates an unencrypted Ed25519 key pair: `{privateKey (OpenSSH), publicKey (authorized_keys line)}`.
/// The seed comes straight from the operating system; nothing is cached or logged.
pub(crate) fn generate(comment: &str) -> Result<Value, EngineError> {
    let comment = checked_comment(comment)?;
    let mut key = PrivateKey::random(&mut UnwrapErr(SysRng), Algorithm::Ed25519).map_err(|_| failed())?;
    key.set_comment(comment);
    let private_key = key.to_openssh(LineEnding::LF).map_err(|_| failed())?;
    let public_key = key.public_key().to_openssh().map_err(|_| failed())?;
    Ok(json!({ "privateKey": private_key.as_str(), "publicKey": public_key }))
}

/// The `authorized_keys` line for an OpenSSH private key, labelled with `comment`.
pub(crate) fn public_line(private_key: Zeroizing<String>, comment: &str) -> Result<Value, EngineError> {
    let comment = checked_comment(comment)?;
    let key = PrivateKey::from_openssh(private_key.as_str()).map_err(|_| unusable())?;
    let mut public = key.public_key().clone();
    public.set_comment(comment);
    Ok(json!({ "publicKey": public.to_openssh().map_err(|_| failed())? }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &Value, field: &str) -> String {
        value[field].as_str().unwrap().to_owned()
    }

    #[test]
    fn generated_keys_are_distinct_and_round_trip() {
        let first = generate("hydra-phone").unwrap();
        let second = generate("hydra-phone").unwrap();
        assert_ne!(text(&first, "privateKey"), text(&second, "privateKey"));

        let private_key = text(&first, "privateKey");
        assert!(private_key.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----\n"));
        let parsed = PrivateKey::from_openssh(&private_key).unwrap();
        assert_eq!(parsed.algorithm(), Algorithm::Ed25519);
        assert!(!parsed.is_encrypted());

        let public_key = text(&first, "publicKey");
        assert!(public_key.starts_with("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI"));
        assert!(public_key.ends_with(" hydra-phone"));
        assert_eq!(parsed.public_key().to_openssh().unwrap(), public_key);
    }

    #[test]
    fn public_line_matches_generation_and_relabels() {
        let pair = generate("hydra-phone").unwrap();
        let key = || Zeroizing::new(text(&pair, "privateKey"));
        assert_eq!(text(&public_line(key(), "hydra-phone").unwrap(), "publicKey"), text(&pair, "publicKey"));
        assert!(text(&public_line(key(), "other").unwrap(), "publicKey").ends_with(" other"));
        assert!(!text(&public_line(key(), "").unwrap(), "publicKey").contains("hydra"));
    }

    #[test]
    fn rejects_unsafe_comments_and_unusable_keys_without_echoing_them() {
        for comment in ["a b", "x'y", "a\nb", "$(id)", &"c".repeat(65)] {
            assert_eq!(generate(comment).unwrap_err().kind, "invalid_params");
        }
        let error = public_line(Zeroizing::new("not a key SECRET".into()), "c").unwrap_err();
        assert_eq!(error.kind, "invalid_private_key");
        assert!(!serde_json::to_string(&error).unwrap().contains("SECRET"));
    }
}
