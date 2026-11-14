//! Checking GitHub releases and replacing the running executable.
//!
//! The check and download run on a worker thread. Installation writes a
//! temporary file next to the current executable, verifies its file magic,
//! makes it executable, and renames it into place. On Unix the running process
//! keeps its old inode; on Windows the old executable is renamed aside and
//! removed on the next start.

use std::cmp::Ordering;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

const RELEASES_URL: &str =
    "https://api.github.com/repos/georgewoodall82/soulsucker/releases/latest";
const DOWNLOAD_PREFIX: &str = "https://github.com/georgewoodall82/soulsucker/releases/download/";
const USER_AGENT: &str = concat!(
    "soulsucker/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/georgewoodall82/soulsucker)"
);
const MAX_DOWNLOAD_BYTES: u64 = 200 * 1024 * 1024;

static HTTP: OnceLock<ureq::Agent> = OnceLock::new();

fn http() -> &'static ureq::Agent {
    HTTP.get_or_init(|| {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(300)))
            .user_agent(USER_AGENT)
            .build();
        ureq::Agent::new_with_config(config)
    })
}

#[derive(Debug, Clone)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct LatestRelease {
    pub tag: String,
    pub version: String,
    pub html_url: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone)]
pub enum UpdateOutcome {
    UpToDate { current: String, latest: String },
    NewerLocal { current: String, latest: String },
    Updated { version: String },
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

pub fn latest_release() -> Result<LatestRelease> {
    let mut response = http()
        .get(RELEASES_URL)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|err| anyhow!(friendly_http_error(&err)))?;
    let release: ApiRelease = response
        .body_mut()
        .read_json()
        .context("could not parse the GitHub release response")?;
    let version = normalize_version(&release.tag_name).ok_or_else(|| {
        anyhow!(
            "GitHub release has an unrecognized version tag: {}",
            release.tag_name
        )
    })?;
    Ok(LatestRelease {
        tag: release.tag_name,
        version,
        html_url: release.html_url,
        assets: release
            .assets
            .into_iter()
            .map(|asset| Asset {
                name: asset.name,
                url: asset.browser_download_url,
                size: asset.size,
            })
            .collect(),
    })
}

fn friendly_http_error(err: &ureq::Error) -> String {
    match err {
        ureq::Error::StatusCode(404) => "no GitHub release found".to_string(),
        ureq::Error::StatusCode(403 | 429) => {
            "GitHub API rate limit reached; try again later".to_string()
        }
        ureq::Error::Timeout(_) => "GitHub timed out".to_string(),
        other => format!("GitHub request failed: {other}"),
    }
}

pub fn compare_versions(latest: &str, current: &str) -> Result<Ordering> {
    let latest =
        parse_version(latest).ok_or_else(|| anyhow!("unrecognized latest version: {latest}"))?;
    let current =
        parse_version(current).ok_or_else(|| anyhow!("unrecognized current version: {current}"))?;
    Ok(latest.cmp(&current))
}

fn normalize_version(tag: &str) -> Option<String> {
    let version = tag
        .trim()
        .strip_prefix('v')
        .or_else(|| tag.trim().strip_prefix('V'))
        .unwrap_or_else(|| tag.trim());
    parse_version(version)?;
    Some(version.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

fn parse_version(input: &str) -> Option<Version> {
    let input = input.trim();
    let input = input
        .strip_prefix('v')
        .or_else(|| input.strip_prefix('V'))
        .unwrap_or(input);
    let core = input.split(['-', '+']).next()?.trim();
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(Version {
        major,
        minor,
        patch,
    })
}

pub fn platform_triple() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") if cfg!(target_env = "gnu") => Ok("x86_64-unknown-linux-gnu"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        (os, arch) => Err(anyhow!(
            "automatic updates are not available for {os}/{arch}"
        )),
    }
}

pub fn platform_asset(release: &LatestRelease) -> Result<&Asset> {
    let triple = platform_triple()?;
    asset_for_triple(release, triple)
}

fn asset_for_triple<'a>(release: &'a LatestRelease, triple: &str) -> Result<&'a Asset> {
    let suffix = format!("-{triple}{}", std::env::consts::EXE_SUFFIX);
    release
        .assets
        .iter()
        .find(|asset| asset.name.starts_with("soulsucker-") && asset.name.ends_with(&suffix))
        .ok_or_else(|| {
            anyhow!(
                "release {} has no automatic-update binary for {triple}; download it from {}",
                release.tag,
                release.html_url
            )
        })
}

