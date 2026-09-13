//! Installing and updating the YouTube tools.
//!
//! No window here: this is what the progress dialog runs on its worker thread.

use crate::youtube::setup;

/// Download and install the YouTube tools. Reports progress via `progress`.
pub fn youtube_install(progress: &dyn Fn(&str)) -> Result<(), String> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("tokio runtime: {e}"))?;
    let paths = setup::resolve_paths().map_err(|e| e.to_string())?;
    if setup::is_installed(&paths) {
        progress("YouTube tools already installed.");
        return Ok(());
    }
    rt.block_on(setup::install(&paths, |l| progress(l)))
        .map_err(|e| e.to_string())
}

/// Refresh the sidecar script and the JavaScript runtime it needs.
pub fn youtube_update(progress: &dyn Fn(&str)) -> Result<(), String> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("tokio runtime: {e}"))?;
    let paths = setup::resolve_paths().map_err(|e| e.to_string())?;
    if !setup::is_installed(&paths) {
        return Err("YouTube tools aren't installed yet. Install them first.".to_string());
    }

    // The sidecar and its pinned dependencies ship inside this binary, so the
    // only thing an update can refresh is the runtime and the written copy of
    // the script.
    match crate::youtube::sidecar::ensure_script(&paths.lib_dir) {
        Ok(_) => progress("Sidecar script up to date."),
        Err(e) => progress(&format!("Could not write the sidecar: {e}")),
    }

    // The runtime the sidecar runs on. Unlike the old setup, where a missing
    // runtime merely cost some formats, this is what YouTube playback is.
    progress("Checking the JavaScript runtime (Deno)...");
    if let Err(e) = rt.block_on(setup::update_js_runtime(&paths, |l| progress(l))) {
        progress(&format!("Could not update Deno: {e}"));
    }

    // A new bot version can ship a lockfile naming dependency versions the
    // cache does not hold yet; fetch them now rather than on the first track.
    setup::warm_dependency_cache(&paths, progress);
    Ok(())
}


