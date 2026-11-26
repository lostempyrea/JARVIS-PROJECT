//! Windows binary locator and auto-installer for `yt-dlp.exe`, `ffmpeg.exe`, and `agy.exe`.
//!
//! Binaries are resolved in `%APPDATA%\Jarvis\bin\` first, then system `PATH`.
//! All subprocess spawns on Windows apply `CREATE_NO_WINDOW` (`0x08000000`) via
//! [`hidden_tokio_command`] / [`hidden_std_command`].

use crate::config::AppConfig;
use crate::events::{CoreEvent, EventBus};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

/// Windows process creation flag suppressing console windows for background tools.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const DOWNLOAD_TIMEOUT_SECS: u64 = 300;
const PROGRESS_EMIT_BYTES: u64 = 512 * 1024;

pub const DEFAULT_YT_DLP_URL: &str =
    "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
pub const DEFAULT_FFMPEG_URL: &str =
    "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip";
pub const DEFAULT_WHISPER_CLI_URL: &str =
    "https://github.com/ggml-org/whisper.cpp/releases/latest/download/whisper-bin-x64.zip";
pub const DEFAULT_WHISPER_MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin";

/// Errors returned by [`BinaryManager`] operations.
#[derive(Debug, Error)]
pub enum BinaryError {
    #[error("HTTP download error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("installer error: {0}")]
    Installer(String),
}

/// Installation status of external binaries used by JARVIS (`yt-dlp`, `ffmpeg`, `agy`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BinaryStatus {
    pub bin_dir: PathBuf,
    pub yt_dlp_installed: bool,
    pub yt_dlp_path: Option<PathBuf>,
    pub ffmpeg_installed: bool,
    pub ffmpeg_path: Option<PathBuf>,
    pub agy_installed: bool,
    pub agy_path: Option<PathBuf>,
    #[serde(default)]
    pub whisper_cli_installed: bool,
    #[serde(default)]
    pub whisper_cli_path: Option<PathBuf>,
    #[serde(default)]
    pub whisper_model_installed: bool,
    #[serde(default)]
    pub whisper_model_path: Option<PathBuf>,
}

/// Return `%APPDATA%\Jarvis\bin` on Windows (or platform config dir fallback).
pub fn default_bin_dir() -> PathBuf {
    AppConfig::default_dir().join("bin")
}

/// Return `%APPDATA%\Jarvis\models` (or platform config dir fallback).
pub fn default_models_dir() -> PathBuf {
    AppConfig::default_dir().join("models")
}

pub fn locate_whisper_cli(bin_dir: Option<&Path>) -> Option<PathBuf> {
    locate_binary("whisper-cli", bin_dir).or_else(|| locate_binary("whisper", bin_dir))
}

pub fn locate_whisper_model(models_dir: Option<&Path>) -> Option<PathBuf> {
    let dir = models_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(default_models_dir);
    let candidates = ["ggml-base.bin", "ggml-small.bin", "ggml-tiny.bin", "ggml-medium.bin"];
    for name in candidates {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    // Also check default_bin_dir() if models_dir wasn't explicitly checking there or as fallback
    let bin_fallback = default_bin_dir();
    for name in candidates {
        let p = bin_fallback.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Normalize executable filename for the host OS (`<stem>.exe` on Windows).
pub fn exe_name(stem: &str) -> String {
    if cfg!(windows) {
        if stem.to_ascii_lowercase().ends_with(".exe") {
            stem.to_string()
        } else {
            format!("{stem}.exe")
        }
    } else {
        stem.to_string()
    }
}

/// Build a `tokio::process::Command` with `CREATE_NO_WINDOW` (`0x08000000`) on Windows.
pub fn hidden_tokio_command(program: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = &mut cmd;
    cmd
}

/// Build a `std::process::Command` with `CREATE_NO_WINDOW` (`0x08000000`) on Windows.
pub fn hidden_std_command(program: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = &mut cmd;
    cmd
}

/// Locate a binary inside `bin_dir` (defaults to `%APPDATA%\Jarvis\bin`) first,
/// then fall back to system `PATH`, and on Windows also scan Python user/program `Scripts` dirs.
pub fn locate_binary(stem: &str, bin_dir: Option<&Path>) -> Option<PathBuf> {
    let filename = exe_name(stem);
    let dir = bin_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(default_bin_dir);
    let local = dir.join(&filename);
    if local.is_file() {
        return Some(local);
    }

    if let Some(path_var) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&path_var) {
            let candidate = entry.join(&filename);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    #[cfg(windows)]
    {
        let mut script_roots = Vec::new();
        if let Some(appdata) = std::env::var_os("APPDATA") {
            if !appdata.is_empty() {
                script_roots.push(PathBuf::from(appdata).join("Python"));
            }
        }
        if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") {
            if !local_appdata.is_empty() {
                script_roots.push(
                    PathBuf::from(local_appdata)
                        .join("Programs")
                        .join("Python"),
                );
            }
        }
        for root in script_roots {
            if let Ok(entries) = std::fs::read_dir(&root) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if name.to_string_lossy().starts_with("Python") {
                        let candidate = entry.path().join("Scripts").join(&filename);
                        if candidate.is_file() {
                            return Some(candidate);
                        }
                    }
                }
            }
        }
    }

    None
}

