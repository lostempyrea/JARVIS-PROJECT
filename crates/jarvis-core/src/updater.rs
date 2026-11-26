//! Self-update checking, verified binary downloading, and atomic replacement for JARVIS.

use crate::config::AppConfig;
use crate::events::{CoreEvent, EventBus, UpdateReleaseInfo};
use futures::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

const DEFAULT_GITHUB_API_BASE: &str = "https://api.github.com";
const PROGRESS_EMIT_BYTES: u64 = 512 * 1024;
const MIN_EXE_SIZE_BYTES: u64 = 1024;
const HTTP_TIMEOUT_SECS: u64 = 300;
const HTTP_CONNECT_TIMEOUT_SECS: u64 = 20;

/// Errors produced during update checks, downloads, verification, or installation.
#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid updater configuration: {0}")]
    InvalidConfig(String),
    #[error("invalid release metadata: {0}")]
    InvalidRelease(String),
    #[error("release is missing a valid SHA-256 checksum")]
    ChecksumMissing,
    #[error("SHA-256 checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("downloaded executable verification failed: {0}")]
    VerificationFailed(String),
    #[error("failed to install staged update: {0}")]
    InstallFailed(String),
}

/// Strategy used to apply a staged executable update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    /// Target executable was renamed to `.old` and replaced directly in-place.
    InPlaceSwapped,
    /// Target executable could not be renamed in-place; a deferred PowerShell script was staged
    /// for the caller to spawn right before exit.
    DeferredScript(PathBuf),
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    name: Option<String>,
    body: Option<String>,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    #[allow(dead_code)]
    prerelease: bool,
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    size: Option<u64>,
    digest: Option<String>,
}

/// Returns the running JARVIS version, preferring `JARVIS_BUILD_VERSION` when set at build time.
pub fn current_version() -> &'static str {
    match option_env!("JARVIS_BUILD_VERSION") {
        Some(v) if !v.is_empty() => v,
        _ => env!("CARGO_PKG_VERSION"),
    }
}

/// Returns the default directory used for staged update downloads (`%APPDATA%\Jarvis\updates`).
pub fn default_updates_dir() -> PathBuf {
    AppConfig::default_dir().join("updates")
}

/// Parse a SemVer string (with optional leading `v`/`V` and optional `+build` metadata)
/// into `(major, minor, patch, prerelease)`.
pub fn parse_semver(raw: &str) -> Option<(u64, u64, u64, Option<String>)> {
    let trimmed = raw.trim();
    let without_v = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);
    let without_build = without_v.split('+').next()?.trim();
    if without_build.is_empty() {
        return None;
    }

    let (core, pre_opt) = match without_build.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (without_build, None),
    };

    let mut parts = core.split('.');
    let major_str = parts.next()?;
    let minor_str = parts.next()?;
    let patch_str = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let parse_num = |s: &str| -> Option<u64> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse::<u64>().ok()
    };

    let major = parse_num(major_str)?;
    let minor = parse_num(minor_str)?;
    let patch = parse_num(patch_str)?;

    let prerelease = match pre_opt {
        Some(pre) => {
            if pre.is_empty() {
                return None;
            }
            for ident in pre.split('.') {
                if ident.is_empty()
                    || !ident
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                {
                    return None;
                }
            }
            Some(pre.to_string())
        }
        None => None,
    };

    Some((major, minor, patch, prerelease))
}

fn compare_prerelease(curr: &str, cand: &str) -> Ordering {
    let curr_parts: Vec<&str> = curr.split('.').collect();
    let cand_parts: Vec<&str> = cand.split('.').collect();

    for (a, b) in curr_parts.iter().zip(cand_parts.iter()) {
        let a_num = !a.is_empty() && a.bytes().all(|ch| ch.is_ascii_digit());
        let b_num = !b.is_empty() && b.bytes().all(|ch| ch.is_ascii_digit());
        let ord = match (a_num, b_num) {
            (true, true) => {
                // Compare numeric identifiers numerically; fall back to length + lexical for huge ints.
                match (a.parse::<u128>(), b.parse::<u128>()) {
                    (Ok(na), Ok(nb)) => na.cmp(&nb),
                    _ => a.len().cmp(&b.len()).then_with(|| a.cmp(b)),
                }
            }
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => a.cmp(b),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }

    curr_parts.len().cmp(&cand_parts.len())
}

/// Returns `true` when `candidate` is a valid SemVer strictly newer than `current`.
pub fn is_newer_version(current: &str, candidate: &str) -> bool {
    let Some((c_maj, c_min, c_pat, c_pre)) = parse_semver(current) else {
        return false;
    };
    let Some((n_maj, n_min, n_pat, n_pre)) = parse_semver(candidate) else {
        return false;
    };

    match (n_maj, n_min, n_pat).cmp(&(c_maj, c_min, c_pat)) {
        Ordering::Greater => return true,
        Ordering::Less => return false,
        Ordering::Equal => {}
    }

    match (c_pre.as_deref(), n_pre.as_deref()) {
        (Some(_), None) => true,
        (None, _) => false,
        (Some(curr_pre), Some(cand_pre)) => {
            compare_prerelease(curr_pre, cand_pre) == Ordering::Less
        }
    }
}

/// Parse a SHA-256 checksum from `.sha256` file contents or GitHub asset `digest` metadata.
///
/// Accepts formats such as:
/// - `<64-hex>  JARVIS.exe`
/// - `<64-hex>`
/// - `sha256:<64-hex>`
/// - `SHA256 (JARVIS.exe) = <64-hex>`
pub fn parse_sha256_text(raw: &str) -> Option<String> {
    let first_line = raw.lines().map(str::trim).find(|line| !line.is_empty())?;
    let candidate = if let Some(rest) = first_line
        .strip_prefix("sha256:")
        .or_else(|| first_line.strip_prefix("SHA256:"))
    {
        rest.trim()
    } else if let Some((_, rhs)) = first_line.rsplit_once('=') {
        rhs.trim()
    } else {
        first_line.split_whitespace().next()?
    };

    if candidate.len() == 64 && candidate.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(candidate.to_ascii_lowercase())
    } else {
        None
    }
}

