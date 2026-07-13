use super::PUBLIC_KEY;
use super::UpdateError;
use minisign_verify::{PublicKey, Signature};
use sha2::{Digest, Sha256};

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Look up the expected hex hash for `asset` in a SHA256SUMS body.
/// Lines look like: `<hex>  <filename>` (two spaces, `sha256sum` format).
pub fn expected_hash<'a>(sums: &'a str, asset: &str) -> Option<&'a str> {
    for line in sums.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (hash, name) = line.split_once("  ")?;
        if name.trim() == asset {
            return Some(hash.trim());
        }
    }
    None
}

/// Verify a minisign signature (`.minisig` file contents) over `signed_data`
/// using the embedded public key.
pub fn verify_signature(signed_data: &[u8], sig_body: &str) -> Result<(), UpdateError> {
    let pk = PublicKey::from_base64(PUBLIC_KEY).map_err(|_| UpdateError::Signature)?;
    let sig = Signature::decode(sig_body).map_err(|_| UpdateError::Signature)?;
    pk.verify(signed_data, &sig, false)
        .map_err(|_| UpdateError::Signature)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_of_empty_is_known() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_of_abc() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn expected_hash_finds_asset() {
        let sums =
            "aaaa  tt-spotify-bot-linux-x86_64.tar.gz\nbbbb  tt-spotify-bot-windows-x86_64.zip\n";
        assert_eq!(
            expected_hash(sums, "tt-spotify-bot-windows-x86_64.zip"),
            Some("bbbb")
        );
    }

    #[test]
    fn expected_hash_missing_asset_is_none() {
        let sums = "aaaa  other.tar.gz\n";
        assert_eq!(expected_hash(sums, "tt-spotify-bot-windows-x86_64.zip"), None);
    }

    // Real minisign signature over the bytes b"hello\n", made with the project
    // secret key (public key = super::PUBLIC_KEY). Regenerate with:
    //   printf 'hello\n' > m && minisign -S -s minisign.key -m m && cat m.minisig
    const SIG_HELLO: &str = "untrusted comment: signature from minisign secret key\nRUTvwlFryO9VLtlXE3U+06tIieFzGC5dVf9j7pPIn3780QI2aAnSKuuqaxznVtxYmyftqhXYzfDk1UfRLxoyGyYFarm+xAIN5wk=\ntrusted comment: timestamp:1783802135\tfile:C:/Users/aloys/Documents/aloy/projects/python/spotifyRust/scratch_m\thashed\nN9/Si2bqNOpabMmF5rCSZmxiB6TuVNGB0yXq31SnXRGapa/0roymZAUGXP+0ZFFQB50YvNr43MJHbUAF8E78Dw==\n";

    #[test]
    fn valid_signature_passes() {
        assert!(verify_signature(b"hello\n", SIG_HELLO).is_ok());
    }

    #[test]
    fn tampered_data_fails() {
        assert!(matches!(
            verify_signature(b"HELLO\n", SIG_HELLO),
            Err(UpdateError::Signature)
        ));
    }
}