pub fn install(asset: &Asset) -> Result<PathBuf> {
    if !asset.url.starts_with(DOWNLOAD_PREFIX) {
        bail!("unexpected update download URL: {}", asset.url);
    }
    let exe = std::env::current_exe().context("could not locate the running executable")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("the running executable has no parent folder"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temp = dir.join(format!(
        ".soulsucker-update-{}-{nonce}.tmp",
        std::process::id()
    ));
    let result = download(asset, &temp).and_then(|downloaded| {
        if asset.size > 0 && downloaded != asset.size {
            bail!(
                "downloaded {downloaded} bytes but the release lists {}",
                asset.size
            );
        }
        verify_binary(&temp)?;
        set_executable(&temp)?;
        replace_executable(&temp, &exe)?;
        Ok(exe.clone())
    });
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn download(asset: &Asset, path: &Path) -> Result<u64> {
    let mut response = http()
        .get(&asset.url)
        .call()
        .map_err(|err| anyhow!(friendly_http_error(&err)))?;
    let mut reader = response.body_mut().as_reader();
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("could not create {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .context("could not read the update download")?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > MAX_DOWNLOAD_BYTES {
            bail!("the update download is larger than {MAX_DOWNLOAD_BYTES} bytes");
        }
        writer
            .write_all(&buffer[..read])
            .context("could not write the update file")?;
    }
    writer.flush().context("could not flush the update file")?;
    writer
        .get_ref()
        .sync_all()
        .context("could not sync the update file")?;
    Ok(total)
}

fn verify_binary(path: &Path) -> Result<()> {
    let mut file =
        File::open(path).with_context(|| format!("could not read {}", path.display()))?;
    let mut magic = [0u8; 4];
    let read = file
        .read(&mut magic)
        .context("could not read the update file header")?;
    if read < 2 || !executable_magic(&magic[..read]) {
        bail!(
            "the downloaded file does not look like a {} executable",
            std::env::consts::OS
        );
    }
    Ok(())
}

fn executable_magic(bytes: &[u8]) -> bool {
    #[cfg(target_os = "linux")]
    {
        bytes.starts_with(b"\x7fELF")
    }
    #[cfg(target_os = "macos")]
    {
        if bytes.len() < 4 {
            return false;
        }
        let magic = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        matches!(
            magic,
            0xfeed_face
                | 0xcefa_edfe
                | 0xfeed_facf
                | 0xcffa_edfe
                | 0xcafe_babe
                | 0xbeba_feca
                | 0xcafe_babf
                | 0xbfba_feca
        )
    }
    #[cfg(target_os = "windows")]
    {
        bytes.starts_with(b"MZ")
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        !bytes.is_empty()
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("could not make {} executable", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn replace_executable(new: &Path, exe: &Path) -> Result<()> {
    fs::rename(new, exe)
        .with_context(|| format!("could not replace {}; check its permissions", exe.display()))
}

#[cfg(windows)]
fn replace_executable(new: &Path, exe: &Path) -> Result<()> {
    let name = exe
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("the running executable has an unusual name"))?;
    let backup = exe.with_file_name(format!("{name}.old"));
    let _ = fs::remove_file(&backup);
    fs::rename(exe, &backup)
        .with_context(|| format!("could not move the old executable aside: {}", exe.display()))?;
    if let Err(err) = fs::rename(new, exe) {
        let _ = fs::rename(&backup, exe);
        return Err(err).with_context(|| {
            format!("could not replace {}; check its permissions", exe.display())
        });
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn replace_executable(new: &Path, exe: &Path) -> Result<()> {
    fs::rename(new, exe)
        .with_context(|| format!("could not replace {}; check its permissions", exe.display()))
}

/// Remove the previous executable left behind by a Windows update.
pub fn cleanup_old_binary() {
    #[cfg(windows)]
    {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let Some(name) = exe.file_name().and_then(|name| name.to_str()) else {
            return;
        };
        let _ = fs::remove_file(exe.with_file_name(format!("{name}.old")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(names: &[&str]) -> LatestRelease {
        LatestRelease {
            tag: "v0.2.0".into(),
            version: "0.2.0".into(),
            html_url: "https://example.test/release".into(),
            assets: names
                .iter()
                .map(|name| Asset {
                    name: (*name).into(),
                    url: format!("{DOWNLOAD_PREFIX}v0.2.0/{name}"),
                    size: 1,
                })
                .collect(),
        }
    }

    #[test]
    fn versions_compare_semantically() {
        assert_eq!(
            compare_versions("v0.2.0", "0.1.9").unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions("0.1.0", "v0.1.0").unwrap(),
            Ordering::Equal
        );
        assert_eq!(
            compare_versions("1.0.0", "0.99.99").unwrap(),
            Ordering::Greater
        );
        assert_eq!(compare_versions("0.1.0", "0.2.0").unwrap(), Ordering::Less);
        assert_eq!(compare_versions("0.2", "0.2.0").unwrap(), Ordering::Equal);
        assert!(compare_versions("nightly", "0.1.0").is_err());
    }

    #[test]
    fn platform_asset_ignores_archives() {
        let release = release(&[
            "soulsucker-v0.2.0-x86_64-unknown-linux-gnu.tar.gz",
            "soulsucker-v0.2.0-x86_64-unknown-linux-gnu",
        ]);
        let asset = asset_for_triple(&release, "x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(asset.name, "soulsucker-v0.2.0-x86_64-unknown-linux-gnu");
    }

    #[test]
    fn a_missing_platform_asset_is_an_error() {
        let release = release(&["soulsucker-v0.2.0-aarch64-apple-darwin"]);
        let error = asset_for_triple(&release, "x86_64-unknown-linux-gnu").unwrap_err();
        assert!(error.to_string().contains("no automatic-update binary"));
    }

    #[cfg(unix)]
    #[test]
    fn replacement_moves_the_new_binary_into_place() {
        let dir = std::env::temp_dir().join(format!(
            "soulsucker-update-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("soulsucker");
        let new = dir.join("new");
        fs::write(&target, b"old").unwrap();
        fs::write(&new, b"new").unwrap();

        replace_executable(&new, &target).unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert!(!new.exists());
        let _ = fs::remove_dir_all(dir);
    }
}
