use crate::{
    backend::Backend,
    config::Config,
    metadata::valid_id,
    pack::{Pack, PackFile},
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};
use zeroize::Zeroizing;

#[derive(Clone, Deserialize)]
pub(crate) struct CurseFile {
    #[serde(rename = "projectID")]
    pub project_id: u64,
    #[serde(rename = "fileID")]
    pub file_id: u64,
    #[serde(default = "yes")]
    pub required: bool,
}
fn yes() -> bool {
    true
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CurseManifest {
    manifest_type: String,
    manifest_version: u32,
    name: String,
    minecraft: Minecraft,
    files: Vec<CurseFile>,
    #[serde(default = "override_default")]
    overrides: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Minecraft {
    version: String,
    #[serde(default)]
    mod_loaders: Vec<ModLoader>,
}
#[derive(Deserialize)]
struct ModLoader {
    id: String,
    #[serde(default)]
    primary: bool,
}
fn override_default() -> String {
    "overrides".into()
}
pub(crate) fn normalize(value: Value) -> Result<Pack> {
    let manifest: CurseManifest = serde_json::from_value(value)?;
    if manifest.manifest_type != "minecraftModpack" || manifest.manifest_version != 1 {
        bail!("不支持的 CurseForge 整合包清单");
    }
    valid_id(&manifest.minecraft.version)?;
    crate::metadata::safe_path(std::path::Path::new("root"), &manifest.overrides)?;
    let mut dependencies = BTreeMap::new();
    dependencies.insert("minecraft".into(), manifest.minecraft.version);
    if !manifest.minecraft.mod_loaders.is_empty() {
        let selected = if manifest.minecraft.mod_loaders.len() == 1 {
            &manifest.minecraft.mod_loaders[0]
        } else {
            let primary = manifest
                .minecraft
                .mod_loaders
                .iter()
                .filter(|loader| loader.primary)
                .collect::<Vec<_>>();
            if primary.len() != 1 {
                bail!("CurseForge 清单需要指定唯一主加载器");
            }
            primary[0]
        };
        let (family, version) = selected
            .id
            .split_once('-')
            .context("无法识别 CurseForge 加载器 ID")?;
        let family = match family.to_lowercase().as_str() {
            "forge" => "forge",
            "neoforge" => "neoforge",
            "fabric" | "fabric-loader" => "fabric-loader",
            "quilt" | "quilt-loader" => "quilt-loader",
            _ => bail!("不支持的 CurseForge 加载器"),
        };
        let version = version.strip_prefix("loader-").unwrap_or(version);
        valid_id(version)?;
        dependencies.insert(family.into(), version.into());
    }
    if manifest
        .files
        .iter()
        .any(|file| file.project_id == 0 || file.file_id == 0)
    {
        bail!("CurseForge 项目 / 文件 ID 无效");
    }
    Ok(Pack {
        format_version: 1,
        game: "minecraft".into(),
        name: manifest.name,
        dependencies,
        files: vec![],
        source_format: "curseforge".into(),
        override_root: manifest.overrides.replace('\\', "/"),
        curse_files: manifest.files,
    })
}
pub fn save_key(key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() || key.chars().any(char::is_control) {
        bail!("API 密钥不能为空或包含控制字符");
    }
    keyring::Entry::new("tui-mc", "curseforge-api")?
        .set_password(key)
        .context("无法将 API 密钥保存到系统安全存储")
}
pub fn clear_key() -> Result<()> {
    match keyring::Entry::new("tui-mc", "curseforge-api")?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error).context("无法移除 API 密钥"),
    }
}
fn load_key() -> Result<Zeroizing<String>> {
    if let Ok(key) = std::env::var("CURSEFORGE_API_KEY") {
        if !key.trim().is_empty() {
            return Ok(Zeroizing::new(key));
        }
    }
    Ok(Zeroizing::new(keyring::Entry::new("tui-mc","curseforge-api")?.get_password().context("CurseForge 导入需要自己的 API 密钥；在账户页按 K 配置，或设置 CURSEFORGE_API_KEY 环境变量")?))
}
pub(crate) fn pack_file(reference: &CurseFile, value: &Value) -> Result<PackFile> {
    let mut file = file_metadata(reference, value)?;
    let url = value["downloadUrl"]
        .as_str()
        .filter(|url| url.starts_with("https://"))
        .context("作者未提供可用的第三方 HTTPS 下载地址，需要手动下载")?;
    file.downloads.push(url.into());
    Ok(file)
}
fn file_metadata(reference: &CurseFile, value: &Value) -> Result<PackFile> {
    if value["id"].as_u64() != Some(reference.file_id)
        || value["modId"].as_u64() != Some(reference.project_id)
    {
        bail!("CurseForge 返回的项目 / 文件 ID 不匹配");
    }
    let filename = value["fileName"]
        .as_str()
        .context("CurseForge 缺少文件名")?;
    valid_id(filename)?;
    let sha1 = value["hashes"]
        .as_array()
        .and_then(|hashes| hashes.iter().find(|hash| hash["algo"] == 1))
        .and_then(|hash| hash["value"].as_str())
        .context("CurseForge 文件没有 SHA-1，无法安全校验")?;
    if sha1.len() != 40 || !sha1.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("CurseForge 文件 SHA-1 无效");
    }
    let path = if filename.to_lowercase().ends_with(".zip") {
        format!("resourcepacks/{filename}")
    } else if filename.to_lowercase().ends_with(".jar") {
        format!("mods/{filename}")
    } else {
        bail!("无法判断 CurseForge 文件的安装目录：{filename}");
    };
    Ok(PackFile {
        path,
        hashes: BTreeMap::from([("sha1".into(), sha1.into())]),
        env: BTreeMap::new(),
        downloads: vec![],
        file_size: value["fileLength"]
            .as_u64()
            .context("CurseForge 文件缺少大小")?,
        local_source: None,
    })
}
impl Backend {
    pub(crate) fn resolve_curseforge(
        &self,
        config: &Config,
        references: &[CurseFile],
    ) -> Result<Vec<PackFile>> {
        let references = references
            .iter()
            .filter(|file| file.required || config.optional_pack_files)
            .collect::<Vec<_>>();
        if references.is_empty() {
            return Ok(vec![]);
        }
        let key = load_key()?;
        // The API key is never attached to a redirected CDN request.
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("tui-mc/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let mut found = HashMap::new();
        for batch in references.chunks(50) {
            self.control.check()?;
            let ids = batch.iter().map(|file| file.file_id).collect::<Vec<_>>();
            let response = client
                .post("https://api.curseforge.com/v1/mods/files")
                .header("x-api-key", key.as_str())
                .json(&json!({"fileIds":ids}))
                .send()
                .context("CurseForge API 网络请求失败")?;
            if !response.status().is_success() {
                bail!(
                    "CurseForge API 返回 HTTP {}；请检查密钥和服务权限",
                    response.status().as_u16()
                );
            }
            let value: Value = response.json()?;
            for file in value["data"]
                .as_array()
                .context("CurseForge 文件列表格式错误")?
            {
                if let Some(id) = file["id"].as_u64() {
                    found.insert(id, file.clone());
                }
            }
        }
        let mut files = vec![];
        let mut unavailable = vec![];
        for reference in references {
            match found
                .get(&reference.file_id)
                .context("CurseForge API 中没有该文件")
                .and_then(|value| pack_file(reference, value))
            {
                Ok(file) => files.push(file),
                Err(error) => {
                    let mut report = json!({"project_id":reference.project_id,"file_id":reference.file_id,"reason":error.to_string()});
                    if let Some(value) = found.get(&reference.file_id) {
                        if let Ok(mut file) = file_metadata(reference, value) {
                            let filename = std::path::Path::new(&file.path)
                                .file_name()
                                .context("文件名缺失")?
                                .to_string_lossy()
                                .to_string();
                            let local = config.root()?.join("tui-mc-manual-files").join(&filename);
                            report["filename"] = json!(filename);
                            report["sha1"] = json!(file.hashes["sha1"]);
                            report["size"] = json!(file.file_size);
                            report["manual_path"] = json!(local.display().to_string());
                            if local.is_file()
                                && crate::download::valid_file(
                                    &local,
                                    Some(&file.hashes["sha1"]),
                                    Some(file.file_size),
                                )?
                            {
                                file.local_source = Some(local);
                                files.push(file);
                                continue;
                            }
                        }
                    }
                    unavailable.push(report);
                }
            }
        }
        if !unavailable.is_empty() {
            let path = config.root()?.join("tui-mc-manual-downloads.json");
            crate::storage::atomic_write(
                &path,
                &serde_json::to_vec_pretty(&json!({"provider":"curseforge","files":unavailable}))?,
            )?;
            bail!(
                "部分 CurseForge 文件不可自动下载，清单已保存至 {}；未将整合包标记为安装完成",
                path.display()
            );
        }
        Ok(files)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_manifest_primary_loader_and_custom_overrides() {
        let pack=normalize(json!({"manifestType":"minecraftModpack","manifestVersion":1,"name":"Test","minecraft":{"version":"1.21.1","modLoaders":[{"id":"neoforge-21.1.100","primary":true}]},"files":[{"projectID":1,"fileID":2,"required":true}],"overrides":"custom"})).unwrap();
        assert_eq!(pack.dependencies["neoforge"], "21.1.100");
        assert_eq!(pack.override_root, "custom");
        assert_eq!(pack.curse_files[0].file_id, 2);
    }
    #[test]
    fn rejects_unavailable_downloads_and_wrong_identity() {
        let reference = CurseFile {
            project_id: 1,
            file_id: 2,
            required: true,
        };
        let data = json!({"id":2,"modId":1,"fileName":"a.jar","fileLength":3,"hashes":[{"algo":1,"value":"a9993e364706816aba3e25717850c26c9cd0d89d"}],"downloadUrl":null});
        assert!(pack_file(&reference, &data).is_err());
        let mut available = data;
        available["downloadUrl"] = json!("https://example.com/a.jar");
        assert!(pack_file(&reference, &available).is_ok());
        available["modId"] = json!(3);
        assert!(pack_file(&reference, &available).is_err());
    }
}
