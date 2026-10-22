use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{fs, path::Path};

/// A launcher preference; `None` leaves the player's game options untouched.
pub(crate) struct GameLanguage(Option<String>);
impl GameLanguage {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let value = value.trim().to_lowercase().replace('-', "_");
        let value = match value.as_str() {
            "" | "keep" => return Ok(Self(None)),
            "launcher" => return Ok(Self(Some("launcher".into()))),
            "简体中文" => "zh_cn",
            "繁體中文" | "繁体中文" => "zh_tw",
            "english" => "en_us",
            _ => &value,
        };
        let mut parts = value.split('_');
        let language = parts.next().unwrap_or_default();
        let region = parts.next();
        if !(2..=3).contains(&language.len())
            || !language.bytes().all(|c| c.is_ascii_lowercase())
            || region.is_some_and(|r| {
                !(2..=8).contains(&r.len()) || !r.bytes().all(|c| c.is_ascii_alphanumeric())
            })
            || parts.next().is_some()
        {
            bail!("游戏语言应为 zh_cn、zh_tw、en_us 等语言代码，或 keep（保持游戏原设置）");
        }
        Ok(Self(Some(value.into())))
    }

    pub(crate) fn for_version(&self, base: &str, meta: &Value) -> Option<String> {
        let preferred = self.0.as_deref()?;
        let modern = language_format(base)
            .or_else(|| meta["assets"].as_str().and_then(language_format))
            .or_else(|| meta["assetIndex"]["id"].as_str().and_then(language_format))
            .or_else(|| {
                // The 2016 transition is resolved only with a known version/asset ID.
                let year: u32 = meta["releaseTime"].as_str()?.get(..4)?.parse().ok()?;
                match year {
                    2012..=2015 => Some(Some(false)),
                    2017.. => Some(Some(true)),
                    _ => None,
                }
            })??;
        let before_classical_chinese = std::iter::once(base)
            .chain(meta["assets"].as_str())
            .chain(meta["assetIndex"]["id"].as_str())
            .find_map(|version| {
                let mut parts = version.split('-').next()?.split('.');
                (parts.next()? == "1")
                    .then(|| parts.next()?.parse::<u32>().ok())
                    .flatten()
            })
            .is_some_and(|minor| minor < 16);
        let language = if preferred == "lzh" && (!modern || before_classical_chinese) {
            "zh_cn"
        } else {
            preferred
        };
        if modern {
            Some(language.into())
        } else if let Some((prefix, region)) = language.split_once('_') {
            Some(format!("{prefix}_{}", region.to_uppercase()))
        } else {
            // Old language registries use region-qualified codes exclusively.
            None
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.0.is_some()
    }

    pub(crate) fn resolve(self, launcher_language: &str) -> Result<Self> {
        if self.0.as_deref() == Some("launcher") {
            return Self::parse(
                crate::localization::UiLanguage::parse(launcher_language)?.game_code(),
            );
        }
        Ok(self)
    }
}

// Outer None = unknown; inner None = known version without language support.
fn language_format(version: &str) -> Option<Option<bool>> {
    if (version.starts_with(['a', 'b'])
        && version.as_bytes().get(1).is_some_and(u8::is_ascii_digit))
        || version.starts_with("rd-")
    {
        return Some(None);
    }
    let release = version.split('-').next()?;
    let numbers: Vec<u32> = release
        .split('.')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    match numbers.as_slice() {
        [1, 0, ..] => Some(None),
        [1, minor, ..] => Some(Some(*minor >= 11)),
        [major, ..] if *major >= 26 => Some(Some(true)),
        _ => None,
    }
}

/// Edits only `lang`, preserving unrelated options and their original line endings.
pub(crate) struct GameOptionsService;
impl GameOptionsService {
    pub(crate) fn legacy_language_available(
        &self,
        jar: &Path,
        index: &Value,
        language: &str,
    ) -> Result<bool> {
        let paths = [
            format!("lang/{language}.lang"),
            format!("assets/minecraft/lang/{language}.lang"),
        ];
        if index["objects"].as_object().is_some_and(|objects| {
            objects.contains_key(&format!("minecraft/lang/{language}.lang"))
                || objects.contains_key(&paths[0])
        }) {
            return Ok(true);
        }
        let mut archive =
            zip::ZipArchive::new(fs::File::open(jar)?).context("检查旧版游戏可用语言失败")?;
        for path in paths {
            match archive.by_name(&path) {
                Ok(_) => return Ok(true),
                Err(zip::result::ZipError::FileNotFound) => {}
                Err(error) => return Err(error).context("读取旧版游戏语言条目失败"),
            }
        }
        Ok(false)
    }

