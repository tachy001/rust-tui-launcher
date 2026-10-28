use crate::{optifine_archive::Identity, task::TaskControl};
use anyhow::{Context, Result};
use std::{fs, path::Path};

/// Read enabled local mod identities without executing or modifying archives.
pub(crate) struct InstalledModService;
impl InstalledModService {
    pub(crate) fn optifine_games(
        &self,
        game_dir: &Path,
        control: &TaskControl,
    ) -> Result<Vec<String>> {
        control.check()?;
        let entries = match fs::read_dir(game_dir.join("mods")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(error) => return Err(error).context("无法读取实例 mods 目录"),
        };
        let mut games = vec![];
        for entry in entries {
            control.check()?;
            let entry = entry?;
            if !entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
            {
                continue;
            }
            let path = entry.path();
            // Read-only inspection follows file links just as opening the JAR does.
            // Directory links are never traversed; mutation APIs retain their policy.
            if !fs::metadata(&path)
                .with_context(|| format!("无法读取 Mod 文件或符号链接目标：{}", path.display()))?
                .is_file()
            {
                continue;
            }
            let file = fs::File::open(&path)
                .with_context(|| format!("无法打开 Mod 文件：{}", path.display()))?;
            let archive = zip::ZipArchive::new(file)
                .with_context(|| format!("Mod JAR 损坏：{}", path.display()))?;
            control.check()?;
            let candidate = archive.file_names().any(|name| {
                matches!(
                    name,
                    "net/optifine/Config.class"
                        | "notch/net/optifine/Config.class"
                        | "optifine/Installer.class"
                )
            });
            if !candidate {
                continue;
            }
            drop(archive);
            let mut archive = crate::installer_archive::open(&path, control)
                .with_context(|| format!("OptiFine 归档目录无效：{}", path.display()))?;
            let identity = Identity::read(&mut archive, control)
                .with_context(|| format!("无法读取 OptiFine 身份：{}", path.display()))?;
            if !games.contains(&identity.game) {
                games.push(identity.game);
            }
        }
        control.check()?;
        Ok(games)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optifine_archive::fixtures;
    #[test]
    fn ambiguous_optifine_directory_is_rejected_with_its_path() {
        let workspace = crate::test_support::Workspace::new();
        let mods = workspace.path.join("mods");
        fs::create_dir(&mods).unwrap();
        let path = mods.join("ambiguous.jar");
        let original = b"net/optifine/Confix.class";
        let replacement = b"net/optifine/Config.class";
        let mut data = fixtures::jar(&[
            ("net/optifine/Config.class", &fixtures::config("1.18.2")),
            ("net/optifine/Confix.class", &fixtures::config("1.21.1")),
        ]);
        for offset in 0..=data.len() - original.len() {
            if &data[offset..offset + original.len()] == original {
                data[offset..offset + original.len()].copy_from_slice(replacement);
            }
        }
        fs::write(path, data).unwrap();
        let error = InstalledModService
            .optifine_games(&workspace.path, &TaskControl::default())
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("ambiguous.jar"));
        assert!(message.contains("重复"));
    }
    #[test]
    #[ignore = "requires permission to create filesystem symbolic links"]
    fn symbolic_mod_file_is_read_without_modifying_its_target() {
        let workspace = crate::test_support::Workspace::new();
        let mods = workspace.path.join("mods");
        fs::create_dir(&mods).unwrap();
        let target = workspace.path.join("shared.jar");
        let content = fixtures::jar(&[("net/optifine/Config.class", &fixtures::config("1.18.2"))]);
        fs::write(&target, &content).unwrap();
        let link = mods.join("linked.jar");
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, &link).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            InstalledModService
                .optifine_games(&workspace.path, &TaskControl::default())
                .unwrap(),
            ["1.18.2"]
        );
        assert_eq!(fs::read(&target).unwrap(), content);
        fs::remove_file(&target).unwrap();
        let error = InstalledModService
            .optifine_games(&workspace.path, &TaskControl::default())
            .unwrap_err();
        assert!(error.to_string().contains("符号链接目标"));
    }
    #[test]
    #[ignore = "requires TUI_TEST_OPTIFINE_JAR; reads a real archive without executing it"]
    fn real_optifine_archive_is_identified_after_renaming() {
        let source = std::path::PathBuf::from(
            std::env::var_os("TUI_TEST_OPTIFINE_JAR").expect("set archive path"),
        );
        let workspace = crate::test_support::Workspace::new();
        let mods = workspace.path.join("mods");
        fs::create_dir(&mods).unwrap();
        let renamed = mods.join("arbitrary-name.jar");
        fs::copy(source, &renamed).unwrap();
        let games = InstalledModService
            .optifine_games(&workspace.path, &TaskControl::default())
            .unwrap();
        assert_eq!(games.len(), 1);
        assert!(games[0].starts_with("1."));
        eprintln!("Real OptiFine Minecraft identity: {}", games[0]);
        fs::rename(&renamed, mods.join("arbitrary-name.jar.disabled")).unwrap();
        assert!(InstalledModService
            .optifine_games(&workspace.path, &TaskControl::default())
            .unwrap()
            .is_empty());
    }
    #[test]
    fn reads_renamed_enabled_mods_and_skips_disabled_and_unrelated_archives() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let service = InstalledModService;
        assert!(service
            .optifine_games(&workspace.path, &control)
            .unwrap()
            .is_empty());
        assert!(!workspace.path.join("mods").exists());
        let mods = workspace.path.join("mods");
        fs::create_dir(&mods).unwrap();
        fs::write(
            mods.join("renamed.jar"),
            fixtures::jar(&[("net/optifine/Config.class", &fixtures::config("1.18.2"))]),
        )
        .unwrap();
        fs::write(mods.join("disabled.jar.disabled"), b"ignored").unwrap();
        fs::write(
            mods.join("other.jar"),
            fixtures::jar(&[("Config.class", b"not OptiFine")]),
        )
        .unwrap();
        assert_eq!(
            service.optifine_games(&workspace.path, &control).unwrap(),
            ["1.18.2"]
        );
        control.cancel();
        assert!(service.optifine_games(&workspace.path, &control).is_err());
    }
}
