//! GitHub Release metadata and Debian package download helpers.

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use reqwest::{header, Client};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::{fs::File, io::AsyncWriteExt};

#[derive(Debug, Clone, PartialEq, Eq)]
/// GitHub Release Debian asset selected for the current system architecture.
pub struct ReleaseDebAsset {
    pub release_tag: String,
    pub release_name: Option<String>,
    pub asset_name: String,
    pub download_url: String,
    pub size: Option<u64>,
    pub candidate_version: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Downloaded Debian package selected from a GitHub Release.
pub struct DownloadedReleaseDeb {
    pub path: PathBuf,
    pub sha256: String,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    name: Option<String>,
    assets: Vec<GitHubReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubReleaseAsset {
    name: String,
    browser_download_url: String,
    size: Option<u64>,
}

/// Fetches the latest GitHub Release and selects a .deb for the current Debian architecture.
pub async fn fetch_latest_deb_asset(
    client: &Client,
    release_api_url: &str,
) -> Result<Option<ReleaseDebAsset>> {
    let release = client
        .get(release_api_url)
        .header(header::USER_AGENT, "codex-update-manager")
        .header(header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("Failed GET request for {release_api_url}"))?
        .error_for_status()
        .with_context(|| format!("GET request for {release_api_url} returned an error status"))?
        .json::<GitHubRelease>()
        .await
        .with_context(|| {
            format!("Failed to parse GitHub Release metadata from {release_api_url}")
        })?;

    let arch = debian_arch()?;
    Ok(select_deb_asset(release, &arch))
}

fn select_deb_asset(release: GitHubRelease, arch: &str) -> Option<ReleaseDebAsset> {
    let arch_suffix = format!("_{arch}.deb");
    let asset = release.assets.into_iter().find(|asset| {
        asset.name.ends_with(&arch_suffix)
            && asset.name.starts_with("codex-desktop_")
            && asset.name.len() > "codex-desktop_".len() + arch_suffix.len()
    })?;

    let candidate_version = asset
        .name
        .strip_prefix("codex-desktop_")?
        .strip_suffix(&arch_suffix)?
        .to_string();
    let fingerprint = format!(
        "github_release_tag={}|asset_name={}|asset_size={}",
        release.tag_name,
        asset.name,
        asset
            .size
            .map(|value| value.to_string())
            .as_deref()
            .unwrap_or("")
    );

    Some(ReleaseDebAsset {
        release_tag: release.tag_name,
        release_name: release.name,
        asset_name: asset.name,
        download_url: asset.browser_download_url,
        size: asset.size,
        candidate_version,
        fingerprint,
    })
}

fn debian_arch() -> Result<String> {
    let output = std::process::Command::new("dpkg")
        .arg("--print-architecture")
        .output()
        .context("Failed to detect Debian architecture with dpkg --print-architecture")?;
    anyhow::ensure!(
        output.status.success(),
        "dpkg --print-architecture exited with {}",
        output.status
    );

    let arch = String::from_utf8(output.stdout)
        .context("dpkg --print-architecture returned non-UTF8 output")?
        .trim()
        .to_string();
    match arch.as_str() {
        "amd64" | "arm64" => Ok(arch),
        _ => Err(anyhow!(
            "Unsupported Debian release asset architecture: {arch}"
        )),
    }
}

/// Downloads the selected Release asset and returns its local path and sha256.
pub async fn download_deb_asset(
    client: &Client,
    asset: &ReleaseDebAsset,
    destination_dir: &Path,
) -> Result<DownloadedReleaseDeb> {
    tokio::fs::create_dir_all(destination_dir)
        .await
        .with_context(|| format!("Failed to create {}", destination_dir.display()))?;

    let destination = destination_dir.join(&asset.asset_name);
    let mut file = File::create(&destination)
        .await
        .with_context(|| format!("Failed to create {}", destination.display()))?;

    let response = client
        .get(&asset.download_url)
        .header(header::USER_AGENT, "codex-update-manager")
        .send()
        .await
        .with_context(|| format!("Failed GET request for {}", asset.download_url))?
        .error_for_status()
        .with_context(|| {
            format!(
                "GET request for Release asset {} returned an error status",
                asset.download_url
            )
        })?;

    let mut hasher = Sha256::new();
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("Failed downloading {}", asset.download_url))?;
        file.write_all(&chunk)
            .await
            .with_context(|| format!("Failed writing {}", destination.display()))?;
        hasher.update(&chunk);
    }

    file.flush()
        .await
        .with_context(|| format!("Failed flushing {}", destination.display()))?;

    let sha256 = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    Ok(DownloadedReleaseDeb {
        path: destination,
        sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_matching_deb_asset() {
        let release = GitHubRelease {
            tag_name: "release-20260522-131557".to_string(),
            name: Some("2026.05.22.131557".to_string()),
            assets: vec![
                GitHubReleaseAsset {
                    name: "codex-desktop_2026.05.22.131557_arm64.deb".to_string(),
                    browser_download_url: "https://example.com/arm64.deb".to_string(),
                    size: Some(10),
                },
                GitHubReleaseAsset {
                    name: "codex-desktop_2026.05.22.131557_amd64.deb".to_string(),
                    browser_download_url: "https://example.com/amd64.deb".to_string(),
                    size: Some(20),
                },
            ],
        };

        let asset = select_deb_asset(release, "amd64").expect("asset should match");
        assert_eq!(
            asset.asset_name,
            "codex-desktop_2026.05.22.131557_amd64.deb"
        );
        assert_eq!(asset.candidate_version, "2026.05.22.131557");
        assert!(asset.fingerprint.contains("release-20260522-131557"));
    }

    #[test]
    fn ignores_non_matching_assets() {
        let release = GitHubRelease {
            tag_name: "release-20260522-131557".to_string(),
            name: None,
            assets: vec![GitHubReleaseAsset {
                name: "codex-desktop_2026.05.22.131557_arm64.deb".to_string(),
                browser_download_url: "https://example.com/arm64.deb".to_string(),
                size: None,
            }],
        };

        assert!(select_deb_asset(release, "amd64").is_none());
    }
}