    pub(crate) fn synchronize(&self, directory: &Path, language: &str) -> Result<bool> {
        let path = directory.join("options.txt");
        let original = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error).context("读取游戏语言设置失败，原文件未修改"),
        };
        let updated = Self::update(&original, language);
        if original == updated {
            return Ok(false);
        }
        crate::storage::atomic_write(&path, updated.as_bytes()).context("保存游戏语言设置失败")?;
        Ok(true)
    }

    fn update(original: &str, language: &str) -> String {
        let newline = if original.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let mut updated = String::with_capacity(original.len() + language.len() + 8);
        let mut found = false;
        for line in original.split_inclusive('\n') {
            let (bom, line) = if updated.is_empty() {
                line.strip_prefix('\u{feff}')
                    .map_or(("", line), |s| ("\u{feff}", s))
            } else {
                ("", line)
            };
            updated.push_str(bom);
            if line.starts_with("lang:") {
                let ending = if line.ends_with("\r\n") {
                    "\r\n"
                } else if line.ends_with('\n') {
                    "\n"
                } else {
                    ""
                };
                updated.push_str(&format!("lang:{language}{ending}"));
                found = true;
            } else {
                updated.push_str(line);
            }
        }
        if !found {
            if !updated.is_empty() && !updated.ends_with('\n') {
                updated.push_str(newline);
            }
            updated.push_str(&format!("lang:{language}{newline}"));
        }
        updated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn respects_legacy_modern_and_unsupported_versions() {
        let language = GameLanguage::parse("ZH-CN").unwrap();
        for version in ["1.1", "1.5.2", "1.10.2"] {
            assert_eq!(
                language.for_version(version, &json!({})).as_deref(),
                Some("zh_CN")
            );
        }
        for version in ["1.11", "1.21.1", "26.1"] {
            assert_eq!(
                language.for_version(version, &json!({})).as_deref(),
                Some("zh_cn")
            );
        }
        for version in ["1.0", "b1.7.3", "unknown"] {
            assert!(language.for_version(version, &json!({})).is_none());
        }
        assert_eq!(
            language
                .for_version("renamed", &json!({"assets":"1.10"}))
                .as_deref(),
            Some("zh_CN")
        );
        assert_eq!(
            language
                .for_version("snapshot", &json!({"releaseTime":"2026-01-01T00:00:00Z"}))
                .as_deref(),
            Some("zh_cn")
        );
        assert!(GameLanguage::parse("keep")
            .unwrap()
            .for_version("1.21", &json!({}))
            .is_none());
        assert_eq!(
            language
                .for_version(
                    "renamed",
                    &json!({"assetIndex":{"id":"1"},"releaseTime":"2022-01-01T00:00:00Z"})
                )
                .as_deref(),
            Some("zh_cn")
        );
    }

    #[test]
    fn follows_launcher_language_unless_explicitly_overridden() {
        for (ui, expected) in [("en_us", "en_us"), ("zh_cn", "zh_cn")] {
            assert_eq!(
                GameLanguage::parse("launcher")
                    .unwrap()
                    .resolve(ui)
                    .unwrap()
                    .for_version("1.21", &json!({}))
                    .as_deref(),
                Some(expected)
            );
            assert_eq!(
                GameLanguage::parse("zh_tw")
                    .unwrap()
                    .resolve(ui)
                    .unwrap()
                    .for_version("1.21", &json!({}))
                    .as_deref(),
                Some("zh_tw")
            );
        }
        for language in crate::localization::UiLanguage::ALL {
            assert_eq!(
                GameLanguage::parse("launcher")
                    .unwrap()
                    .resolve(language.game_code())
                    .unwrap()
                    .for_version("1.21", &json!({}))
                    .as_deref(),
                Some(language.game_code())
            );
        }
        let classical = GameLanguage::parse("lzh").unwrap();
        assert_eq!(
            classical.for_version("1.15.2", &json!({})).as_deref(),
            Some("zh_cn")
        );
        assert_eq!(
            classical.for_version("1.10.2", &json!({})).as_deref(),
            Some("zh_CN")
        );
    }

    #[test]
    fn edits_only_language_with_bom_mixed_endings_and_duplicate_keys() {
        let original = "\u{feff}lang:en_us\r\nfov:0.0\nlang:en_us\nkey_key.forward:key.keyboard.w";
        let expected = "\u{feff}lang:zh_cn\r\nfov:0.0\nlang:zh_cn\nkey_key.forward:key.keyboard.w";
        let updated = GameOptionsService::update(original, "zh_cn");
        assert_eq!(updated, expected);
        assert_eq!(GameOptionsService::update(&updated, "zh_cn"), updated);
        assert_eq!(
            GameOptionsService::update("fov:0.0\r\nfullscreen:false", "zh_cn"),
            "fov:0.0\r\nfullscreen:false\r\nlang:zh_cn\r\n"
        );
    }

    #[test]
    fn validates_language_without_allowing_option_injection() {
        for value in [
            "zh_cn",
            "zh_tw",
            "en-us",
            "es_419",
            "tok",
            "简体中文",
            "keep",
            "",
        ] {
            assert!(GameLanguage::parse(value).is_ok(), "{value}");
        }
        for value in ["zh_cn\nfullscreen:true", "../en_us", "x_y", "en__us"] {
            assert!(GameLanguage::parse(value).is_err(), "{value}");
        }
    }

    #[test]
    fn file_sync_creates_updates_and_preserves_invalid_file() {
        let workspace = crate::test_support::Workspace::new();
        let service = GameOptionsService;
        let path = workspace.path.join("options.txt");
        assert!(service.synchronize(&workspace.path, "zh_cn").unwrap());
        assert!(!service.synchronize(&workspace.path, "zh_cn").unwrap());
        assert!(service.synchronize(&workspace.path, "en_us").unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "lang:en_us\n");
        fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(service.synchronize(&workspace.path, "zh_cn").is_err());
        assert_eq!(fs::read(&path).unwrap(), [0xff, 0xfe]);
    }

    #[test]
    fn legacy_language_must_exist_in_client_or_assets() {
        use std::io::Write;
        let workspace = crate::test_support::Workspace::new();
        let jar = workspace.path.join("client.jar");
        let mut zip = zip::ZipWriter::new(fs::File::create(&jar).unwrap());
        zip.start_file("lang/zh_CN.lang", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"test=value").unwrap();
        zip.finish().unwrap();
        let service = GameOptionsService;
        assert!(service
            .legacy_language_available(&jar, &json!({}), "zh_CN")
            .unwrap());
        assert!(!service
            .legacy_language_available(&jar, &json!({}), "foo_BAR")
            .unwrap());
        assert!(service
            .legacy_language_available(
                &jar,
                &json!({"objects":{"minecraft/lang/zh_TW.lang":{}}}),
                "zh_TW"
            )
            .unwrap());
    }
}
