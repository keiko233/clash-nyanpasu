use std::{collections::HashMap, path::Path, sync::OnceLock};

use crate::config::ClashCore;
use anyhow::{anyhow, Result};
use gunzip::Decompressor;
use log::debug;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use tempfile::{tempdir, TempDir};
use tokio::{sync::RwLock, task::spawn_blocking};

#[cfg(target_family = "unix")]
use std::os::unix::fs::PermissionsExt;

use super::CoreManager;

pub struct Updater {
    manifest_version: ManifestVersion,
    mirror: String,
}

impl Default for Updater {
    fn default() -> Self {
        Self {
            manifest_version: ManifestVersion::default(),
            mirror: "https://mirror.ghproxy.com".to_string(),
        }
    }
}

#[derive(Deserialize, Serialize, Clone)]
pub struct ManifestVersion {
    manifest_version: u64,
    latest: ManifestVersionLatest,
    arch_template: ArchTemplate,
    updated_at: String,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct ManifestVersionLatest {
    mihomo: String,
    mihomo_alpha: String,
    clash_rs: String,
    clash_premium: String,
}

#[derive(Deserialize, Serialize, Default, Clone)]
pub struct ArchTemplate {
    mihomo: HashMap<String, String>,
    mihomo_alpha: HashMap<String, String>,
    clash_rs: HashMap<String, String>,
    clash_premium: HashMap<String, String>,
}

impl Default for ManifestVersion {
    fn default() -> Self {
        Self {
            manifest_version: 0,
            latest: ManifestVersionLatest::default(),
            arch_template: ArchTemplate::default(),
            updated_at: "".to_string(),
        }
    }
}

impl Default for ManifestVersionLatest {
    fn default() -> Self {
        Self {
            mihomo: "".to_string(),
            mihomo_alpha: "".to_string(),
            clash_rs: "".to_string(),
            clash_premium: "".to_string(),
        }
    }
}

fn get_arch() -> Result<&'static str> {
    let env = {
        let arch = std::env::consts::ARCH;
        let os = std::env::consts::OS;
        (arch, os)
    };

    match env {
        ("x86_64", "macos") => Ok("darwin-x64"),
        ("x86_64", "linux") => Ok("linux-amd64"),
        ("x86_64", "windows") => Ok("windows-x86_64"),
        ("aarch64", "macos") => Ok("darwin-arm64"),
        ("aarch64", "linux") => Ok("linux-aarch64"),
        // ("aarch64", "windows") => Ok("windows-arm64"),
        _ => anyhow::bail!("unsupported platform"),
    }
}

impl Updater {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn global() -> &'static RwLock<Self> {
        static INSTANCE: OnceLock<RwLock<Updater>> = OnceLock::new();
        INSTANCE.get_or_init(|| RwLock::new(Updater::new()))
    }

    pub fn get_latest_versions(&self) -> ManifestVersionLatest {
        self.manifest_version.latest.clone()
    }

    pub async fn fetch_latest(&mut self) -> Result<()> {
        let latest = get_latest_version_manifest(self.mirror.as_str()).await?;
        self.manifest_version = latest;
        Ok(())
    }

    pub async fn update_core(&self, core_type: &ClashCore) -> Result<()> {
        let current_core = crate::config::Config::verge()
            .latest()
            .clash_core
            .clone()
            .unwrap_or_default();
        let tmp_dir = tempdir()?;
        // 1. download core
        let artifact = self.download_core(core_type, &tmp_dir).await?;
        // 2. decompress core
        let core_type_ref = core_type.clone();
        let tmp_dir_path = tmp_dir.path().to_owned();
        let artifact_ref = artifact.clone();
        spawn_blocking(move || {
            decompress_and_set_permission(&core_type_ref, &tmp_dir_path, &artifact_ref)
        })
        .await??;
        // 3. if core is used, close it
        if current_core == *core_type {
            CoreManager::global().stop_core()?;
        }
        // 4. replace core
        #[cfg(target_os = "windows")]
        let target_core = format!("{}.exe", core_type);
        #[cfg(not(target_os = "windows"))]
        let target_core = core_type.clone().to_string();
        let core_dir = tauri::utils::platform::current_exe()?;
        let core_dir = core_dir.parent().ok_or(anyhow!("failed to get core dir"))?;
        let target_core = core_dir.join(target_core);
        std::fs::copy(tmp_dir.path().join(&artifact), target_core)?;

        // 5. if core is used before, restart it
        if current_core == *core_type {
            CoreManager::global().run_core().await?;
        }
        Ok(())
    }

    async fn download_core(&self, core_type: &ClashCore, tmp_dir: &TempDir) -> Result<String> {
        let arch = get_arch()?;
        let version_manifest = &self.manifest_version;
        let (artifact, core_type_meta) = match core_type {
            ClashCore::ClashPremium => (
                version_manifest
                    .arch_template
                    .clash_premium
                    .get(arch)
                    .ok_or(anyhow!("invalid arch"))?
                    .clone()
                    .replace("{}", &version_manifest.latest.clash_premium),
                CoreTypeMeta::ClashPremium(version_manifest.latest.clash_premium.clone()),
            ),
            ClashCore::Mihomo => (
                version_manifest
                    .arch_template
                    .mihomo
                    .get(arch)
                    .ok_or(anyhow!("invalid arch"))?
                    .clone()
                    .replace("{}", &version_manifest.latest.mihomo),
                CoreTypeMeta::Mihomo(version_manifest.latest.mihomo.clone()),
            ),
            ClashCore::MihomoAlpha => (
                version_manifest
                    .arch_template
                    .mihomo_alpha
                    .get(arch)
                    .ok_or(anyhow!("invalid arch"))?
                    .clone()
                    .replace("{}", &version_manifest.latest.mihomo_alpha),
                CoreTypeMeta::MihomoAlpha,
            ),
            ClashCore::ClashRs => (
                version_manifest
                    .arch_template
                    .clash_rs
                    .get(arch)
                    .ok_or(anyhow!("invalid arch"))?
                    .clone()
                    .replace("{}", &version_manifest.latest.clash_rs),
                CoreTypeMeta::ClashRs(version_manifest.latest.clash_rs.clone()),
            ),
        };
        let url = format!(
            "{}/{}",
            &self.mirror,
            get_download_path(core_type_meta, artifact.clone())
        );
        let file_path = tmp_dir.path().join(&artifact);
        let mut dst = std::fs::File::create(&file_path)?;

        let client = reqwest::Client::new();
        let buff = client
            .get(format!("{}/{}", url, core_type))
            .send()
            .await?
            .text()
            .await?;
        std::io::copy(&mut buff.as_bytes(), &mut dst)?;
        Ok(artifact)
    }
}

