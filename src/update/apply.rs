use super::verify::{expected_hash, sha256_hex, verify_signature};
use super::{current_asset_name, UpdateError, UpdateInfo};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};

/// Extract the bot binary from a downloaded release archive held in memory.
/// Windows assets are `.zip`; Linux/arm are `.tar.gz`. Returns the raw binary
/// bytes. `binary_name` is the file to pull out of the archive.
fn extract_binary(archive: &[u8], is_zip: bool, binary_name: &str) -> Result<Vec<u8>, UpdateError> {
    if is_zip {
        let reader = std::io::Cursor::new(archive);
        let mut zip =
            zip::ZipArchive::new(reader).map_err(|e| UpdateError::Extract(e.to_string()))?;
        let mut file = zip
            .by_name(binary_name)
            .map_err(|e| UpdateError::Extract(format!("{binary_name}: {e}")))?;
        let mut out = Vec::new();
        file.read_to_end(&mut out)
            .map_err(|e| UpdateError::Extract(e.to_string()))?;
        Ok(out)
    } else {
        let gz = flate2::read::GzDecoder::new(archive);
        let mut tar = tar::Archive::new(gz);
        for entry in tar
            .entries()
            .map_err(|e| UpdateError::Extract(e.to_string()))?
        {
            let mut entry = entry.map_err(|e| UpdateError::Extract(e.to_string()))?;
            let path = entry
                .path()
                .map_err(|e| UpdateError::Extract(e.to_string()))?;
            if path.file_name().and_then(|n| n.to_str()) == Some(binary_name) {
                let mut out = Vec::new();
                entry
                    .read_to_end(&mut out)
                    .map_err(|e| UpdateError::Extract(e.to_string()))?;
                return Ok(out);
            }
        }
        Err(UpdateError::Extract(format!(
            "{binary_name} not found in archive"
        )))
    }
}

/// The binary name inside the archive (no directory prefix — CI archives the
/// bare binary from the working dir).
fn binary_name() -> &'static str {
    if cfg!(windows) {
        "tt-spotify-bot.exe"
    } else {
        "tt-spotify-bot"
    }
}

async fn get_bytes(
    client: &reqwest::Client,
    url: &str,
    progress: Option<&(dyn Fn(u64, Option<u64>) + Sync)>,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, UpdateError> {
    use futures_util::StreamExt;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| UpdateError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(UpdateError::Http(format!("HTTP {}", resp.status())));
    }
    let total = resp.content_length();
    let mut out = Vec::with_capacity(total.unwrap_or(0) as usize);
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if cancel.load(Ordering::Relaxed) {
            return Err(UpdateError::Cancelled);
        }
        let chunk = chunk.map_err(|e| UpdateError::Http(e.to_string()))?;
        out.extend_from_slice(&chunk);
        if let Some(cb) = progress {
            cb(out.len() as u64, total);
        }
    }
    Ok(out)
}

/// Download SHA256SUMS + its signature + the platform asset, verify the
/// signature and hash, extract the binary, and replace the running executable.
/// Nothing is written to the binary unless BOTH signature and hash pass.
pub async fn download_and_apply(
    info: &UpdateInfo,
    progress: &(dyn Fn(u64, Option<u64>) + Sync),
    cancel: &AtomicBool,
) -> Result<(), UpdateError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .user_agent(concat!("ttspotify-rs/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| UpdateError::Http(e.to_string()))?;

    // 1. SHA256SUMS + signature (small; no progress).
    let sums = get_bytes(&client, &info.sums_url, None, cancel).await?;
    let sig = get_bytes(&client, &info.sig_url, None, cancel).await?;
    let sig_str = String::from_utf8(sig).map_err(|_| UpdateError::Signature)?;

    // 2. Verify signature over the SUMS bytes. Abort before touching anything.
    verify_signature(&sums, &sig_str)?;

    // 3. Download the asset with progress.
    let asset = get_bytes(&client, &info.asset_url, Some(progress), cancel).await?;

    // 4. Hash-check the asset against the (now-trusted) SUMS.
    let sums_text = String::from_utf8_lossy(&sums);
    let want = expected_hash(&sums_text, current_asset_name()).ok_or(UpdateError::Hash)?;
    if sha256_hex(&asset) != want {
        return Err(UpdateError::Hash);
    }

    // 5. Extract the binary.
    let is_zip = cfg!(windows);
    let bin = extract_binary(&asset, is_zip, binary_name())?;

    // 6. Write to a temp file next to the current exe, then self-replace.
    let exe = std::env::current_exe().map_err(|e| UpdateError::Io(e.to_string()))?;
    let dir = exe
        .parent()
        .ok_or_else(|| UpdateError::Io("no exe dir".into()))?;
    let tmp = dir.join("tt-spotify-bot.update.tmp");
    std::fs::write(&tmp, &bin).map_err(|e| UpdateError::Io(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        let _ = std::fs::set_permissions(&tmp, perms);
    }
    // Remove the temp file whether or not the replace succeeded — an early
    // `?` here used to strand tt-spotify-bot.update.tmp next to the exe when
    // the swap failed (locked file, AV, permissions).
    let replaced = self_replace::self_replace(&tmp).map_err(|e| UpdateError::Io(e.to_string()));
    let _ = std::fs::remove_file(&tmp);
    replaced?;

    // Post-update: stamp any newly-added config keys into every config on disk,
    // so they are present without having to run each bot. Best-effort; a top-up
    // problem must never fail an otherwise-successful update.
    crate::config::top_up_configs();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn extract_from_tar_gz() {
        // Build a tar.gz in memory containing "tt-spotify-bot" -> b"BINARY".
        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            let data = b"BINARY";
            let mut header = tar::Header::new_gnu();
            header.set_path("tt-spotify-bot").unwrap();
            header.set_size(data.len() as u64);
            header.set_cksum();
            builder.append(&header, &data[..]).unwrap();
            builder.finish().unwrap();
        }
        let mut gz = Vec::new();
        {
            let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
            enc.write_all(&tar_buf).unwrap();
            enc.finish().unwrap();
        }
        let out = extract_binary(&gz, false, "tt-spotify-bot").unwrap();
        assert_eq!(out, b"BINARY");
    }

    #[test]
    fn extract_from_zip() {
        let mut buf = Vec::new();
        {
            let w = std::io::Cursor::new(&mut buf);
            let mut zip = zip::ZipWriter::new(w);
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            zip.start_file("tt-spotify-bot.exe", opts).unwrap();
            zip.write_all(b"WINBIN").unwrap();
            zip.finish().unwrap();
        }
        let out = extract_binary(&buf, true, "tt-spotify-bot.exe").unwrap();
        assert_eq!(out, b"WINBIN");
    }

    #[test]
    fn extract_missing_binary_errors() {
        let mut buf = Vec::new();
        {
            let w = std::io::Cursor::new(&mut buf);
            let mut zip = zip::ZipWriter::new(w);
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            zip.start_file("something-else", opts).unwrap();
            zip.write_all(b"x").unwrap();
            zip.finish().unwrap();
        }
        assert!(matches!(
            extract_binary(&buf, true, "tt-spotify-bot.exe"),
            Err(UpdateError::Extract(_))
        ));
    }
}
