use crate::task::TaskControl;
use anyhow::{bail, Context, Result};
use serde_json::Value;

pub(crate) trait MinecraftProfileProvider {
    /// This request also refreshes subscription entitlements. An empty list
    /// alone is not sufficient to reject an otherwise valid game profile.
    fn activate_entitlements(&self) -> Result<()>;
    /// Only HTTP 404 is represented by None; authentication/network failures
    /// must remain errors, never be interpreted as an absent player profile.
    fn profile(&self) -> Result<Option<Value>>;
    fn license(&self) -> Result<Value>;
}

pub(crate) struct MinecraftProfileService;
impl MinecraftProfileService {
    pub(crate) fn fetch(
        &self,
        provider: &impl MinecraftProfileProvider,
        control: &TaskControl,
    ) -> Result<Value> {
        control.check()?;
        provider.activate_entitlements()?;
        control.check()?;
        if let Some(profile) = provider.profile()? {
            control.check()?;
            return Ok(profile);
        }
        control.check()?;
        let license = provider.license()?;
        control.check()?;
        let items = license["items"]
            .as_array()
            .context("Minecraft 许可证响应缺少有效的 items 列表")?;
        if items.iter().any(|item| item["name"] == "game_minecraft") {
            bail!("账户拥有 Minecraft Java 版资格，但尚未创建玩家档案。请在 Minecraft 官网设置 Java 版角色名后重新登录：https://www.minecraft.net/msaprofile");
        }
        bail!(
            "该账户没有有效的 Minecraft Java 版资格，请检查是否已购买游戏或 Game Pass 订阅已到期"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    struct Provider {
        calls: RefCell<Vec<&'static str>>,
        missing: bool,
        license: Value,
        fail: &'static str,
    }
    impl MinecraftProfileProvider for Provider {
        fn activate_entitlements(&self) -> Result<()> {
            self.calls.borrow_mut().push("activate");
            if self.fail == "activate" {
                bail!("activation unavailable");
            }
            Ok(())
        }
        fn profile(&self) -> Result<Option<Value>> {
            self.calls.borrow_mut().push("profile");
            if self.fail == "profile" {
                bail!("profile HTTP 401");
            }
            Ok((!self.missing).then(|| json!({"id":"player-id", "name":"Player"})))
        }
        fn license(&self) -> Result<Value> {
            self.calls.borrow_mut().push("license");
            if self.fail == "license" {
                bail!("license HTTP 503");
            }
            Ok(self.license.clone())
        }
    }
    fn provider(missing: bool, license: Value, fail: &'static str) -> Provider {
        Provider {
            calls: RefCell::new(vec![]),
            missing,
            license,
            fail,
        }
    }
    #[test]
    fn active_profile_requires_activation_but_not_an_extra_license_request() {
        let p = provider(false, Value::Null, "");
        let profile = MinecraftProfileService
            .fetch(&p, &TaskControl::default())
            .unwrap();
        assert_eq!(profile["name"], "Player");
        assert_eq!(*p.calls.borrow(), ["activate", "profile"]);
    }
    #[test]
    fn missing_profile_distinguishes_license_from_missing_player_name() {
        for (items, message) in [
            (json!([{ "name":"game_minecraft" }]), "尚未创建玩家档案"),
            (json!([]), "没有有效的 Minecraft Java 版资格"),
            (
                json!([{ "name":"unrelated_product" }]),
                "没有有效的 Minecraft Java 版资格",
            ),
        ] {
            let p = provider(true, json!({"items":items}), "");
            let error = MinecraftProfileService
                .fetch(&p, &TaskControl::default())
                .unwrap_err();
            assert!(error.to_string().contains(message));
            assert_eq!(*p.calls.borrow(), ["activate", "profile", "license"]);
        }
        let p = provider(true, json!({"items":null}), "");
        assert!(MinecraftProfileService
            .fetch(&p, &TaskControl::default())
            .unwrap_err()
            .to_string()
            .contains("响应"));
    }
    #[test]
    fn service_failures_and_cancellation_do_not_fall_back_to_no_license() {
        for (stage, calls) in [
            ("activate", vec!["activate"]),
            ("profile", vec!["activate", "profile"]),
            ("license", vec!["activate", "profile", "license"]),
        ] {
            let p = provider(true, json!({"items":[]}), stage);
            let error = MinecraftProfileService
                .fetch(&p, &TaskControl::default())
                .unwrap_err();
            assert!(!error.to_string().contains("没有有效"));
            assert_eq!(*p.calls.borrow(), calls);
        }
        let p = provider(false, Value::Null, "");
        let control = TaskControl::default();
        control.cancel();
        assert!(MinecraftProfileService.fetch(&p, &control).is_err());
        assert!(p.calls.borrow().is_empty());
    }
}
