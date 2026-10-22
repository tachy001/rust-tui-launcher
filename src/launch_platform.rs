use crate::{
    java_architecture::JavaArchitecture, library_service::LibraryResolver,
    platform_rule_service::PlatformRuleEvaluator,
};
use serde_json::Value;
use std::collections::HashMap;

/// One JVM target shared by dependency selection, loader policy and launch arguments.
pub(crate) struct LaunchPlatform {
    architecture: JavaArchitecture,
}
impl LaunchPlatform {
    pub(crate) fn new(architecture: JavaArchitecture) -> Self {
        Self { architecture }
    }
    pub(crate) fn rules(&self) -> PlatformRuleEvaluator<'static> {
        PlatformRuleEvaluator::for_java(self.architecture.rule_name())
    }
    pub(crate) fn libraries(&self) -> LibraryResolver<'static> {
        LibraryResolver::new(crate::metadata::os_name(), self.architecture.bits())
    }
    pub(crate) fn profile(&self, source: &Value) -> Value {
        let mut profile = source.clone();
        if let Some(libraries) = source["libraries"].as_array() {
            let rules = self.rules();
            profile["libraries"] = Value::Array(
                libraries
                    .iter()
                    .filter(|lib| rules.allowed(lib, &HashMap::new()))
                    .map(|lib| {
                        let mut lib = lib.clone();
                        // The target has already been evaluated. Downstream loader inspectors
                        // must not evaluate these rules again against the launcher architecture.
                        if let Some(object) = lib.as_object_mut() {
                            object.remove("rules");
                        }
                        lib
                    })
                    .collect(),
            );
        }
        profile
    }
    pub(crate) fn arguments(&self, value: &Value, features: &HashMap<&str, bool>) -> Vec<String> {
        crate::metadata::arguments_for(value, features, &self.rules())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn jvm_bitness_drives_native_classifier_and_rules_together() {
        let profile = json!({"libraries":[
            {"name":"test:binding:1","natives":{crate::metadata::os_name():"natives-${arch}"}},
            {"name":"test:x86:1","rules":[{"action":"allow","os":{"arch":"x86"}}]},
            {"name":"test:arm:1","rules":[{"action":"allow","os":{"arch":"aarch64"}}]}
        ]});
        for (arch, bits, selected) in [
            (JavaArchitecture::X86, "32", "test:x86:1"),
            (JavaArchitecture::Arm64, "64", "test:arm:1"),
        ] {
            let platform = LaunchPlatform::new(arch);
            let target = platform.profile(&profile);
            let libraries = target["libraries"].as_array().unwrap();
            assert_eq!(libraries.len(), 2);
            assert_eq!(libraries[1]["name"], selected);
            assert!(libraries[1].get("rules").is_none());
            let artifacts = platform.libraries().resolve(&libraries[0]).unwrap();
            assert!(artifacts[0].1);
            assert!(artifacts[0].0["path"]
                .as_str()
                .unwrap()
                .ends_with(&format!("-natives-{bits}.jar")));
        }
        assert!(profile["libraries"][1].get("rules").is_some());
    }
    #[test]
    fn jvm_and_game_arguments_keep_feature_rules_on_the_jvm_target() {
        let args = json!([
            "common",
            {"rules":[{"action":"allow","os":{"arch":"x86"}}],"value":"-Xss1M"},
            {"rules":[{"action":"allow","os":{"arch":"aarch64"},"features":{"has_custom_resolution":true}}],"value":["--width","${resolution_width}"]}
        ]);
        assert_eq!(
            LaunchPlatform::new(JavaArchitecture::X86).arguments(&args, &HashMap::new()),
            ["common", "-Xss1M"]
        );
        let arm = LaunchPlatform::new(JavaArchitecture::Arm64);
        assert_eq!(arm.arguments(&args, &HashMap::new()), ["common"]);
        assert_eq!(
            arm.arguments(&args, &HashMap::from([("has_custom_resolution", true)])),
            ["common", "--width", "${resolution_width}"]
        );
    }
}