fn old_backup_path(target_exe: &Path) -> PathBuf {
    let mut os: OsString = target_exe.as_os_str().to_os_string();
    os.push(".old");
    PathBuf::from(os)
}

fn strip_staged_exe_version(file_name: &str) -> Option<&str> {
    let rest = file_name
        .strip_prefix("JARVIS-")
        .or_else(|| file_name.strip_prefix("jarvis-"))?;
    if rest.len() > 4 && rest[rest.len() - 4..].eq_ignore_ascii_case(".exe") {
        Some(&rest[..rest.len() - 4])
    } else {
        None
    }
}

fn cleanup_updates_dir(updates_dir: &Path, current_ver: &str) {
    let curr_valid = parse_semver(current_ver).is_some();
    if let Ok(entries) = std::fs::read_dir(updates_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let is_part = name.to_ascii_lowercase().ends_with(".part");
            if is_part {
                if let Err(err) = std::fs::remove_file(&path) {
                    tracing::debug!(path = %path.display(), error = %err, "Failed to remove stale .part file");
                }
                continue;
            }
            if curr_valid {
                if let Some(ver) = strip_staged_exe_version(name) {
                    if parse_semver(ver).is_some() && !is_newer_version(current_ver, ver) {
                        if let Err(err) = std::fs::remove_file(&path) {
                            tracing::debug!(path = %path.display(), error = %err, "Failed to prune obsolete staged executable");
                        }
                    }
                }
            }
        }
    }
}

/// Best-effort cleanup of `<current_exe>.old`, stale `.part` files, and obsolete staged executables
/// in `%APPDATA%\Jarvis\updates\`.
pub fn cleanup_stale_old_executable() {
    if let Ok(current_exe) = std::env::current_exe() {
        let old_path = old_backup_path(&current_exe);
        if old_path.exists() {
            if let Err(err) = std::fs::remove_file(&old_path) {
                tracing::debug!(path = %old_path.display(), error = %err, "Failed to remove stale .old executable");
            } else {
                tracing::info!(path = %old_path.display(), "Removed stale .old executable");
            }
        }
    }

    cleanup_updates_dir(&default_updates_dir(), current_version());
}

/// Manages checking GitHub Releases, downloading & verifying updates, and installing them.
#[derive(Debug, Clone)]
pub struct UpdateManager {
    config: Arc<RwLock<AppConfig>>,
    event_bus: EventBus,
    updates_dir: PathBuf,
    api_base_url: String,
    client: reqwest::Client,
}

impl UpdateManager {
    /// Create a new [`UpdateManager`] backed by `%APPDATA%\Jarvis\updates` and `https://api.github.com`.
    pub fn new(config: Arc<RwLock<AppConfig>>, event_bus: EventBus) -> Self {
        Self::with_config(
            config,
            event_bus,
            default_updates_dir(),
            DEFAULT_GITHUB_API_BASE,
        )
    }

