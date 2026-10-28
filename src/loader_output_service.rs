use crate::{metadata::valid_id, task::TaskControl};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{fs, io::Read, path::Path};

/// Required client profile from the verified installer, independent of exit code.
pub(crate) struct LoaderOutputRequirement {
    profile: Value,
    id: String,
    game: String,
}
impl LoaderOutputRequirement {
    pub(crate) fn new(profile: &Value, game: &str) -> Result<Self> {
        let id = profile["id"].as_str().context("现代加载器缺少实例 ID")?;
        valid_id(id)?;
        valid_id(game)?;
        let normalized = crate::version_profile_service::VersionProfileDecoder
            .decode(profile.clone(), id)
            .context("加载器安装包的版本配置无效")?;
        if id == game
            || profile
                .get("inheritsFrom")
                .filter(|value| !value.is_null())
                .is_some_and(|value| value.as_str() != Some(game))
        {
            bail!("现代加载器父版本不匹配");
        }
        if !profile["mainClass"].as_str().is_some_and(|main| {
            !main.is_empty() && !main.chars().any(|c| c.is_whitespace() || c.is_control())
        }) {
            bail!("现代加载器描述缺少有效启动入口");
        }
        let libraries = profile["libraries"]
            .as_array()
            .context("现代加载器描述缺少依赖列表")?;
        for library in libraries {
            crate::version::maven_path(
                library["name"].as_str().context("现代加载器依赖缺少坐标")?,
            )?;
        }
        Ok(Self {
            profile: normalized,
            id: id.into(),
            game: game.into(),
        })
    }