fn decompress_and_set_permission(
    core_type: &ClashCore,
    tmp_path: &Path,
    fname: &str,
) -> Result<()> {
    let mut buff = Vec::<u8>::new();
    let path = tmp_path.join(fname);
    let mut file = std::fs::File::open(path)?;
    match fname {
        fname if fname.ends_with(".gz") => {
            let mut decompressor = Decompressor::new(file, true);
            std::io::copy(&mut decompressor, &mut buff)?;
        }
        fname if fname.ends_with(".zip") => {
            let mut archive = zip::ZipArchive::new(file)?;
            for i in 0..archive.len() {
                let mut file = archive.by_index(i)?;
                let file_name = file.name();
                debug!("Filename: {}", file.name());
                // TODO: 在 enum 做点魔法
                if file_name.contains("mihomo") || file_name.contains("clash") {
                    std::io::copy(&mut file, &mut buff)?;
                    break;
                }
            }
            anyhow::bail!("failed to find core file in a zip archive");
        }
        _ => {
            std::io::copy(&mut file, &mut buff)?;
        }
    };
    let tmp_core = tmp_path.join(core_type.clone().to_string());
    let mut core_file = std::fs::File::create(tmp_core)?;
    std::io::copy(&mut buff.as_slice(), &mut core_file)?;
    drop(core_file); // release the file handle
    #[cfg(target_family = "unix")]
    {
        std::fs::set_permissions(&tmp_core, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

pub async fn get_latest_version_manifest(mirror: &str) -> Result<ManifestVersion> {
    let url = format!(
        "{}/greenhat616/clash-nyanpasu/raw/dev/manifest/version.json",
        mirror
    );
    let client = reqwest::Client::new();
    Ok(client
        .get(url)
        .send()
        .await?
        .json::<ManifestVersion>()
        .await?)
}

enum CoreTypeMeta {
    ClashPremium(String),
    Mihomo(String),
    MihomoAlpha,
    ClashRs(String),
}

pub fn get_download_path(core_type: CoreTypeMeta, artifact: String) -> String {
    match core_type {
        CoreTypeMeta::Mihomo(tag) => {
            format!("MetaCubeX/mihomo/releases/download/{}/{}", tag, artifact)
        }
        CoreTypeMeta::MihomoAlpha => format!(
            "MetaCubeX/mihomo/releases/download/Prerelease-Alpha/{}",
            artifact
        ),
        CoreTypeMeta::ClashRs(tag) => {
            format!("Watfaq/clash-rs/releases/download/{}/{}", tag, artifact)
        }
        CoreTypeMeta::ClashPremium(tag) => format!(
            "zhongfly/Clash-premium-backup/releases/download/{}/{}",
            tag, artifact
        ),
    }
}