    /// Create an [`UpdateManager`] with custom updates directory and GitHub API base URL (for tests).
    pub fn with_config(
        config: Arc<RwLock<AppConfig>>,
        event_bus: EventBus,
        updates_dir: PathBuf,
        api_base_url: impl Into<String>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(format!("jarvis-rs/{}", current_version()))
            .connect_timeout(Duration::from_secs(HTTP_CONNECT_TIMEOUT_SECS))
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            config,
            event_bus,
            updates_dir,
            api_base_url: api_base_url.into(),
            client,
        }
    }

    pub fn updates_dir(&self) -> &Path {
        &self.updates_dir
    }

    fn emit_download_progress(
        &self,
        version: &str,
        status: &str,
        bytes_downloaded: u64,
        total_bytes: Option<u64>,
    ) {
        self.event_bus.publish(CoreEvent::UpdateDownloadProgress {
            version: version.to_string(),
            status: status.to_string(),
            bytes_downloaded,
            total_bytes,
        });
    }

    /// Check GitHub Releases (`releases/latest`) for a newer JARVIS release.
    ///
    /// Returns `Ok(None)` on HTTP 404, draft releases, non-newer versions, missing `JARVIS.exe` asset,
    /// or when `!manual` and the version matches `updater.skipped_version`.
    pub async fn check_for_update(
        &self,
        manual: bool,
    ) -> Result<Option<UpdateReleaseInfo>, UpdateError> {
        self.check_for_update_with_base_url(&self.api_base_url, manual)
            .await
    }

    /// Check a GitHub-compatible Releases API base URL (`releases/latest`) for a newer JARVIS release.
    pub async fn check_for_update_with_base_url(
        &self,
        base_url: &str,
        manual: bool,
    ) -> Result<Option<UpdateReleaseInfo>, UpdateError> {
        let updater_cfg = {
            let guard = self
                .config
                .read()
                .map_err(|_| UpdateError::InvalidConfig("config lock poisoned".to_string()))?;
            guard.updater.clone()
        };

        if !manual && !updater_cfg.auto_check_enabled {
            return Ok(None);
        }

        let repo = updater_cfg.github_repo.trim().trim_matches('/');
        let mut repo_parts = repo.split('/');
        let (Some(owner), Some(name), None) =
            (repo_parts.next(), repo_parts.next(), repo_parts.next())
        else {
            return Err(UpdateError::InvalidConfig(format!(
                "invalid github_repo '{repo}'; expected 'owner/repo'"
            )));
        };
        if owner.is_empty() || name.is_empty() || repo.contains(char::is_whitespace) {
            return Err(UpdateError::InvalidConfig(format!(
                "invalid github_repo '{repo}'; expected 'owner/repo'"
            )));
        }

        let url = format!(
            "{}/repos/{owner}/{name}/releases/latest",
            base_url.trim_end_matches('/')
        );

        let resp = self
            .client
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(UpdateError::InvalidRelease(format!(
                "GitHub releases API returned HTTP {}",
                resp.status()
            )));
        }

        let release: GitHubRelease = resp.json().await?;
        if release.draft {
            return Ok(None);
        }

        let current = current_version();
        if !is_newer_version(current, &release.tag_name) {
            return Ok(None);
        }

        let normalized_version = normalize_version(&release.tag_name);
        if !manual {
            if let Some(skipped) = updater_cfg.skipped_version.as_deref() {
                if normalize_version(skipped) == normalized_version {
                    return Ok(None);
                }
            }
        }

        let exe_asset = release
            .assets
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case("JARVIS.exe"));
        let Some(exe_asset) = exe_asset else {
            tracing::warn!(
                tag = %release.tag_name,
                "Latest GitHub release has no JARVIS.exe asset; ignoring"
            );
            return Ok(None);
        };

        let sha_asset = release
            .assets
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case("JARVIS.exe.sha256"));

        let expected_sha256 = exe_asset
            .digest
            .as_deref()
            .and_then(parse_sha256_text);
        let sha256_download_url = sha_asset.map(|a| a.browser_download_url.clone());

        let title = release
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&release.tag_name)
            .to_string();

        let info = UpdateReleaseInfo {
            tag_name: release.tag_name.clone(),
            version: normalized_version,
            title,
            notes: release.body.unwrap_or_default(),
            html_url: release.html_url,
            published_at: release.published_at,
            exe_download_url: exe_asset.browser_download_url.clone(),
            sha256_download_url,
            expected_sha256,
            size_bytes: exe_asset.size,
        };

        self.event_bus.publish(CoreEvent::UpdateAvailable {
            release: info.clone(),
            manual_check: manual,
        });

        Ok(Some(info))
    }

    /// Download `release.exe_download_url` into `<updates_dir>/JARVIS-<version>.exe.part`,
    /// emit progress every 512 KB, verify `b"MZ"`, size >= 1024, and exact 64-hex SHA-256,
    /// then promote to `<updates_dir>/JARVIS-<version>.exe`.
    pub async fn download_and_verify(
        &self,
        release: &UpdateReleaseInfo,
    ) -> Result<PathBuf, UpdateError> {
        let expected_sha = self.resolve_expected_sha256(release).await?;

        tokio::fs::create_dir_all(&self.updates_dir).await?;
        let safe_ver = sanitize_version_for_filename(&release.version);
        let part_path = self.updates_dir.join(format!("JARVIS-{safe_ver}.exe.part"));
        let final_path = self.updates_dir.join(format!("JARVIS-{safe_ver}.exe"));

        if part_path.exists() {
            let _ = tokio::fs::remove_file(&part_path).await;
        }

        let download_result = self
            .stream_and_hash_to_part(release, &part_path, &expected_sha)
            .await;

        match download_result {
            Ok(downloaded_bytes) => {
                if final_path.exists() {
                    let _ = tokio::fs::remove_file(&final_path).await;
                }
                tokio::fs::rename(&part_path, &final_path).await?;
                self.emit_download_progress(
                    &release.version,
                    "ready",
                    downloaded_bytes,
                    Some(downloaded_bytes),
                );
                Ok(final_path)
            }
            Err(err) => {
                let _ = tokio::fs::remove_file(&part_path).await;
                self.emit_download_progress(&release.version, "failed", 0, release.size_bytes);
                Err(err)
            }
        }
    }

    async fn resolve_expected_sha256(
        &self,
        release: &UpdateReleaseInfo,
    ) -> Result<String, UpdateError> {
        if let Some(raw) = release.expected_sha256.as_deref() {
            if let Some(parsed) = parse_sha256_text(raw) {
                return Ok(parsed);
            }
            return Err(UpdateError::ChecksumMissing);
        }

        let Some(sha_url) = release.sha256_download_url.as_deref() else {
            return Err(UpdateError::ChecksumMissing);
        };

        let resp = self.client.get(sha_url).send().await?;
        if !resp.status().is_success() {
            return Err(UpdateError::InvalidRelease(format!(
                "failed to fetch SHA-256 checksum from '{sha_url}': HTTP {}",
                resp.status()
            )));
        }
        let body = resp.text().await?;
        parse_sha256_text(&body).ok_or(UpdateError::ChecksumMissing)
    }

    async fn stream_and_hash_to_part(
        &self,
        release: &UpdateReleaseInfo,
        part_path: &Path,
        expected_sha: &str,
    ) -> Result<u64, UpdateError> {
        let resp = self.client.get(&release.exe_download_url).send().await?;
        if !resp.status().is_success() {
            return Err(UpdateError::InvalidRelease(format!(
                "failed to download release executable: HTTP {}",
                resp.status()
            )));
        }

        let total_bytes = resp.content_length().or(release.size_bytes);
        self.emit_download_progress(&release.version, "downloading", 0, total_bytes);

        let mut file = tokio::fs::File::create(part_path).await?;
        let mut stream = resp.bytes_stream();
        let mut hasher = Sha256::new();
        let mut downloaded: u64 = 0;
        let mut next_emit_at: u64 = PROGRESS_EMIT_BYTES;
        let mut magic = [0u8; 2];
        let mut magic_len: usize = 0;

        while let Some(chunk_res) = stream.next().await {
            let chunk = chunk_res?;
            if magic_len < 2 {
                let take = (2 - magic_len).min(chunk.len());
                magic[magic_len..magic_len + take].copy_from_slice(&chunk[..take]);
                magic_len += take;
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
            downloaded += chunk.len() as u64;

            if downloaded >= next_emit_at {
                self.emit_download_progress(
                    &release.version,
                    "downloading",
                    downloaded,
                    total_bytes,
                );
                next_emit_at = downloaded + PROGRESS_EMIT_BYTES;
            }
        }

        file.flush().await?;
        file.sync_all().await?;
        drop(file);

        self.emit_download_progress(&release.version, "verifying", downloaded, total_bytes);

        if downloaded < MIN_EXE_SIZE_BYTES {
            return Err(UpdateError::VerificationFailed(format!(
                "downloaded executable is too small ({downloaded} bytes < {MIN_EXE_SIZE_BYTES})"
            )));
        }
        if magic_len < 2 || &magic != b"MZ" {
            return Err(UpdateError::VerificationFailed(
                "downloaded file does not have a valid Windows PE 'MZ' header".to_string(),
            ));
        }

        let actual_sha = hex::encode(hasher.finalize());
        if actual_sha != expected_sha {
            return Err(UpdateError::ChecksumMismatch {
                expected: expected_sha.to_string(),
                actual: actual_sha,
            });
        }

        Ok(downloaded)
    }
}

