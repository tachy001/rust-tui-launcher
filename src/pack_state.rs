use crate::{
    backend::Backend,
    config::Config,
    download::{valid_file, Download},
    metadata::{safe_path, valid_id},
    storage::{atomic_copy, atomic_write},
    task::TaskControl,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

pub const RECEIPT: &str = "tui-mc-pack-receipt.json";
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackedFile {
    pub path: String,
    pub sha1: String,
    pub size: u64,
    #[serde(default)]
    pub urls: Vec<String>,
    #[serde(default)]
    pub member: Option<usize>,
}
#[derive(Serialize, Deserialize)]
pub struct Receipt {
    pub schema: u32,
    pub instance: String,
    pub archive_sha1: String,
    pub format: String,
    pub files: Vec<TrackedFile>,
}
pub fn sha1_file(path: &Path, control: &TaskControl) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha1::new();
    let mut buffer = [0u8; 65536];
    loop {
        control.check()?;
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
fn validate_hash(hash: &str) -> Result<()> {
    if hash.len() != 40 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("整合包记录包含无效 SHA-1");
    }
    Ok(())
}
fn cache_path(config: &Config, hash: &str) -> Result<PathBuf> {
    validate_hash(hash)?;
    Ok(config
        .root()?
        .join("tui-mc-packs")
        .join(format!("{hash}.zip")))
}
pub fn cache_archive(
    config: &Config,
    source: &Path,
    hash: &str,
    control: &TaskControl,
) -> Result<()> {
    control.check()?;
    let dest = cache_path(config, hash)?;
    if dest.is_file() && valid_file(&dest, Some(hash), None)? {
        return Ok(());
    }
    crate::storage::atomic_copy_controlled(&dest, &mut fs::File::open(source)?, control)?;
    if !valid_file(&dest, Some(hash), None)? {
        bail!("整合包归档在导入期间发生改变，请重新导入");
    }
    Ok(())
}
pub fn cache_local_file(config: &Config, source: &Path, hash: &str) -> Result<PathBuf> {
    validate_hash(hash)?;
    let dest = config.root()?.join("tui-mc-packs/files").join(hash);
    if !dest.is_file() || !valid_file(&dest, Some(hash), None)? {
        atomic_copy(&dest, &mut fs::File::open(source)?)?;
        if !valid_file(&dest, Some(hash), None)? {
            bail!("手动文件在导入期间发生改变");
        }
    }
    Ok(dest)
}
pub fn save(
    config: &Config,
    id: &str,
    hash: String,
    format: String,
    files: Vec<TrackedFile>,
) -> Result<()> {
    valid_id(id)?;
    validate_hash(&hash)?;
    let receipt = Receipt {
        schema: 1,
        instance: id.into(),
        archive_sha1: hash,
        format,
        files,
    };
    atomic_write(
        &config.root()?.join("versions").join(id).join(RECEIPT),
        &serde_json::to_vec_pretty(&receipt)?,
    )
}
pub fn clone_receipt(config: &Config, source: &str, target: &str) -> Result<()> {
    let root = config.root()?.join("versions");
    let source = root.join(source).join(RECEIPT);
    if !source.is_file() {
        return Ok(());
    }
    let mut receipt: Receipt = serde_json::from_slice(&fs::read(source)?)?;
    receipt.instance = target.into();
    atomic_write(
        &root.join(target).join(RECEIPT),
        &serde_json::to_vec_pretty(&receipt)?,
    )
}
fn effective_destination(root: &Path, path: &str) -> Result<PathBuf> {
    let dest = safe_path(root, path)?;
    if !dest.exists() && path.to_lowercase().ends_with(".jar") {
        let disabled = safe_path(root, &format!("{path}.disabled"))?;
        if disabled.is_file() {
            return Ok(disabled);
        }
    }
    Ok(dest)
}
impl Backend {
    #[cfg(test)]
    pub fn repair_pack_files(&self, config: &Config, id: &str) -> Result<()> {
        let mutation = crate::instance_service::InstanceMutation::begin(&config.root()?, id)?;
        self.repair_pack_files_with_session(config, id, &mutation)
    }
    pub(crate) fn repair_pack_files_with_session(
        &self,
        config: &Config,
        id: &str,
        mutation: &crate::instance_service::InstanceMutation,
    ) -> Result<()> {
        valid_id(id)?;
        let root = config.root()?;
        mutation.require(&root, id)?;
        let version_dir = root.join("versions").join(id);
        let receipt_path = version_dir.join(RECEIPT);
        if !receipt_path.exists() {
            return Ok(());
        }
        let receipt: Receipt =
            serde_json::from_slice(&fs::read(receipt_path)?).context("整合包修复记录损坏")?;
        if receipt.schema != 1 || receipt.instance != id {
            bail!("整合包修复记录版本或实例 ID 不一致");
        }
        validate_hash(&receipt.archive_sha1)?;
        let game_dir = config.effective(id)?.instance_dir(id)?;
        let _game = crate::local_mod_service::ModMutationSession::lock_game_directory(config, id)?;
        let mut downloads = vec![];
        let mut restore = vec![];
        let mut seen = BTreeMap::new();
        for file in &receipt.files {
            self.control.check()?;
            validate_hash(&file.sha1)?;
            let dest = effective_destination(&game_dir, &file.path)?;
            crate::pack::file_destination(&game_dir, &file.path, id)?;
            if seen.insert(file.path.to_lowercase(), ()).is_some() {
                bail!("整合包修复记录含重复路径");
            }
            let managed = !file.urls.is_empty() || file.path.to_lowercase().starts_with("mods/");
            if dest.is_file() && (!managed || valid_file(&dest, Some(&file.sha1), Some(file.size))?)
            {
                continue;
            }
            if let Some(member) = file.member {
                restore.push((member, dest, file));
            } else if let Some(url) = file.urls.first() {
                downloads.push(Download {
                    url: url.clone(),
                    dest,
                    sha1: Some(file.sha1.clone()),
                    size: Some(file.size),
                    alternatives: file.urls.iter().skip(1).cloned().collect(),
                });
            } else {
                let local = root.join("tui-mc-packs/files").join(&file.sha1);
                if !local.is_file() || !valid_file(&local, Some(&file.sha1), Some(file.size))? {
                    bail!("手动文件缓存缺失：{}，请重新导入原文件", file.path);
                }
                crate::storage::atomic_copy_controlled(
                    &dest,
                    &mut fs::File::open(local)?,
                    &self.control,
                )?;
            }
        }
        self.downloader()
            .many(downloads, config.download_threads, "整合包修复")?;
        if !restore.is_empty() {
            let source = cache_path(config, &receipt.archive_sha1)?;
            if !source.is_file() || !valid_file(&source, Some(&receipt.archive_sha1), None)? {
                bail!("整合包原始归档缓存缺失或损坏，无法恢复内嵌文件；请重新导入原整合包到新实例");
            }
            let mut archive = zip::ZipArchive::new(fs::File::open(source)?)?;
            for (member, dest, file) in restore {
                self.control.check()?;
                let mut entry = archive.by_index(member)?;
                if entry.is_dir() || entry.size() != file.size {
                    bail!("整合包缓存条目与记录不匹配");
                }
                crate::storage::atomic_copy_controlled(&dest, &mut entry, &self.control)?;
                if !valid_file(&dest, Some(&file.sha1), Some(file.size))? {
                    bail!("恢复后的整合包内嵌文件校验失败");
                }
            }
        }
        self.log("整合包文件已校验；已禁用的 Mod 与已有配置保持原状态");
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keeps_disabled_mod_destination() {
        let root = std::env::temp_dir().join(format!("tui-receipt-{}", std::process::id()));
        fs::create_dir_all(root.join("mods")).unwrap();
        fs::write(root.join("mods/a.jar.disabled"), b"disabled").unwrap();
        assert_eq!(
            effective_destination(&root, "mods/a.jar").unwrap(),
            root.join("mods/a.jar.disabled")
        );
        assert!(effective_destination(&root, "../outside").is_err());
        fs::remove_file(root.join("mods/a.jar.disabled")).unwrap();
        fs::remove_dir(root.join("mods")).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