/// Check installation status for `yt-dlp`, `ffmpeg`, `agy`, and whisper cli/model.
pub fn check_binaries(bin_dir: Option<&Path>) -> BinaryStatus {
    let resolved_dir = bin_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(default_bin_dir);
    let yt_dlp_path = locate_binary("yt-dlp", Some(&resolved_dir));
    let ffmpeg_path = locate_binary("ffmpeg", Some(&resolved_dir));
    let agy_path = locate_binary("agy", Some(&resolved_dir));
    let whisper_cli_path = locate_whisper_cli(Some(&resolved_dir));
    let models_dir = resolved_dir.parent().map(|p| p.join("models")).unwrap_or_else(default_models_dir);
    let whisper_model_path = locate_whisper_model(Some(&models_dir));

    BinaryStatus {
        bin_dir: resolved_dir,
        yt_dlp_installed: yt_dlp_path.is_some(),
        yt_dlp_path,
        ffmpeg_installed: ffmpeg_path.is_some(),
        ffmpeg_path,
        agy_installed: agy_path.is_some(),
        agy_path,
        whisper_cli_installed: whisper_cli_path.is_some(),
        whisper_cli_path,
        whisper_model_installed: whisper_model_path.is_some(),
        whisper_model_path,
    }
}

/// Manages locating and auto-downloading external media/CLI binaries into `%APPDATA%\Jarvis\bin\`.
#[derive(Debug, Clone)]
pub struct BinaryManager {
    bin_dir: PathBuf,
    yt_dlp_url: String,
    ffmpeg_url: String,
    event_bus: Option<EventBus>,
    local_only: bool,
}

impl Default for BinaryManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BinaryManager {
    /// Create a [`BinaryManager`] targeting `%APPDATA%\Jarvis\bin\`.
    pub fn new() -> Self {
        Self {
            bin_dir: default_bin_dir(),
            yt_dlp_url: DEFAULT_YT_DLP_URL.to_string(),
            ffmpeg_url: DEFAULT_FFMPEG_URL.to_string(),
            event_bus: None,
            local_only: false,
        }
    }

    /// Create a [`BinaryManager`] wired to [`EventBus`] for download progress events.
    pub fn with_event_bus(event_bus: EventBus) -> Self {
        Self {
            event_bus: Some(event_bus),
            ..Self::new()
        }
    }

    /// Create a [`BinaryManager`] with custom directory and download URLs (ideal for tests).
    pub fn with_config(
        bin_dir: PathBuf,
        yt_dlp_url: impl Into<String>,
        ffmpeg_url: impl Into<String>,
        event_bus: Option<EventBus>,
    ) -> Self {
        Self {
            bin_dir,
            yt_dlp_url: yt_dlp_url.into(),
            ffmpeg_url: ffmpeg_url.into(),
            event_bus,
            local_only: false,
        }
    }

    /// Restrict binary resolution/ensure checks to `bin_dir` without falling back to `PATH`.
    pub fn with_local_only(mut self, local_only: bool) -> Self {
        self.local_only = local_only;
        self
    }

    pub fn bin_dir(&self) -> &Path {
        &self.bin_dir
    }

    pub fn is_local_only(&self) -> bool {
        self.local_only
    }

    pub fn locate(&self, stem: &str) -> Option<PathBuf> {
        if self.local_only {
            let local = self.bin_dir.join(exe_name(stem));
            return local.is_file().then_some(local);
        }
        locate_binary(stem, Some(&self.bin_dir))
    }

    pub fn check_status(&self) -> BinaryStatus {
        check_binaries(Some(&self.bin_dir))
    }

    fn emit_progress(
        &self,
        binary: &str,
        status: &str,
        bytes_downloaded: u64,
        total_bytes: Option<u64>,
    ) {
        if let Some(bus) = &self.event_bus {
            bus.publish(CoreEvent::BinaryInstallProgress {
                binary: binary.to_string(),
                status: status.to_string(),
                bytes_downloaded,
                total_bytes,
            });
        }
    }