    pub(crate) fn verify(&self, root: &Path, control: &TaskControl) -> Result<()> {
        control.check()?;
        let path = root
            .join("versions")
            .join(&self.id)
            .join(format!("{}.json", self.id));
        let mut bytes = vec![];
        fs::File::open(&path)
            .context("安装器未生成目标实例配置")?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            bail!("安装器生成的实例配置超过 1 MiB 限制");
        }
        control.check()?;
        let actual: Value = serde_json::from_slice(&bytes).context("安装器生成的实例配置损坏")?;
        let actual = crate::version_profile_service::VersionProfileDecoder
            .decode(actual, &self.id)
            .context("安装器生成的版本配置格式无效")?;
        if actual["id"].as_str() != Some(&self.id)
            || actual["inheritsFrom"].as_str() != Some(&self.game)
        {
            bail!("安装器输出的实例或父版本与安装包不一致，不能判定安装完成");
        }
        for field in [
            "mainClass",
            "arguments",
            "minecraftArguments",
            "jar",
            "downloads",
            "javaVersion",
        ] {
            if let Some(expected) = self.profile.get(field).filter(|value| !value.is_null()) {
                if actual.get(field) != Some(expected) {
                    bail!("安装器输出缺少或改变了预期 {field}，不能判定加载器安装完成");
                }
            }
        }
        for field in ["arguments", "minecraftArguments"] {
            if self.profile.get(field).is_none_or(Value::is_null)
                && actual.get(field).is_some_and(|value| !value.is_null())
            {
                bail!("安装器输出新增了安装包未声明的 {field}，不能判定安装完成");
            }
        }
        if self.profile.get("jar").is_none_or(Value::is_null)
            && actual
                .get("jar")
                .filter(|value| !value.is_null())
                .is_some_and(|value| value.as_str() != Some(&self.game))
        {
            bail!("安装器输出改变了客户端 JAR 归属");
        }
        if self
            .profile
            .pointer("/downloads/client")
            .is_none_or(Value::is_null)
            && actual
                .pointer("/downloads/client")
                .is_some_and(|value| !value.is_null())
        {
            bail!("安装器输出新增了安装包未声明的客户端替换");
        }
        let libraries = actual["libraries"]
            .as_array()
            .context("安装器输出缺少加载器依赖列表")?;
        for expected in self.profile["libraries"].as_array().unwrap() {
            control.check()?;
            if !libraries.contains(expected) {
                bail!(
                    "安装器输出缺少或改变了加载器依赖 {}，不能判定安装完成",
                    expected["name"].as_str().unwrap_or("未知")
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn profile() -> Value {
        json!({"id":"loader", "inheritsFrom":"game", "mainClass":"cpw.mods.bootstraplauncher.BootstrapLauncher", "libraries":[{"name":"test:loader:1", "downloads":{"artifact":{"url":"","path":"test/loader/1/loader-1.jar"}}}], "arguments":{"game":["--launchTarget","forgeclient"]}})
    }
    fn write(root: &Path, value: &Value) {
        fs::create_dir_all(root.join("versions/loader")).unwrap();
        fs::write(
            root.join("versions/loader/loader.json"),
            serde_json::to_vec(value).unwrap(),
        )
        .unwrap();
    }
    #[test]
    fn only_expected_loader_output_can_complete_even_if_old_profile_resolves() {
        let workspace = crate::test_support::Workspace::new();
        let root = &workspace.path;
        fs::create_dir_all(root.join("versions/game")).unwrap();
        fs::write(
            root.join("versions/game/game.json"),
            br#"{"id":"game","mainClass":"net.minecraft.client.main.Main","libraries":[]}"#,
        )
        .unwrap();
        let required = LoaderOutputRequirement::new(&profile(), "game").unwrap();
        write(root, &json!({"id":"loader","inheritsFrom":"game"}));
        assert!(crate::version::resolve(root, "loader").is_ok());
        assert!(required.verify(root, &TaskControl::default()).is_err());
        write(root, &profile());
        required.verify(root, &TaskControl::default()).unwrap();
        let control = TaskControl::default();
        control.cancel();
        assert!(required.verify(root, &control).is_err());
    }
    #[test]
    fn equivalent_nullable_fields_and_argument_aliases_do_not_fail_output_validation() {
        let workspace = crate::test_support::Workspace::new();
        let mut expected = profile();
        expected["arguments"] =
            json!({"jvm":null,"game":[{"rules":null,"values":["--example","literal"]}]});
        let mut output = expected.clone();
        output["arguments"] = json!({"game":[{"value":["--example","literal"]}]});
        let required = LoaderOutputRequirement::new(&expected, "game").unwrap();
        write(&workspace.path, &output);
        required
            .verify(&workspace.path, &TaskControl::default())
            .unwrap();
        output["arguments"]["game"][0]["value"][1] = json!("changed");
        write(&workspace.path, &output);
        assert!(required
            .verify(&workspace.path, &TaskControl::default())
            .is_err());
    }
    #[test]
    fn invalid_containers_are_rejected_before_install_and_in_generated_output() {
        let workspace = crate::test_support::Workspace::new();
        for (field, value) in [
            ("javaVersion", json!("21")),
            ("arguments", json!({"jvm":"-Xmx2G"})),
            ("downloads", json!({"client":"client.jar"})),
            ("logging", json!([])),
            ("assetIndex", json!(false)),
        ] {
            let mut invalid = profile();
            invalid[field] = value;
            let error = LoaderOutputRequirement::new(&invalid, "game")
                .err()
                .unwrap();
            assert!(format!("{error:#}").contains(field));
            let requirement = LoaderOutputRequirement::new(&profile(), "game").unwrap();
            write(&workspace.path, &invalid);
            assert!(requirement
                .verify(&workspace.path, &TaskControl::default())
                .is_err());
        }
    }
    #[test]
    fn declared_java_requirement_must_survive_installer_output() {
        let workspace = crate::test_support::Workspace::new();
        let mut expected = profile();
        expected["javaVersion"] = json!({"majorVersion":21,"component":"java-runtime-delta"});
        let required = LoaderOutputRequirement::new(&expected, "game").unwrap();
        write(&workspace.path, &expected);
        required
            .verify(&workspace.path, &TaskControl::default())
            .unwrap();
        for changed in [
            Value::Null,
            json!({"majorVersion":8,"component":"jre-legacy"}),
            json!({"majorVersion":21,"component":"java-runtime-gamma"}),
        ] {
            let mut output = expected.clone();
            output["javaVersion"] = changed;
            write(&workspace.path, &output);
            let error = required
                .verify(&workspace.path, &TaskControl::default())
                .unwrap_err();
            assert!(error.to_string().contains("javaVersion"));
        }
        expected.as_object_mut().unwrap().remove("javaVersion");
        write(&workspace.path, &expected);
        assert!(required
            .verify(&workspace.path, &TaskControl::default())
            .is_err());
    }
    #[test]
    fn changed_entrypoint_arguments_parent_and_dependency_are_rejected() {
        let workspace = crate::test_support::Workspace::new();
        let required = LoaderOutputRequirement::new(&profile(), "game").unwrap();
        for (key, value) in [
            ("id", json!("other")),
            ("inheritsFrom", json!("other")),
            ("mainClass", json!("net.minecraft.client.main.Main")),
            ("arguments", json!({"game":[]})),
            ("libraries", json!([])),
        ] {
            let mut output = profile();
            output[key] = value;
            write(&workspace.path, &output);
            assert!(
                required
                    .verify(&workspace.path, &TaskControl::default())
                    .is_err(),
                "{key}"
            );
        }
        let mut output = profile();
        output["libraries"][0]["downloads"]["artifact"]["path"] = json!("other.jar");
        write(&workspace.path, &output);
        assert!(required
            .verify(&workspace.path, &TaskControl::default())
            .is_err());
    }
    #[test]
    fn additive_installer_fields_are_allowed_but_invalid_package_or_missing_output_is_not() {
        let workspace = crate::test_support::Workspace::new();
        let required = LoaderOutputRequirement::new(&profile(), "game").unwrap();
        assert!(required
            .verify(&workspace.path, &TaskControl::default())
            .is_err());
        let mut output = profile();
        output["time"] = json!("generated");
        output["libraries"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"test:extra:1"}));
        write(&workspace.path, &output);
        required
            .verify(&workspace.path, &TaskControl::default())
            .unwrap();
        for (key, value) in [
            ("jar", json!("unrelated")),
            ("minecraftArguments", json!("--unexpected")),
            (
                "downloads",
                json!({"client":{"url":"https://example.invalid/client.jar"}}),
            ),
        ] {
            let mut changed = output.clone();
            changed[key] = value;
            write(&workspace.path, &changed);
            assert!(
                required
                    .verify(&workspace.path, &TaskControl::default())
                    .is_err(),
                "{key}"
            );
        }
        for (key, value) in [
            ("mainClass", json!("")),
            ("libraries", json!(null)),
            ("inheritsFrom", json!("other")),
        ] {
            let mut invalid = profile();
            invalid[key] = value;
            assert!(LoaderOutputRequirement::new(&invalid, "game").is_err());
        }
    }
}