fn normalize_version(raw: &str) -> String {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed)
        .to_string()
}

fn sanitize_version_for_filename(raw: &str) -> String {
    let sanitized: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn verify_staged_pe_file(staged_exe: &Path) -> Result<(), UpdateError> {
    let meta = std::fs::metadata(staged_exe).map_err(|err| {
        UpdateError::VerificationFailed(format!(
            "cannot stat staged executable '{}': {err}",
            staged_exe.display()
        ))
    })?;
    if !meta.is_file() {
        return Err(UpdateError::VerificationFailed(format!(
            "staged path '{}' is not a file",
            staged_exe.display()
        )));
    }
    if meta.len() < MIN_EXE_SIZE_BYTES {
        return Err(UpdateError::VerificationFailed(format!(
            "staged executable '{}' is too small ({} bytes)",
            staged_exe.display(),
            meta.len()
        )));
    }
    let mut f = std::fs::File::open(staged_exe)?;
    let mut magic = [0u8; 2];
    f.read_exact(&mut magic).map_err(|err| {
        UpdateError::VerificationFailed(format!(
            "failed reading staged executable header '{}': {err}",
            staged_exe.display()
        ))
    })?;
    if &magic != b"MZ" {
        return Err(UpdateError::VerificationFailed(format!(
            "staged executable '{}' lacks Windows PE 'MZ' header",
            staged_exe.display()
        )));
    }
    Ok(())
}

/// Apply a staged executable onto `target_exe` using atomic `.old` rename + rollback if copy fails,
/// falling back on Windows to staging `apply_update.ps1` (`InstallMethod::DeferredScript`) for the
/// caller to spawn right before exit when `target_exe` cannot be renamed in-place.
pub fn install_staged_onto_path(
    staged_exe: &Path,
    target_exe: &Path,
) -> Result<InstallMethod, UpdateError> {
    install_staged_onto_path_with_copier(staged_exe, target_exe, |src, dst| {
        std::fs::copy(src, dst).map(|_| ())
    })
}

fn install_staged_onto_path_with_copier<F>(
    staged_exe: &Path,
    target_exe: &Path,
    copy_fn: F,
) -> Result<InstallMethod, UpdateError>
where
    F: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    verify_staged_pe_file(staged_exe)?;

    if let Some(parent) = target_exe.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    if !target_exe.exists() {
        copy_fn(staged_exe, target_exe).map_err(|err| {
            UpdateError::InstallFailed(format!(
                "failed copying staged executable to '{}': {err}",
                target_exe.display()
            ))
        })?;
        return Ok(InstallMethod::InPlaceSwapped);
    }

    let old_path = old_backup_path(target_exe);
    if old_path.exists() {
        let _ = std::fs::remove_file(&old_path);
    }

    match std::fs::rename(target_exe, &old_path) {
        Ok(()) => match copy_fn(staged_exe, target_exe) {
            Ok(()) => {
                let _ = std::fs::remove_file(&old_path);
                Ok(InstallMethod::InPlaceSwapped)
            }
            Err(copy_err) => {
                let _ = std::fs::remove_file(target_exe);
                match std::fs::rename(&old_path, target_exe) {
                    Ok(()) => Err(UpdateError::InstallFailed(format!(
                        "copy to '{}' failed ({copy_err}); rolled back original executable",
                        target_exe.display()
                    ))),
                    Err(rollback_err) => Err(UpdateError::InstallFailed(format!(
                        "copy to '{}' failed ({copy_err}); rollback rename from '{}' also failed ({rollback_err})",
                        target_exe.display(),
                        old_path.display()
                    ))),
                }
            }
        },
        Err(rename_err) => {
            #[cfg(windows)]
            {
                let script_path = prepare_deferred_apply_script(staged_exe, target_exe)?;
                let _ = rename_err;
                Ok(InstallMethod::DeferredScript(script_path))
            }
            #[cfg(not(windows))]
            {
                Err(UpdateError::InstallFailed(format!(
                    "failed to rename '{}' to '{}': {rename_err}",
                    target_exe.display(),
                    old_path.display()
                )))
            }
        }
    }
}