    async fn download_to_temp_file(
        &self,
        binary: &str,
        url: &str,
    ) -> Result<PathBuf, BinaryError> {
        std::fs::create_dir_all(&self.bin_dir)?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .build()?;
        let tmp_path = self
            .bin_dir
            .join(format!(".{binary}-{}.tmp", uuid::Uuid::new_v4()));

        let download_fut = async {
            let resp = client.get(url).send().await?;
            if !resp.status().is_success() {
                return Err(BinaryError::Installer(format!(
                    "Failed to download '{url}': HTTP {}",
                    resp.status()
                )));
            }

            let total_bytes = resp.content_length();
            self.emit_progress(binary, "downloading", 0, total_bytes);

            let mut file = tokio::fs::File::create(&tmp_path).await?;
            let mut stream = resp.bytes_stream();
            let mut downloaded: u64 = 0;
            let mut next_emit_at: u64 = PROGRESS_EMIT_BYTES;

            while let Some(chunk_res) = stream.next().await {
                let chunk = chunk_res?;
                downloaded += chunk.len() as u64;
                file.write_all(&chunk).await?;
                if downloaded >= next_emit_at {
                    self.emit_progress(binary, "downloading", downloaded, total_bytes);
                    next_emit_at = downloaded + PROGRESS_EMIT_BYTES;
                }
            }
            file.flush().await?;

            if downloaded == 0 {
                return Err(BinaryError::Installer(format!(
                    "Downloaded empty payload from '{url}'"
                )));
            }
            self.emit_progress(binary, "downloaded", downloaded, total_bytes);
            Ok::<(), BinaryError>(())
        };

        let res = tokio::time::timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS), download_fut)
            .await
            .map_err(|_| {
                BinaryError::Installer(format!(
                    "Timed out after {DOWNLOAD_TIMEOUT_SECS}s downloading '{url}'"
                ))
            })?;

        if let Err(err) = res {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            self.emit_progress(binary, "failed", 0, None);
            return Err(err);
        }

        Ok(tmp_path)
    }

    /// Ensure `ffmpeg.exe` is available, downloading and extracting it from ZIP into
    /// `bin_dir` if missing from both `bin_dir` and `PATH`.
    pub async fn ensure_ffmpeg(&self) -> Result<PathBuf, BinaryError> {
        if let Some(existing) = self.locate("ffmpeg") {
            self.emit_progress("ffmpeg", "already_installed", 0, None);
            return Ok(existing);
        }

        let tmp_path = self.download_to_temp_file("ffmpeg", &self.ffmpeg_url).await?;
        let dest = self.bin_dir.join(exe_name("ffmpeg"));

        let mut magic = [0u8; 4];
        let is_zip = {
            let mut f = std::fs::File::open(&tmp_path)?;
            f.read_exact(&mut magic).is_ok() && magic == [0x50, 0x4B, 0x03, 0x04]
        };

        if is_zip {
            self.emit_progress("ffmpeg", "extracting", 0, None);
            let target_name = exe_name("ffmpeg");
            let tmp_clone = tmp_path.clone();
            let dest_clone = dest.clone();

            let extract_res = tokio::task::spawn_blocking(move || -> Result<(), BinaryError> {
                let file = std::fs::File::open(&tmp_clone)?;
                let mut archive = zip::ZipArchive::new(file)
                    .map_err(|e| BinaryError::Installer(format!("Invalid ffmpeg zip: {e}")))?;

                let mut extracted = false;
                for i in 0..archive.len() {
                    let mut entry = archive
                        .by_index(i)
                        .map_err(|e| BinaryError::Installer(format!("Zip entry error: {e}")))?;
                    let entry_name = entry.name().replace('\\', "/");
                    if entry_name.ends_with(&format!("/{target_name}")) || entry_name == target_name {
                        let mut out = std::fs::File::create(&dest_clone)?;
                        std::io::copy(&mut entry, &mut out)?;
                        extracted = true;
                        break;
                    }
                }
                if !extracted {
                    return Err(BinaryError::Installer(format!(
                        "'{target_name}' not found inside downloaded archive"
                    )));
                }
                Ok(())
            })
            .await
            .map_err(|e| BinaryError::Installer(format!("Zip extraction task failed: {e}")))?;

            let _ = tokio::fs::remove_file(&tmp_path).await;
            if let Err(err) = extract_res {
                self.emit_progress("ffmpeg", "failed", 0, None);
                return Err(err);
            }
        } else {
            if dest.exists() {
                let _ = tokio::fs::remove_file(&dest).await;
            }
            tokio::fs::rename(&tmp_path, &dest).await?;
        }

        let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
        self.emit_progress("ffmpeg", "installed", size, Some(size));
        Ok(dest)
    }

    /// Ensure `yt-dlp.exe` is available, downloading it into `bin_dir` if missing.
    pub async fn ensure_yt_dlp(&self) -> Result<PathBuf, BinaryError> {
        if let Some(existing) = self.locate("yt-dlp") {
            self.emit_progress("yt-dlp", "already_installed", 0, None);
            return Ok(existing);
        }

        let tmp_path = self.download_to_temp_file("yt-dlp", &self.yt_dlp_url).await?;
        let dest = self.bin_dir.join(exe_name("yt-dlp"));
        if dest.exists() {
            let _ = tokio::fs::remove_file(&dest).await;
        }
        tokio::fs::rename(&tmp_path, &dest).await?;
        let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
        self.emit_progress("yt-dlp", "installed", size, Some(size));
        Ok(dest)
    }

    /// Ensure both `yt-dlp` and `ffmpeg` are installed.
    pub async fn ensure_media_binaries(&self) -> Result<BinaryStatus, BinaryError> {
        self.ensure_yt_dlp().await?;
        self.ensure_ffmpeg().await?;
        Ok(self.check_status())
    }

    /// Add `ensure_whisper_offline` on `BinaryManager`.
    pub async fn ensure_whisper_offline(&self) -> Result<(PathBuf, PathBuf), BinaryError> {
        // Step A: Locate or download+extract `whisper-cli`
        let cli_path = if let Some(existing) = locate_whisper_cli(Some(&self.bin_dir)) {
            existing
        } else {
            let tmp_path = self.download_to_temp_file("whisper-cli", DEFAULT_WHISPER_CLI_URL).await?;
            self.emit_progress("whisper-cli", "extracting", 0, None);

            let bin_dir_clone = self.bin_dir.clone();
            let tmp_clone = tmp_path.clone();

            let extract_res = tokio::task::spawn_blocking(move || -> Result<(), BinaryError> {
                std::fs::create_dir_all(&bin_dir_clone)?;
                let file = std::fs::File::open(&tmp_clone)?;
                let mut archive = zip::ZipArchive::new(file)
                    .map_err(|e| BinaryError::Installer(format!("Invalid whisper-cli zip: {e}")))?;

                let mut main_extracted_path: Option<PathBuf> = None;

                for i in 0..archive.len() {
                    let mut entry = archive
                        .by_index(i)
                        .map_err(|e| BinaryError::Installer(format!("Zip entry error: {e}")))?;
                    if entry.is_dir() {
                        continue;
                    }
                    let entry_name = entry.name().to_string();
                    if let Some(filename) = Path::new(&entry_name).file_name() {
                        let name_str = filename.to_string_lossy().to_ascii_lowercase();
                        if name_str.ends_with(".exe") || name_str.ends_with(".dll") {
                            let dest_file = bin_dir_clone.join(filename);
                            let mut out = std::fs::File::create(&dest_file)?;
                            std::io::copy(&mut entry, &mut out)?;
                            if name_str == "main.exe" {
                                main_extracted_path = Some(dest_file);
                            }
                        }
                    }
                }

                let target_whisper_cli = bin_dir_clone.join(exe_name("whisper-cli"));
                if !target_whisper_cli.exists() {
                    if let Some(main_exe) = main_extracted_path {
                        if main_exe.is_file() {
                            std::fs::copy(&main_exe, &target_whisper_cli)?;
                        }
                    }
                }

                Ok(())
            })
            .await
            .map_err(|e| BinaryError::Installer(format!("Zip extraction task failed: {e}")))?;

            let _ = tokio::fs::remove_file(&tmp_path).await;
            if let Err(err) = extract_res {
                self.emit_progress("whisper-cli", "failed", 0, None);
                return Err(err);
            }

            let resolved_cli = locate_whisper_cli(Some(&self.bin_dir)).ok_or_else(|| {
                BinaryError::Installer("Failed to locate whisper-cli after extraction".to_string())
            })?;

            let size = std::fs::metadata(&resolved_cli).map(|m| m.len()).unwrap_or(0);
            self.emit_progress("whisper-cli", "installed", size, Some(size));
            resolved_cli
        };

        // Step B: Locate or download `ggml-base.bin` into models_dir
        let models_dir = self.bin_dir.parent().map(|p| p.join("models")).unwrap_or_else(default_models_dir);
        std::fs::create_dir_all(&models_dir)?;

        let model_path = if let Some(existing) = locate_whisper_model(Some(&models_dir)) {
            existing
        } else {
            let tmp_path = self.download_to_temp_file("whisper-model", DEFAULT_WHISPER_MODEL_URL).await?;
            let dest_model = models_dir.join("ggml-base.bin");
            if dest_model.exists() {
                let _ = tokio::fs::remove_file(&dest_model).await;
            }
            if tokio::fs::rename(&tmp_path, &dest_model).await.is_err() {
                tokio::fs::copy(&tmp_path, &dest_model).await?;
                let _ = tokio::fs::remove_file(&tmp_path).await;
            }
            let size = std::fs::metadata(&dest_model).map(|m| m.len()).unwrap_or(0);
            self.emit_progress("whisper-model", "installed", size, Some(size));
            dest_model
        };

        Ok((cli_path, model_path))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_mock_server(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut req_buf = [0u8; 1024];
                let _ = socket.read(&mut req_buf).await;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            }
        });
        format!("http://{addr}/binary")
    }

    #[test]
    fn default_bin_dir_uses_jarvis_bin() {
        let dir = default_bin_dir();
        let norm = dir.to_string_lossy().replace('\\', "/");
        assert!(norm.ends_with("Jarvis/bin"), "unexpected bin dir: {norm}");
    }

    #[test]
    fn locate_binary_prefers_bin_dir_over_path() {
        let dir = TempDir::new().unwrap();
        let custom_yt = dir.path().join(exe_name("yt-dlp"));
        let custom_agy = dir.path().join(exe_name("agy"));
        std::fs::write(&custom_yt, b"dummy").unwrap();
        std::fs::write(&custom_agy, b"dummy").unwrap();

        let found_yt = locate_binary("yt-dlp", Some(dir.path())).unwrap();
        let found_agy = locate_binary("agy", Some(dir.path())).unwrap();
        assert_eq!(found_yt, custom_yt);
        assert_eq!(found_agy, custom_agy);

        let status = check_binaries(Some(dir.path()));
        assert!(status.yt_dlp_installed);
        assert!(status.agy_installed);
        assert_eq!(status.bin_dir, dir.path());
    }

    #[test]
    fn binary_status_round_trip_serialization() {
        let status = BinaryStatus {
            bin_dir: PathBuf::from("C:/Jarvis/bin"),
            yt_dlp_installed: true,
            yt_dlp_path: Some(PathBuf::from("C:/Jarvis/bin/yt-dlp.exe")),
            ffmpeg_installed: false,
            ffmpeg_path: None,
            agy_installed: true,
            agy_path: Some(PathBuf::from("C:/Jarvis/bin/agy.exe")),
            whisper_cli_installed: false,
            whisper_cli_path: None,
            whisper_model_installed: true,
            whisper_model_path: Some(PathBuf::from("C:/Jarvis/models/ggml-base.bin")),
        };

        let json = serde_json::to_string(&status).unwrap();
        let decoded: BinaryStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, status);
    }

    #[tokio::test]
    async fn ensure_ffmpeg_downloads_and_extracts_zip_with_progress_events() {
        let temp_dir = TempDir::new().unwrap();
        let bus = EventBus::new(32);
        let mut rx = bus.subscribe();

        let mut zip_buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut zip_buf));
            let options = zip::write::SimpleFileOptions::default();
            writer
                .start_file(format!("ffmpeg-master/bin/{}", exe_name("ffmpeg")), options)
                .unwrap();
            writer.write_all(b"MOCK-FFMPEG-EXE-PAYLOAD").unwrap();
            writer.finish().unwrap();
        }
        let ff_url = spawn_mock_server(zip_buf).await;
        let yt_url = spawn_mock_server(b"MOCK-YTDLP".to_vec()).await;

        let mgr = BinaryManager::with_config(
            temp_dir.path().to_path_buf(),
            yt_url,
            ff_url,
            Some(bus),
        )
        .with_local_only(true);

        let ff_path = mgr.ensure_ffmpeg().await.unwrap();
        assert!(ff_path.is_file());
        assert_eq!(std::fs::read(&ff_path).unwrap(), b"MOCK-FFMPEG-EXE-PAYLOAD");

        let yt_path = mgr.ensure_yt_dlp().await.unwrap();
        assert!(yt_path.is_file());
        assert_eq!(std::fs::read(&yt_path).unwrap(), b"MOCK-YTDLP");

        let mut statuses = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let CoreEvent::BinaryInstallProgress { binary, status, .. } = ev {
                if binary == "ffmpeg" {
                    statuses.push(status);
                }
            }
        }
        assert!(statuses.contains(&"downloading".to_string()));
        assert!(statuses.contains(&"extracting".to_string()));
        assert!(statuses.contains(&"installed".to_string()));
    }
}
