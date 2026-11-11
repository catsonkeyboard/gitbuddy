//! Public-release metadata with explicit origin and digest validation.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

/// Cancellation stops a download before its verified package is published.
#[derive(Clone, Debug, Default)]
pub struct DownloadCancellation(Arc<AtomicU8>);
impl DownloadCancellation {
    pub fn cancel(&self) -> bool {
        self.0
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    fn check(&self) -> Result<()> {
        ensure!(
            self.0.load(Ordering::Acquire) != 1,
            "Update download cancelled"
        );
        Ok(())
    }
    fn begin_publication(&self) -> Result<()> {
        self.0
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow::anyhow!("Update download cancelled or already published"))?;
        Ok(())
    }
}

const RELEASE_ROOT: &str = "https://github.com/catsonkeyboard/gitbuddy/releases/";
fn release_url(text: &str, kind: &str) -> Result<String> {
    ensure!(
        !text.chars().any(|c| c.is_control() || c == '\\'),
        "Invalid release URL"
    );
    let url = reqwest::Url::parse(text)?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("github.com")
            && url.username().is_empty()
            && url.password().is_none()
            && url.port_or_known_default() == Some(443),
        "Unexpected release origin"
    );
    let path = url.path().to_ascii_lowercase();
    ensure!(
        url.query().is_none()
            && url.fragment().is_none()
            && url
                .path()
                .starts_with(&format!("/catsonkeyboard/gitbuddy/releases/{kind}/"))
            && !path.contains("%2f")
            && !path.contains("%5c")
            && !path.contains("%00"),
        "Unexpected release path"
    );
    Ok(url.to_string())
}
#[derive(Clone, Debug)]
pub struct UpdateAsset {
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}
#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub version: String,
    pub release_url: String,
    pub asset: Option<UpdateAsset>,
}
#[derive(Deserialize)]
struct Release {
    tag_name: String,
    prerelease: bool,
    draft: bool,
    html_url: String,
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
    size: u64,
}
#[derive(Clone, Debug)]
pub enum UpdateCheck {
    NoReleases,
    Current,
    Available(UpdateInfo),
}
impl UpdateInfo {
    pub fn parse(current: &str, bytes: &[u8], os: &str, arch: &str) -> Result<Option<Self>> {
        ensure!(
            bytes.len() <= 2 * 1024 * 1024,
            "Release metadata exceeds 2 MiB"
        );
        let release: Release = serde_json::from_slice(bytes)?;
        if release.draft || release.prerelease {
            return Ok(None);
        }
        let version = semver::Version::parse(
            release
                .tag_name
                .strip_prefix('v')
                .unwrap_or(&release.tag_name),
        )?;
        if !version.pre.is_empty() || version <= semver::Version::parse(current)? {
            return Ok(None);
        }
        let page_url = release_url(&release.html_url, "tag")?;
        let fallback = [arch];
        let aliases: &[&str] = match arch {
            "x86_64" | "amd64" => &["x86_64", "amd64", "x64"],
            "aarch64" | "arm64" => &["aarch64", "arm64"],
            _ => &fallback,
        };
        let os = if os == "darwin" { "macos" } else { os };
        let mut selected = None;
        for asset in release.assets {
            let name = asset.name.to_ascii_lowercase();
            if !name.contains(os)
                || !aliases.iter().any(|alias| name.contains(alias))
                || !(name.ends_with(".zip") || name.ends_with(".tar.gz"))
            {
                continue;
            }
            let asset_url = release_url(&asset.browser_download_url, "download")?;
            let Some(digest) = asset
                .digest
                .and_then(|s| s.strip_prefix("sha256:").map(str::to_owned))
            else {
                continue;
            };
            ensure!(
                digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
                "Invalid release digest"
            );
            ensure!(
                asset.size > 0 && asset.size <= 1024 * 1024 * 1024,
                "Release asset exceeds 1 GiB or is empty"
            );
            ensure!(
                selected.is_none(),
                "Ambiguous release packages for this platform"
            );
            selected = Some(UpdateAsset {
                name: asset.name,
                url: asset_url,
                sha256: digest.to_ascii_lowercase(),
                size: asset.size,
            });
        }
        Ok(Some(Self {
            version: version.to_string(),
            release_url: page_url,
            asset: selected,
        }))
    }
}
fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent(concat!("GitBuddy/", env!("CARGO_PKG_VERSION")))
        .build()?)
}
pub fn check_updates() -> Result<UpdateCheck> {
    let response = client()?
        .get("https://api.github.com/repos/catsonkeyboard/gitbuddy/releases/latest")
        .header("Accept", "application/vnd.github+json")
        .send()
        .context("Cannot query published GitBuddy releases")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(UpdateCheck::NoReleases);
    }
    let mut bytes = Vec::new();
    response
        .error_for_status()?
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    Ok(
        match UpdateInfo::parse(
            env!("CARGO_PKG_VERSION"),
            &bytes,
            std::env::consts::OS,
            std::env::consts::ARCH,
        )? {
            Some(update) => UpdateCheck::Available(update),
            None => UpdateCheck::Current,
        },
    )
}
pub fn download_update(asset: &UpdateAsset, destination: &std::path::Path) -> Result<()> {
    download_update_with_cancel(asset, destination, &DownloadCancellation::default())
}
pub fn download_update_with_cancel(
    asset: &UpdateAsset,
    destination: &std::path::Path,
    cancellation: &DownloadCancellation,
) -> Result<()> {
    cancellation.check()?;
    let url = release_url(&asset.url, "download")?;
    ensure!(
        asset.url.starts_with(&format!("{RELEASE_ROOT}download/")) && asset.sha256.len() == 64,
        "Invalid release asset"
    );
    let response = client()?.get(url).send();
    cancellation.check()?;
    store_verified_download_controlled(
        response?.error_for_status()?,
        asset,
        destination,
        cancellation,
    )
}
#[cfg(test)]
fn store_verified_download(
    response: impl Read,
    asset: &UpdateAsset,
    destination: &std::path::Path,
) -> Result<()> {
    store_verified_download_controlled(
        response,
        asset,
        destination,
        &DownloadCancellation::default(),
    )
}
fn store_verified_download_controlled(
    mut response: impl Read,
    asset: &UpdateAsset,
    destination: &std::path::Path,
    cancellation: &DownloadCancellation,
) -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::io::Write;
    ensure!(
        asset.url.starts_with(&format!("{RELEASE_ROOT}download/")) && asset.sha256.len() == 64,
        "Invalid release asset"
    );
    cancellation.check()?;
    let mut temporary = tempfile::NamedTempFile::new_in(
        destination.parent().context("Missing destination parent")?,
    )?;
    let mut digest = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        cancellation.check()?;
        let read = response.read(&mut buffer);
        cancellation.check()?;
        let n = read?;
        if n == 0 {
            break;
        }
        count += n as u64;
        ensure!(
            count <= asset.size && count <= 1024 * 1024 * 1024,
            "Downloaded asset exceeded expected size"
        );
        temporary.write_all(&buffer[..n])?;
        digest.update(&buffer[..n]);
    }
    ensure!(
        count == asset.size && format!("{:x}", digest.finalize()) == asset.sha256,
        "Downloaded update failed its size or SHA-256 check"
    );
    temporary.as_file().sync_all()?;
    cancellation.begin_publication()?;
    temporary
        .persist_noclobber(destination)
        .map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn downloaded_bytes_require_digest_and_size_and_never_overwrite_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("package.zip");
        let asset = UpdateAsset {
            name: "package.zip".into(),
            url: format!("{RELEASE_ROOT}download/v1/package.zip"),
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
            size: 3,
        };
        store_verified_download(std::io::Cursor::new(b"abc"), &asset, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"abc");
        std::fs::write(&target, b"sentinel").unwrap();
        assert!(store_verified_download(std::io::Cursor::new(b"abc"), &asset, &target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"sentinel");
        let other = dir.path().join("other.zip");
        assert!(store_verified_download(std::io::Cursor::new(b"abd"), &asset, &other).is_err());
        assert!(!other.exists());
        assert!(store_verified_download(std::io::Cursor::new(b"abcd"), &asset, &other).is_err());
        assert!(!other.exists());
    }

    #[test]
    fn cancelled_download_never_publishes_or_leaves_temporary_bytes() {
        struct CancelAtEof {
            bytes: std::io::Cursor<&'static [u8]>,
            cancellation: DownloadCancellation,
        }
        impl Read for CancelAtEof {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let n = self.bytes.read(buffer)?;
                if n == 0 {
                    self.cancellation.cancel();
                }
                Ok(n)
            }
        }
        let asset = UpdateAsset {
            name: "package.zip".into(),
            url: format!("{RELEASE_ROOT}download/v1/package.zip"),
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
            size: 3,
        };
        for precancelled in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("package.zip");
            let cancellation = DownloadCancellation::default();
            if precancelled {
                cancellation.cancel();
            }
            let reader = CancelAtEof {
                bytes: std::io::Cursor::new(b"abc"),
                cancellation: cancellation.clone(),
            };
            assert!(
                store_verified_download_controlled(reader, &asset, &target, &cancellation).is_err()
            );
            assert!(!target.exists());
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        }
    }
}