#[cfg(windows)]
fn ps_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(windows)]
fn prepare_deferred_apply_script(
    staged_exe: &Path,
    target_exe: &Path,
) -> Result<PathBuf, UpdateError> {
    let script_dir = staged_exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(default_updates_dir);
    std::fs::create_dir_all(&script_dir)?;
    let script_path = script_dir.join("apply_update.ps1");

    let pid = std::process::id();
    let staged_quoted = ps_single_quote(&staged_exe.to_string_lossy());
    let target_quoted = ps_single_quote(&target_exe.to_string_lossy());

    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$pidToWait = {pid}
$stagedExe = {staged_quoted}
$targetExe = {target_quoted}

for ($i = 0; $i -lt 120; $i++) {{
    if (-not (Get-Process -Id $pidToWait -ErrorAction SilentlyContinue)) {{
        break
    }}
    Start-Sleep -Milliseconds 250
}}

$copied = $false
for ($attempt = 0; $attempt -lt 20; $attempt++) {{
    try {{
        Copy-Item -LiteralPath $stagedExe -Destination $targetExe -Force
        $copied = $true
        break
    }}
    catch {{
        Start-Sleep -Milliseconds 500
    }}
}}

if ($copied) {{
    Remove-Item -LiteralPath $stagedExe -Force -ErrorAction SilentlyContinue
    Start-Process -FilePath $targetExe
}}
"#
    );

    // Prepend UTF-8 BOM so Windows PowerShell 5.1 decodes non-ASCII paths reliably.
    let mut bytes = Vec::with_capacity(3 + script.len());
    bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    bytes.extend_from_slice(script.as_bytes());
    std::fs::write(&script_path, &bytes)?;

    Ok(script_path)
}

