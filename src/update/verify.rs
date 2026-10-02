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
        // Skip lines that don't match the `<hex>  <name>` shape (comments,
        // `sha256sum -b`'s `<hex> *<name>`) instead of aborting the whole
        // lookup: the `?` here failed the update on the first odd line even
        // when the wanted entry sat right below it. Fails closed either way -
        // no match means the update is refused, never wrongly accepted.
        let Some((hash, name)) = line.split_once("  ") else {
            continue;
        };
        if name.trim() == asset {
            return Some(hash.trim());
        }
    }
    None
}

/// Verify a minisign signature (`.minisig` file contents) over `signed_data`
/// using the embedded public key, and that it was made for release `tag`.
///
/// The tag comes from GitHub and the signature covers only the checksums, so
/// without the version check an old signed release published under a new tag
/// would be installed as an update. CI signs with the trusted comment
/// `version:<tag>`, and minisign signs that comment too.
pub fn verify_signature(signed_data: &[u8], sig_body: &str, tag: &str) -> Result<(), UpdateError> {
    verify_with_key(PUBLIC_KEY, signed_data, sig_body, tag)
}

fn verify_with_key(
    public_key: &str,
    signed_data: &[u8],
    sig_body: &str,
    tag: &str,
) -> Result<(), UpdateError> {
    let pk = PublicKey::from_base64(public_key).map_err(|_| UpdateError::Signature)?;
    let sig = Signature::decode(sig_body).map_err(|_| UpdateError::Signature)?;
    pk.verify(signed_data, &sig, false)
        .map_err(|_| UpdateError::Signature)?;
    if signed_version(sig.trusted_comment()) == Some(tag.trim_start_matches('v')) {
        Ok(())
    } else {
        Err(UpdateError::SignedVersion)
    }
}

/// The version in a trusted comment like `version:v1.3.0`, without its `v`.
fn signed_version(comment: &str) -> Option<&str> {
    comment
        .split_whitespace()
        .find_map(|field| field.strip_prefix("version:"))
        .map(|v| v.trim_start_matches('v'))
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

    #[test]
    fn expected_hash_survives_malformed_lines_before_the_match() {
        // A comment, a binary-mode `hash *name` line and a tab-separated line
        // used to abort the whole lookup via `?`, failing the update even
        // though the wanted entry was right there.
        let sums = "# release manifest\n\
                    bbbb *binary-mode.zip\n\
                    cccc\tone-tab.zip\n\
                    aaaa  tt-spotify-bot-windows-x86_64.zip\n";
        assert_eq!(
            expected_hash(sums, "tt-spotify-bot-windows-x86_64.zip"),
            Some("aaaa")
        );
    }

    // Real minisign signature over the bytes b"hello\n", made with the project
    // secret key (public key = super::PUBLIC_KEY). Regenerate with:
    //   printf 'hello\n' > m && minisign -S -s minisign.key -m m && cat m.minisig
    const SIG_HELLO: &str = "untrusted comment: signature from minisign secret key\nRUTvwlFryO9VLtlXE3U+06tIieFzGC5dVf9j7pPIn3780QI2aAnSKuuqaxznVtxYmyftqhXYzfDk1UfRLxoyGyYFarm+xAIN5wk=\ntrusted comment: timestamp:1783802135\tfile:C:/Users/aloys/Documents/aloy/projects/python/spotifyRust/scratch_m\thashed\nN9/Si2bqNOpabMmF5rCSZmxiB6TuVNGB0yXq31SnXRGapa/0roymZAUGXP+0ZFFQB50YvNr43MJHbUAF8E78Dw==\n";

    #[test]
    fn the_release_key_verifies_but_a_signature_without_a_version_is_refused() {
        // Signature, not SignedVersion, would mean the release key itself failed.
        assert!(matches!(
            verify_signature(b"hello\n", SIG_HELLO, "v1.2.0"),
            Err(UpdateError::SignedVersion)
        ));
    }

    #[test]
    fn tampered_data_fails() {
        assert!(matches!(
            verify_signature(b"HELLO\n", SIG_HELLO, "v1.2.0"),
            Err(UpdateError::Signature)
        ));
    }

    // A throwaway key, so tests never need the release key. Made with:
    //   minisign -G -W -p test.pub -s test.key
    //   printf 'hello\n' > m && minisign -S -s test.key -m m -t "version:v9.1.0"
    const TEST_KEY: &str = "RWQl6IzQUmuUu+EfJ3VI/m91lMoXD201bufX6dmmxNkp7qHrgfxDMT6j";
    const SIG_V910: &str = "untrusted comment: signature from minisign secret key\nRUQl6IzQUmuUu5MPZ+y78C07VeCh68qUKs8JBlkCRY7gM2Q0e31JvjZQwYGrkDUXfkMzUmGBpEOxh6xP6GJknQp0LW3oyB6zZAc=\ntrusted comment: version:v9.1.0\nB2ta8BSxrN0NKCD46gdEEr9G0mqAbDtKe4DWS6Qm9IwyLVYPS44ItZONJFLvQ89CM8R2MrUS7Zmhw9NEaWVkDw==\n";

    #[test]
    fn a_signature_for_the_tagged_version_passes() {
        assert!(verify_with_key(TEST_KEY, b"hello\n", SIG_V910, "v9.1.0").is_ok());
        assert!(verify_with_key(TEST_KEY, b"hello\n", SIG_V910, "9.1.0").is_ok());
    }

    #[test]
    fn an_old_release_under_a_new_tag_is_refused() {
        assert!(matches!(
            verify_with_key(TEST_KEY, b"hello\n", SIG_V910, "v9.2.0"),
            Err(UpdateError::SignedVersion)
        ));
    }

    #[test]
    fn an_edited_version_breaks_the_signature() {
        let forged = SIG_V910.replace("version:v9.1.0", "version:v9.2.0");
        assert!(matches!(
            verify_with_key(TEST_KEY, b"hello\n", &forged, "v9.2.0"),
            Err(UpdateError::Signature)
        ));
    }

    #[test]
    fn the_version_is_read_from_the_trusted_comment() {
        assert_eq!(signed_version("version:v1.3.0"), Some("1.3.0"));
        assert_eq!(signed_version("timestamp:1\tversion:1.3.0"), Some("1.3.0"));
        assert_eq!(signed_version("timestamp:1790798489\tfile:n\thashed"), None);
    }
}
