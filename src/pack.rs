use crate::{
    backend::Backend,
    config::Config,
    download::Download,
    loader::Loader,
    metadata::{safe_path, valid_id},
    storage::atomic_write,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Pack {
    pub(crate) format_version: u32,
    pub(crate) game: String,
    pub(crate) name: String,
    pub(crate) dependencies: BTreeMap<String, String>,
    pub(crate) files: Vec<PackFile>,
    #[serde(skip, default = "modrinth_format")]
    pub(crate) source_format: String,
    #[serde(skip, default = "overrides_default")]
    pub(crate) override_root: String,
    #[serde(skip)]
    pub(crate) curse_files: Vec<crate::curseforge::CurseFile>,
}
fn modrinth_format() -> String {
    "modrinth".into()
}
fn overrides_default() -> String {
    "overrides".into()
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PackFile {
    pub(crate) path: String,
    pub(crate) hashes: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) downloads: Vec<String>,
    pub(crate) file_size: u64,
    #[serde(skip)]
    pub(crate) local_source: Option<PathBuf>,
}
pub struct PackSummary {
    pub name: String,
    pub minecraft: String,
    pub files: usize,
    pub format: String,
}
fn read_index(archive: &mut zip::ZipArchive<fs::File>) -> Result<Pack> {
    if !archive
        .file_names()
        .any(|name| name == "modrinth.index.json")
    {
        let entry = archive
            .by_name("manifest.json")
            .context("整合包中没有 Modrinth 或 CurseForge 索引")?;
        if entry.size() > 16 * 1024 * 1024 {
            bail!("整合包索引过大");
        }
        return crate::curseforge::normalize(serde_json::from_reader(entry)?);
    }
    let entry = archive
        .by_name("modrinth.index.json")
        .context("当前支持 Modrinth .mrpack 整合包；缺少 modrinth.index.json")?;
    if entry.size() > 16 * 1024 * 1024 {
        bail!("整合包索引过大");
    }
    let pack: Pack = serde_json::from_reader(entry)?;
    if pack.format_version != 1 || pack.game != "minecraft" {
        bail!("不支持的整合包格式或游戏");
    }
    if !pack.dependencies.contains_key("minecraft") {
        bail!("整合包没有指定 Minecraft 版本");
    }
    Ok(pack)
}
pub fn inspect(path: &Path) -> Result<PackSummary> {
    let mut archive = zip::ZipArchive::new(fs::File::open(path)?)?;
    let pack = read_index(&mut archive)?;
    Ok(PackSummary {
        name: pack.name,
        minecraft: pack.dependencies["minecraft"].clone(),
        files: pack.files.len() + pack.curse_files.len(),
        format: pack.source_format,
    })
}
fn generated_id(name: &str) -> String {
    let name = name
        .chars()
        .filter(|c| c.is_alphanumeric() || " -_+".contains(*c))
        .take(50)
        .collect::<String>();
    let name = name.trim_matches([' ', '.']);
    format!("pack-{}", if name.is_empty() { "minecraft" } else { name })
}
pub(crate) fn file_destination(root: &Path, path: &str, id: &str) -> Result<PathBuf> {
    let path = safe_path(root, path)?;
    // Game directories coexist with launch metadata: packs must not replace it.
    let relative = path.strip_prefix(root)?;
    let first = relative
        .components()
        .next()
        .context("空整合包路径")?
        .as_os_str()
        .to_string_lossy();
    if first.eq_ignore_ascii_case(&format!("{id}.json"))
        || first.eq_ignore_ascii_case(&format!("{id}.jar"))
        || first.to_lowercase().starts_with("tui-mc-")
    {
        bail!("整合包文件与启动器元数据冲突");
    }
    Ok(path)
}
impl Backend {
    pub fn export_pack(&self, config: &Config, id: &str, output: &Path) -> Result<()> {
        valid_id(id)?;
        self.control.check()?;
        if output.exists() {
            bail!("导出目标已存在，不会覆盖");
        }
        let root = config.root()?;
        let resolved = crate::version::resolve(&root, id)?;
        let minecraft = resolved.chain.first().context("实例版本链为空")?;
        let mut dependencies = serde_json::Map::new();
        dependencies.insert("minecraft".into(), json!(minecraft));
        for lib in resolved.meta["libraries"].as_array().into_iter().flatten() {
            let Some(name) = lib["name"].as_str() else {
                continue;
            };
            let parts = name.split(':').collect::<Vec<_>>();
            if parts.len() < 3 {
                continue;
            }
            let kind = match (parts[0], parts[1]) {
                ("net.fabricmc", "fabric-loader") => Some("fabric-loader"),
                ("org.quiltmc", "quilt-loader") => Some("quilt-loader"),
                ("net.minecraftforge", "forge") => Some("forge"),
                ("net.neoforged", "neoforge" | "forge") => Some("neoforge"),
                _ => None,
            };
            if let Some(kind) = kind {
                let version = if kind == "forge" {
                    parts[2]
                        .strip_prefix(&format!("{minecraft}-"))
                        .unwrap_or(parts[2])
                } else {
                    parts[2]
                };
                dependencies.insert(kind.into(), json!(version));
            }
        }
        if dependencies.len() > 2 {
            bail!("实例包含多个加载器，无法生成标准 Modrinth 整合包");
        }
        let game_dir = config.effective(id).instance_dir(id)?;
        let files = crate::instances::data_files(&game_dir, false, &self.control)?;
        let mut total_size = 0u64;
        for (source, _) in &files {
            let size = fs::metadata(source)?.len();
            total_size = total_size.saturating_add(size);
            if size > 1024 * 1024 * 1024 || total_size > 4 * 1024 * 1024 * 1024 {
                bail!("导出内容超过整合包导入的大小上限");
            }
        }
        let name = crate::instances::names(config)
            .remove(id)
            .unwrap_or_else(|| id.into());
        let index = json!({"formatVersion":1,"game":"minecraft","versionId":"1.0.0","name":name,"dependencies":dependencies,"files":[]});
        let parent = output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(
            ".tui-mc-export-{}-{}.part",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let result = (|| -> Result<()> {
            use std::io::Write;
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            let mut archive = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(0o644);
            archive.start_file("modrinth.index.json", options)?;
            archive.write_all(&serde_json::to_vec_pretty(&index)?)?;
            let total = files.len();
            for (i, (source, relative)) in files.into_iter().enumerate() {
                self.control.check()?;
                archive.start_file(format!("overrides/{relative}"), options)?;
                std::io::copy(&mut fs::File::open(source)?, &mut archive)?;
                let _ = self.tx.send(crate::backend::Message::Progress(
                    "整合包导出".into(),
                    i + 1,
                    total,
                ));
            }
            archive.finish()?.sync_all()?;
            self.control.check()?;
            if output.exists() {
                bail!("目标文件已经出现，取消覆盖");
            }
            fs::rename(&temporary, output)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        self.log(format!("整合包已导出：{}", output.display()));
        Ok(())
    }
    pub fn import_pack(
        &self,
        config: &Config,
        path: &Path,
        requested_id: Option<&str>,
    ) -> Result<String> {
        self.control.check()?;
        let mut archive = zip::ZipArchive::new(fs::File::open(path)?)?;
        let mut pack = read_index(&mut archive)?;
        if !pack.curse_files.is_empty() {
            pack.files = self.resolve_curseforge(config, &pack.curse_files)?;
        }
        let source_hash = crate::pack_state::sha1_file(path, &self.control)?;
        let id = requested_id
            .map(str::to_owned)
            .unwrap_or_else(|| generated_id(&pack.name));
        valid_id(&id)?;
        let root = config.root()?;
        let game_dir = root.join("versions").join(&id);
        if game_dir.exists() {
            let pending = game_dir.join("tui-mc-import-pending.json");
            let resume = fs::read(pending)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .is_some_and(|value| {
                    value["source_sha1"] == source_hash
                        || (value.get("source_sha1").is_none()
                            && value["source"] == path.display().to_string())
                });
            if !resume || game_dir.join(format!("{id}.json")).exists() {
                bail!("实例 {id} 已存在，请指定新名称以避免覆盖存档和配置");
            }
            self.log("继续上次未完成的整合包导入");
        }
        let minecraft = &pack.dependencies["minecraft"];
        let mut loaders = pack
            .dependencies
            .iter()
            .filter(|(key, _)| key.as_str() != "minecraft");
        let loader = loaders.next();
        if loaders.next().is_some() {
            bail!("整合包指定了多个加载器");
        }
        if loader.is_some_and(|(name, _)| {
            !matches!(
                name.as_str(),
                "fabric-loader" | "quilt-loader" | "forge" | "neoforge"
            )
        }) {
            bail!("不支持的整合包加载器：{}", loader.unwrap().0);
        }
        // Validate all paths and hashes before installing or extracting anything.
        let mut files = vec![];
        let mut tracked = BTreeMap::new();
        let mut local_files = vec![];
        let mut paths = HashSet::new();
        for file in &pack.files {
            if file.env.get("client").is_some_and(|value| {
                value == "unsupported" || (value == "optional" && !config.optional_pack_files)
            }) {
                continue;
            }
            let dest = file_destination(&game_dir, &file.path, &id)?;
            let key = dest.to_string_lossy().to_lowercase();
            if !paths.insert(key) {
                bail!("整合包存在重复文件路径");
            }
            let hash = file.hashes.get("sha1").context("整合包文件缺少 SHA-1")?;
            if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("整合包 SHA-1 无效");
            }
            if let Some(source) = &file.local_source {
                tracked.insert(
                    file.path.to_lowercase(),
                    crate::pack_state::TrackedFile {
                        path: file.path.clone(),
                        sha1: hash.clone(),
                        size: file.file_size,
                        urls: vec![],
                        member: None,
                    },
                );
                local_files.push((source.clone(), dest, hash.clone(), file.file_size));
                continue;
            }
            let url = file
                .downloads
                .iter()
                .find(|url| reqwest::Url::parse(url).is_ok_and(|u| u.scheme() == "https"))
                .context("整合包文件没有可用的 HTTPS 下载地址")?;
            tracked.insert(
                file.path.to_lowercase(),
                crate::pack_state::TrackedFile {
                    path: file.path.clone(),
                    sha1: hash.clone(),
                    size: file.file_size,
                    urls: file
                        .downloads
                        .iter()
                        .filter(|url| reqwest::Url::parse(url).is_ok_and(|u| u.scheme() == "https"))
                        .cloned()
                        .collect(),
                    member: None,
                },
            );
            files.push(Download {
                url: url.clone(),
                dest,
                sha1: Some(hash.clone()),
                size: Some(file.file_size),
                alternatives: file
                    .downloads
                    .iter()
                    .filter(|url| url.starts_with("https://"))
                    .cloned()
                    .collect(),
            });
        }
        let mut overrides = vec![];
        let overrides_prefix = format!("{}/", pack.override_root.trim_end_matches('/'));
        let mut total = 0u64;
        for i in 0..archive.len() {
            let entry = archive.by_index(i)?;
            if entry.is_dir() {
                continue;
            }
            let name = entry.name();
            let priority = if name.starts_with("client-overrides/") {
                1
            } else {
                0
            };
            let relative = if let Some(path) = name.strip_prefix("client-overrides/") {
                path
            } else if let Some(path) = name.strip_prefix(&overrides_prefix) {
                path
            } else {
                continue;
            };
            let dest = file_destination(&game_dir, relative, &id)?;
            if entry
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 == 0o120000)
            {
                bail!("整合包覆盖文件不能是符号链接");
            }
            total = total.saturating_add(entry.size());
            if total > 4 * 1024 * 1024 * 1024 || entry.size() > 1024 * 1024 * 1024 {
                bail!("整合包覆盖文件超出导入大小限制");
            }
            overrides.push((priority, i, dest));
        }
        self.log(format!("导入整合包 {} · Minecraft {minecraft}", pack.name));
        let parent = if let Some((kind, version)) = loader {
            match kind.as_str() {
                "fabric-loader" => {
                    self.install_loader(config, Loader::Fabric, minecraft, Some(version))?
                }
                "quilt-loader" => {
                    self.install_loader(config, Loader::Quilt, minecraft, Some(version))?
                }
                "forge" => self.install_forge(
                    config,
                    crate::forge::ForgeKind::Forge,
                    minecraft,
                    Some(version),
                )?,
                "neoforge" => self.install_forge(
                    config,
                    crate::forge::ForgeKind::NeoForge,
                    minecraft,
                    Some(version),
                )?,
                _ => unreachable!(),
            }
        } else {
            if root
                .join("versions")
                .join(minecraft)
                .join(format!("{minecraft}.json"))
                .is_file()
            {
                self.repair(config, minecraft)?;
            } else {
                let remote = self.remote_version(minecraft)?;
                self.install(config, &remote)?;
            }
            minecraft.clone()
        };
        fs::create_dir_all(&game_dir)?;
        // A manifest marks an incomplete import so failure never looks like a ready instance.
        let pending = game_dir.join("tui-mc-import-pending.json");
        atomic_write(
            &pending,
            &serde_json::to_vec_pretty(
                &json!({"name":pack.name,"parent":parent,"source":path.display().to_string(),"source_sha1":source_hash}),
            )?,
        )?;
        self.downloader()
            .many(files, config.download_threads, "整合包文件")?;
        crate::pack_state::cache_archive(config, path, &source_hash, &self.control)?;
        for (source, dest, hash, size) in local_files {
            self.control.check()?;
            if !crate::download::valid_file(&source, Some(&hash), Some(size))? {
                bail!("手动下载的文件校验失败：{}", source.display());
            }
            let cached = crate::pack_state::cache_local_file(config, &source, &hash)?;
            crate::storage::atomic_copy(&dest, &mut fs::File::open(cached)?)?;
        }
        overrides.sort_by_key(|(priority, _, _)| *priority);
        for (_, index, dest) in overrides {
            self.control.check()?;
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut entry = archive.by_index(index)?;
            crate::storage::atomic_copy(&dest, &mut entry)?;
            let relative = dest
                .strip_prefix(&game_dir)?
                .to_string_lossy()
                .replace('\\', "/");
            tracked.insert(
                relative.to_lowercase(),
                crate::pack_state::TrackedFile {
                    path: relative,
                    sha1: crate::pack_state::sha1_file(&dest, &self.control)?,
                    size: fs::metadata(&dest)?.len(),
                    urls: vec![],
                    member: Some(index),
                },
            );
        }
        crate::pack_state::save(
            config,
            &id,
            source_hash,
            pack.source_format.clone(),
            tracked.into_values().collect(),
        )?;
        let profile = json!({"id":id,"inheritsFrom":parent,"tuiMcIsolated":true,"tuiMcPack":{"name":pack.name,"format":pack.source_format}});
        atomic_write(
            &game_dir.join(format!("{id}.json")),
            &serde_json::to_vec_pretty(&profile)?,
        )?;
        fs::remove_file(pending)?;
        self.log(format!("整合包已就绪：{id}"));
        Ok(id)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn imports_local_pack_and_repairs_without_changing_disabled_mods_or_configuration() {
        use std::io::Write;
        let temp = crate::test_support::Workspace::new();
        let root = temp.path.join("game");
        let base = root.join("versions/base");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(root.join("assets/indexes")).unwrap();
        let control = crate::task::TaskControl::default();
        fs::write(base.join("base.jar"), b"fixture-client").unwrap();
        let index_path = root.join("assets/indexes/base.json");
        fs::write(&index_path, b"{\"objects\":{}}").unwrap();
        let client_hash = crate::pack_state::sha1_file(&base.join("base.jar"), &control).unwrap();
        let index_hash = crate::pack_state::sha1_file(&index_path, &control).unwrap();
        fs::write(base.join("base.json"),json!({"id":"base","libraries":[],"downloads":{"client":{"url":"https://invalid.example/client.jar","sha1":client_hash,"size":14}},"assetIndex":{"id":"base","url":"https://invalid.example/index.json","sha1":index_hash,"size":14}}).to_string()).unwrap();
        let source = temp.path.join("example.mrpack");
        let mut archive = zip::ZipWriter::new(fs::File::create(&source).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        for(name,bytes)in[("modrinth.index.json",json!({"formatVersion":1,"game":"minecraft","name":"Example","dependencies":{"minecraft":"base"},"files":[]}).to_string().into_bytes()),("overrides/mods/a.jar",b"original-mod".to_vec()),("overrides/config/settings.json",b"original-config".to_vec())]{archive.start_file(name,options).unwrap();archive.write_all(&bytes).unwrap();}
        archive.finish().unwrap();
        let config = Config {
            game_dir: root.clone(),
            ..Config::default()
        };
        let (tx, _) = std::sync::mpsc::channel();
        let backend = Backend::new(tx).unwrap();
        let id = backend
            .import_pack(&config, &source, Some("test-pack"))
            .unwrap();
        let dir = root.join("versions").join(&id);
        fs::rename(dir.join("mods/a.jar"), dir.join("mods/a.jar.disabled")).unwrap();
        fs::write(dir.join("mods/a.jar.disabled"), b"bad").unwrap();
        fs::write(dir.join("config/settings.json"), b"user-config").unwrap();
        backend.repair_pack_files(&config, &id).unwrap();
        assert_eq!(
            fs::read(dir.join("mods/a.jar.disabled")).unwrap(),
            b"original-mod"
        );
        assert!(!dir.join("mods/a.jar").exists());
        assert_eq!(
            fs::read(dir.join("config/settings.json")).unwrap(),
            b"user-config"
        );
        fs::remove_file(dir.join("config/settings.json")).unwrap();
        backend.repair_pack_files(&config, &id).unwrap();
        assert_eq!(
            fs::read(dir.join("config/settings.json")).unwrap(),
            b"original-config"
        );
    }
    #[test]
    fn export_contains_mods_but_excludes_personal_worlds() {
        let root = std::env::temp_dir().join(format!("tui-export-{}", std::process::id()));
        let instance = root.join("versions/base");
        fs::create_dir_all(instance.join("mods")).unwrap();
        fs::create_dir_all(instance.join("saves/world")).unwrap();
        fs::write(
            instance.join("base.json"),
            json!({"id":"base","libraries":[]}).to_string(),
        )
        .unwrap();
        fs::write(instance.join("mods/a.jar"), b"mod").unwrap();
        fs::write(instance.join("saves/world/level.dat"), b"world").unwrap();
        let config = Config {
            game_dir: root.clone(),
            ..Config::default()
        };
        let (tx, _) = std::sync::mpsc::channel();
        let backend = Backend::new(tx).unwrap();
        let output = root.join("pack.mrpack");
        backend.export_pack(&config, "base", &output).unwrap();
        {
            let mut archive = zip::ZipArchive::new(fs::File::open(&output).unwrap()).unwrap();
            assert!(archive.by_name("overrides/mods/a.jar").is_ok());
            assert!(archive.by_name("overrides/saves/world/level.dat").is_err());
            let index: serde_json::Value =
                serde_json::from_reader(archive.by_name("modrinth.index.json").unwrap()).unwrap();
            assert_eq!(index["dependencies"]["minecraft"], "base");
        }
        assert!(backend.export_pack(&config, "base", &output).is_err());
        fs::remove_file(output).unwrap();
        for relative in ["base.json", "mods/a.jar", "saves/world/level.dat"] {
            fs::remove_file(instance.join(relative)).unwrap();
        }
        for dir in ["mods", "saves/world", "saves"] {
            fs::remove_dir(instance.join(dir)).unwrap();
        }
        fs::remove_dir(instance).unwrap();
        fs::remove_dir(root.join("versions")).unwrap();
        fs::remove_dir(root).unwrap();
    }
    #[test]
    fn protects_launch_metadata_and_traversal() {
        let root = Path::new("instance");
        assert!(file_destination(root, "../evil", "test").is_err());
        assert!(file_destination(root, "test.json", "test").is_err());
        assert!(file_destination(root, "tui-mc-import-pending.json", "test").is_err());
        assert!(file_destination(root, "mods/example.jar", "test").is_ok());
    }
    #[test]
    fn pack_names_produce_valid_instance_ids() {
        for name in ["My Modpack!", "中文整合包", "../../oops", ""] {
            valid_id(&generated_id(name)).unwrap();
        }
    }
}