/// Apply a staged executable onto the currently running executable path (`std::env::current_exe()`).
///
/// When `target_exe` cannot be renamed in-place on Windows, `apply_update.ps1` is staged and
/// returned as [`InstallMethod::DeferredScript`] for the caller to spawn right before exiting.
pub fn apply_staged_to_current_exe(staged_exe: &Path) -> Result<InstallMethod, UpdateError> {
    let current_exe = std::env::current_exe()?;
    install_staged_onto_path(staged_exe, &current_exe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_fake_pe(dir: &Path, name: &str, fill: u8, size: usize) -> PathBuf {
        let path = dir.join(name);
        let mut bytes = vec![fill; size.max(1024)];
        bytes[0] = b'M';
        bytes[1] = b'Z';
        std::fs::write(&path, &bytes).unwrap();
        path
    }

    #[test]
    fn parse_semver_valid_and_invalid_cases() {
        assert_eq!(parse_semver("0.1.0"), Some((0, 1, 0, None)));
        assert_eq!(parse_semver("v1.2.3"), Some((1, 2, 3, None)));
        assert_eq!(
            parse_semver("V2.10.30-beta.1+build42"),
            Some((2, 10, 30, Some("beta.1".to_string())))
        );
        assert_eq!(parse_semver("  v0.2.0-rc1  "), Some((0, 2, 0, Some("rc1".to_string()))));

        assert_eq!(parse_semver(""), None);
        assert_eq!(parse_semver("v1.2"), None);
        assert_eq!(parse_semver("1.2.3.4"), None);
        assert_eq!(parse_semver("1.2.x"), None);
        assert_eq!(parse_semver("1.2.3-"), None);
        assert_eq!(parse_semver("1.2.3-beta..1"), None);
        assert_eq!(parse_semver("+1.2.3"), None);
    }

    #[test]
    fn is_newer_version_respects_semver_precedence() {
        assert!(is_newer_version("0.1.0", "0.2.0"));
        assert!(is_newer_version("0.1.0", "v0.1.1"));
        assert!(is_newer_version("0.1.9", "0.2.0-alpha.1"));
        assert!(is_newer_version("1.0.0-alpha.1", "1.0.0-alpha.2"));
        assert!(is_newer_version("1.0.0-alpha.1", "1.0.0-alpha.beta"));
        assert!(is_newer_version("1.0.0-beta.11", "1.0.0-rc.1"));
        assert!(is_newer_version("1.0.0-rc.1", "1.0.0"));

        assert!(!is_newer_version("0.2.0", "0.2.0"));
        assert!(!is_newer_version("0.2.0", "v0.2.0"));
        assert!(!is_newer_version("0.2.0", "0.1.9"));
        assert!(!is_newer_version("1.0.0", "1.0.0-rc.1"));
        assert!(!is_newer_version("1.0.0", "invalid"));
        assert!(!is_newer_version("invalid", "1.0.0"));
    }

    #[test]
    fn parse_sha256_text_extracts_valid_digests() {
        let digest = "a".repeat(64);
        let upper = "A".repeat(64);

        assert_eq!(
            parse_sha256_text(&format!("{upper}  JARVIS.exe\n")),
            Some(digest.clone())
        );
        assert_eq!(parse_sha256_text(&format!("  {digest}\r\n")), Some(digest.clone()));
        assert_eq!(
            parse_sha256_text(&format!("sha256:{upper}")),
            Some(digest.clone())
        );
        assert_eq!(
            parse_sha256_text(&format!("SHA256 (JARVIS.exe) = {upper}")),
            Some(digest)
        );

        assert_eq!(parse_sha256_text(""), None);
        assert_eq!(parse_sha256_text("not-a-hash  JARVIS.exe"), None);
        assert_eq!(parse_sha256_text(&"a".repeat(63)), None);
        assert_eq!(parse_sha256_text(&format!("{}z", "a".repeat(63))), None);
    }

    #[test]
    fn install_staged_onto_path_happy_path_replaces_target() {
        let dir = TempDir::new().unwrap();
        let staged = make_fake_pe(dir.path(), "JARVIS-0.2.0.exe", 0xAA, 2048);
        let target = make_fake_pe(dir.path(), "JARVIS.exe", 0x11, 2048);

        let method = install_staged_onto_path(&staged, &target).unwrap();
        assert_eq!(method, InstallMethod::InPlaceSwapped);

        let updated = std::fs::read(&target).unwrap();
        assert_eq!(updated.len(), 2048);
        assert_eq!(&updated[..2], b"MZ");
        assert!(updated[2..].iter().all(|&b| b == 0xAA));
        assert!(!old_backup_path(&target).exists());
    }

    #[test]
    fn install_staged_onto_path_rolls_back_when_copy_fails() {
        let dir = TempDir::new().unwrap();
        let staged = make_fake_pe(dir.path(), "JARVIS-0.2.0.exe", 0xBB, 2048);
        let target = make_fake_pe(dir.path(), "JARVIS.exe", 0x22, 2048);

        let err = install_staged_onto_path_with_copier(&staged, &target, |_src, dst| {
            // Simulate partial write before copy fails
            let _ = std::fs::write(dst, b"MZpartial");
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "simulated disk full",
            ))
        })
        .unwrap_err();

        assert!(err.to_string().contains("rolled back"));
        let restored = std::fs::read(&target).unwrap();
        assert_eq!(restored.len(), 2048);
        assert_eq!(&restored[..2], b"MZ");
        assert!(restored[2..].iter().all(|&b| b == 0x22));
        assert!(!old_backup_path(&target).exists());
    }

    #[test]
    fn install_staged_onto_path_rejects_invalid_pe_before_touching_target() {
        let dir = TempDir::new().unwrap();
        let bad_staged = dir.path().join("bad.exe");
        std::fs::write(&bad_staged, vec![0x00; 2048]).unwrap();
        let target = make_fake_pe(dir.path(), "JARVIS.exe", 0x33, 2048);

        let err = install_staged_onto_path(&bad_staged, &target).unwrap_err();
        assert!(matches!(err, UpdateError::VerificationFailed(_)));
        let intact = std::fs::read(&target).unwrap();
        assert!(intact[2..].iter().all(|&b| b == 0x33));
    }

    async fn spawn_mock_http_server<F>(handler: F) -> (String, tokio::task::JoinHandle<()>)
    where
        F: Fn(&str) -> (u16, &'static str, Vec<u8>) + Send + Sync + 'static,
    {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());
        let handler = Arc::new(handler);

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let (reader_half, mut writer_half) = stream.split();
                    let mut buf_reader = BufReader::new(reader_half);
                    let mut request_line = String::new();
                    if buf_reader.read_line(&mut request_line).await.is_err() {
                        return;
                    }
                    loop {
                        let mut hdr = String::new();
                        match buf_reader.read_line(&mut hdr).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) if hdr == "\r\n" || hdr == "\n" => break,
                            Ok(_) => {}
                        }
                    }
                    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
                    let (status, content_type, body) = handler(path);
                    let reason = match status {
                        200 => "OK",
                        404 => "Not Found",
                        _ => "Error",
                    };
                    let head = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = writer_half.write_all(head.as_bytes()).await;
                    let _ = writer_half.write_all(&body).await;
                    let _ = writer_half.flush().await;
                });
            }
        });

        (base_url, handle)
    }

    fn make_test_manager(
        updates_dir: PathBuf,
        api_base_url: &str,
        skipped_version: Option<&str>,
    ) -> (UpdateManager, Arc<RwLock<AppConfig>>, EventBus) {
        let mut cfg = AppConfig::default();
        cfg.updater.github_repo = "owner/jarvis-test".to_string();
        cfg.updater.skipped_version = skipped_version.map(str::to_string);
        let cfg_lock = Arc::new(RwLock::new(cfg));
        let event_bus = EventBus::default();
        let mgr = UpdateManager::with_config(
            Arc::clone(&cfg_lock),
            event_bus.clone(),
            updates_dir,
            api_base_url,
        );
        (mgr, cfg_lock, event_bus)
    }

    #[tokio::test]
    async fn check_for_update_handles_404_and_draft_releases() {
        let dir = TempDir::new().unwrap();
        let (base_404, h_404) =
            spawn_mock_http_server(|_path| (404, "application/json", b"{}".to_vec())).await;
        let (mgr_404, _, _) = make_test_manager(dir.path().to_path_buf(), "https://unused", None);
        let res_404 = mgr_404
            .check_for_update_with_base_url(&base_404, true)
            .await
            .unwrap();
        assert_eq!(res_404, None);
        h_404.abort();

        let draft_json = r#"{
            "tag_name": "v9.9.9",
            "name": "v9.9.9",
            "body": "Draft release",
            "html_url": "https://example.test/r/v9.9.9",
            "draft": true,
            "prerelease": false,
            "assets": [
                {
                    "name": "JARVIS.exe",
                    "browser_download_url": "https://example.test/JARVIS.exe",
                    "size": 2048
                }
            ]
        }"#;
        let (base_draft, h_draft) =
            spawn_mock_http_server(move |_path| (200, "application/json", draft_json.as_bytes().to_vec())).await;
        let (mgr_draft, _, _) = make_test_manager(dir.path().to_path_buf(), &base_draft, None);
        let res_draft = mgr_draft
            .check_for_update_with_base_url(&base_draft, true)
            .await
            .unwrap();
        assert_eq!(res_draft, None);
        h_draft.abort();
    }

    #[tokio::test]
    async fn check_for_update_respects_skipped_version_unless_manual() {
        let dir = TempDir::new().unwrap();
        let release_json = r#"{
            "tag_name": "v9.9.9",
            "name": "JARVIS v9.9.9",
            "body": "Release notes",
            "html_url": "https://example.test/r/v9.9.9",
            "draft": false,
            "prerelease": false,
            "assets": [
                {
                    "name": "JARVIS.exe",
                    "browser_download_url": "https://example.test/JARVIS.exe",
                    "size": 4096
                },
                {
                    "name": "JARVIS.exe.sha256",
                    "browser_download_url": "https://example.test/JARVIS.exe.sha256",
                    "size": 76
                }
            ]
        }"#;

        let (base_url, server_handle) = spawn_mock_http_server(move |path| {
            if path == "/repos/owner/jarvis-test/releases/latest" {
                (200, "application/json", release_json.as_bytes().to_vec())
            } else {
                (404, "text/plain", b"not found".to_vec())
            }
        })
        .await;

        let (mgr, _, bus) = make_test_manager(dir.path().to_path_buf(), &base_url, Some("9.9.9"));
        let mut rx = bus.subscribe();

        // Background auto-check skips skipped_version
        let auto_res = mgr
            .check_for_update_with_base_url(&base_url, false)
            .await
            .unwrap();
        assert_eq!(auto_res, None);

        // Manual check bypasses skipped_version and publishes UpdateAvailable
        let manual_res = mgr
            .check_for_update_with_base_url(&base_url, true)
            .await
            .unwrap()
            .expect("manual check should return skipped release");
        assert_eq!(manual_res.version, "9.9.9");
        assert_eq!(manual_res.notes, "Release notes");

        let ev = rx.recv().await.unwrap();
        assert!(matches!(
            ev,
            CoreEvent::UpdateAvailable {
                manual_check: true,
                ..
            }
        ));

        server_handle.abort();
    }

    #[tokio::test]
    async fn download_and_verify_handles_missing_mismatch_non_pe_and_happy_path() {
        let dir = TempDir::new().unwrap();
        let mut valid_pe = vec![0x42u8; 4096];
        valid_pe[0] = b'M';
        valid_pe[1] = b'Z';
        let valid_sha = hex::encode(Sha256::digest(&valid_pe));

        let non_pe = vec![0x55u8; 4096];
        let non_pe_sha = hex::encode(Sha256::digest(&non_pe));

        let valid_pe_srv = valid_pe.clone();
        let valid_sha_srv = valid_sha.clone();
        let non_pe_srv = non_pe.clone();

        let (base_url, server_handle) = spawn_mock_http_server(move |path| match path {
            "/valid.exe" => (200, "application/octet-stream", valid_pe_srv.clone()),
            "/valid.sha256" => (
                200,
                "text/plain",
                format!("{valid_sha_srv}  JARVIS.exe\n").into_bytes(),
            ),
            "/nonpe.exe" => (200, "application/octet-stream", non_pe_srv.clone()),
            _ => (404, "text/plain", b"not found".to_vec()),
        })
        .await;

        let (mgr, _, bus) = make_test_manager(dir.path().to_path_buf(), &base_url, None);
        let mut rx = bus.subscribe();

        // 1. Missing SHA-256 -> ChecksumMissing
        let missing_sha_rel = UpdateReleaseInfo {
            tag_name: "v9.1.0".to_string(),
            version: "9.1.0".to_string(),
            title: "v9.1.0".to_string(),
            notes: String::new(),
            html_url: format!("{base_url}/r"),
            published_at: None,
            exe_download_url: format!("{base_url}/valid.exe"),
            sha256_download_url: None,
            expected_sha256: None,
            size_bytes: Some(valid_pe.len() as u64),
        };
        let err = mgr.download_and_verify(&missing_sha_rel).await.unwrap_err();
        assert!(matches!(err, UpdateError::ChecksumMissing));

        // 2. Checksum mismatch -> ChecksumMismatch & .part removed
        let bad_sha_rel = UpdateReleaseInfo {
            expected_sha256: Some("0".repeat(64)),
            ..missing_sha_rel.clone()
        };
        let err = mgr.download_and_verify(&bad_sha_rel).await.unwrap_err();
        assert!(matches!(err, UpdateError::ChecksumMismatch { .. }));
        assert!(!dir.path().join("JARVIS-9.1.0.exe.part").exists());

        // 3. Non-PE header -> VerificationFailed & .part removed
        let non_pe_rel = UpdateReleaseInfo {
            version: "9.1.1".to_string(),
            exe_download_url: format!("{base_url}/nonpe.exe"),
            expected_sha256: Some(non_pe_sha),
            ..missing_sha_rel.clone()
        };
        let err = mgr.download_and_verify(&non_pe_rel).await.unwrap_err();
        assert!(matches!(err, UpdateError::VerificationFailed(_)));
        assert!(!dir.path().join("JARVIS-9.1.1.exe.part").exists());

        // 4. Happy path via sha256_download_url -> promotes to JARVIS-9.2.0.exe
        let good_rel = UpdateReleaseInfo {
            tag_name: "v9.2.0".to_string(),
            version: "9.2.0".to_string(),
            title: "v9.2.0".to_string(),
            sha256_download_url: Some(format!("{base_url}/valid.sha256")),
            expected_sha256: None,
            ..missing_sha_rel
        };
        let staged = mgr.download_and_verify(&good_rel).await.unwrap();
        assert_eq!(staged, dir.path().join("JARVIS-9.2.0.exe"));
        assert!(!dir.path().join("JARVIS-9.2.0.exe.part").exists());
        assert_eq!(std::fs::read(&staged).unwrap(), valid_pe);

        let mut saw_ready = false;
        while let Ok(ev) = rx.try_recv() {
            if let CoreEvent::UpdateDownloadProgress { status, .. } = ev {
                if status == "ready" {
                    saw_ready = true;
                }
            }
        }
        assert!(saw_ready);

        server_handle.abort();
    }

    #[cfg(windows)]
    #[test]
    fn prepare_deferred_apply_script_writes_utf8_bom_and_escapes_quotes() {
        assert_eq!(
            ps_single_quote(r"C:\Users\O'Brien\Nguyễn\JARVIS.exe"),
            r"'C:\Users\O''Brien\Nguyễn\JARVIS.exe'"
        );

        let dir = TempDir::new().unwrap();
        let staged = make_fake_pe(dir.path(), "JARVIS-0.2.0.exe", 0xAA, 2048);
        let target = dir.path().join("O'Brien").join("Nguyễn").join("JARVIS.exe");

        let script_path = prepare_deferred_apply_script(&staged, &target).unwrap();
        let raw = std::fs::read(&script_path).unwrap();
        assert!(raw.starts_with(&[0xEF, 0xBB, 0xBF]));

        let text = std::str::from_utf8(&raw[3..]).unwrap();
        assert!(text.contains(r"O''Brien\Nguyễn\JARVIS.exe"));
    }

    #[test]
    fn cleanup_updates_dir_removes_part_and_obsolete_staged_exes() {
        let dir = TempDir::new().unwrap();
        let part_file = dir.path().join("JARVIS-0.2.0.exe.part");
        let old_exe = make_fake_pe(dir.path(), "JARVIS-0.1.0.exe", 0x11, 1024);
        let curr_exe = make_fake_pe(dir.path(), "JARVIS-0.2.0.exe", 0x22, 1024);
        let newer_exe = make_fake_pe(dir.path(), "JARVIS-0.3.0.exe", 0x33, 1024);
        std::fs::write(&part_file, b"partial").unwrap();

        cleanup_updates_dir(dir.path(), "0.2.0");

        assert!(!part_file.exists());
        assert!(!old_exe.exists());
        assert!(!curr_exe.exists());
        assert!(newer_exe.exists());
    }
}
