use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};

/// Validate the containers consumed by inheritance before indexing/merging.
/// Null optional values mean "unspecified", matching nullable launcher profiles.
pub(crate) struct VersionProfileDecoder;
impl VersionProfileDecoder {
    pub(crate) fn decode(&self, value: Value, id: &str) -> Result<Value> {
        self.decode_inner(value)
            .with_context(|| format!("版本 {id} 元数据无效"))
    }
    fn decode_inner(&self, value: Value) -> Result<Value> {
        let mut object = match value {
            Value::Object(object) => object,
            _ => bail!("版本元数据必须为 JSON 对象"),
        };
        for key in [
            "id",
            "inheritsFrom",
            "jar",
            "mainClass",
            "minecraftArguments",
            "assets",
            "type",
        ] {
            if let Some(value) = optional(&mut object, key) {
                if !value.is_string() {
                    bail!("字段 {key} 必须为字符串");
                }
            }
        }
        for key in ["assetIndex", "logging", "javaVersion"] {
            if let Some(value) = optional(&mut object, key) {
                if !value.is_object() {
                    bail!("字段 {key} 必须为对象");
                }
            }
        }
        if let Some(java) = object.get("javaVersion") {
            if let Some(major) = java.get("majorVersion") {
                if !major.as_u64().is_some_and(|major| major > 0) {
                    bail!("字段 javaVersion.majorVersion 必须为正整数");
                }
            }
            if let Some(component) = java.get("component").filter(|value| !value.is_null()) {
                let component = component
                    .as_str()
                    .filter(|component| !component.trim().is_empty())
                    .context("字段 javaVersion.component 必须为非空字符串")?;
                crate::metadata::valid_id(component)
                    .context("字段 javaVersion.component 不是有效组件名称")?;
            }
        }
        if let Some(libraries) = optional(&mut object, "libraries") {
            let libraries = libraries.as_array().context("字段 libraries 必须为数组")?;
            for (index, library) in libraries.iter().enumerate() {
                if !library.is_object() {
                    bail!("字段 libraries[{index}] 必须为对象");
                }
                if let Some(rules) = library.get("rules").filter(|value| !value.is_null()) {
                    let rules = rules
                        .as_array()
                        .with_context(|| format!("字段 libraries[{index}].rules 必须为数组"))?;
                    for (rule_index, rule) in rules.iter().enumerate() {
                        validate_rule(rule).with_context(|| {
                            format!("字段 libraries[{index}].rules[{rule_index}] 无效")
                        })?;
                    }
                }
            }
        }
        if let Some(downloads) = optional(&mut object, "downloads") {
            let downloads = downloads
                .as_object_mut()
                .context("字段 downloads 必须为对象")?;
            downloads.retain(|_, entry| !entry.is_null());
            for (kind, entry) in downloads {
                if !entry.is_object() {
                    bail!("字段 downloads.{kind} 必须为对象");
                }
            }
        }
        if let Some(arguments) = optional(&mut object, "arguments") {
            let arguments = arguments
                .as_object_mut()
                .context("字段 arguments 必须为对象")?;
            for kind in ["jvm", "game"] {
                let Some(values) = optional(arguments, kind) else {
                    continue;
                };
                let values = values
                    .as_array_mut()
                    .with_context(|| format!("字段 arguments.{kind} 必须为数组"))?;
                for (index, argument) in values.iter_mut().enumerate() {
                    match argument {
                        Value::String(_) => {}
                        Value::Object(object) => {
                            if let Some(rules) = optional(object, "rules") {
                                if !rules.is_array() {
                                    bail!("字段 arguments.{kind}[{index}].rules 必须为数组");
                                }
                                for (rule_index, rule) in
                                    rules.as_array().unwrap().iter().enumerate()
                                {
                                    validate_rule(rule).with_context(|| format!("字段 arguments.{kind}[{index}].rules[{rule_index}] 无效"))?;
                                }
                            }
                            // HMCL's RuledArgument also accepts the plural
                            // spelling, with values taking precedence over value.
                            if let Some(values) = object.remove("values") {
                                object.insert("value".into(), values);
                            }
                            let valid = match object.get("value") {
                                Some(Value::String(_)) => true,
                                Some(Value::Array(values)) => values.iter().all(Value::is_string),
                                _ => false,
                            };
                            if !valid {
                                bail!(
                                    "字段 arguments.{kind}[{index}].value 必须为字符串或字符串数组"
                                );
                            }
                        }
                        _ => bail!("字段 arguments.{kind}[{index}] 必须为字符串或规则对象"),
                    }
                }
            }
        }
        Ok(Value::Object(object))
    }
}
fn validate_rule(rule: &Value) -> Result<()> {
    let rule = rule.as_object().context("规则必须为对象")?;
    if !rule.get("action").is_some_and(Value::is_string) {
        bail!("规则 action 必须为字符串");
    }
    if let Some(features) = rule.get("features").filter(|value| !value.is_null()) {
        let features = features.as_object().context("规则 features 必须为对象")?;
        if !features.values().all(Value::is_boolean) {
            bail!("规则 features 的值必须为布尔值");
        }
    }
    if let Some(os) = rule.get("os").filter(|value| !value.is_null()) {
        let os = os.as_object().context("规则 os 必须为对象")?;
        for field in ["name", "arch", "version"] {
            if os
                .get(field)
                .is_some_and(|value| !value.is_null() && !value.is_string())
            {
                bail!("规则 os.{field} 必须为字符串");
            }
        }
    }
    Ok(())
}
fn optional<'a>(object: &'a mut Map<String, Value>, key: &str) -> Option<&'a mut Value> {
    if object.get(key).is_some_and(Value::is_null) {
        object.remove(key);
    }
    object.get_mut(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn nullable_optional_fields_are_absent_without_erasing_unknown_data() {
        let decoded = VersionProfileDecoder.decode(json!({"inheritsFrom":null,"jar":null,"libraries":null,"arguments":{"jvm":null,"game":[]},"downloads":{"client":null,"server":{}},"future":{"keep":null}}), "test").unwrap();
        assert!(decoded.get("inheritsFrom").is_none());
        assert!(decoded.get("jar").is_none());
        assert!(decoded.get("libraries").is_none());
        assert!(decoded["arguments"].get("jvm").is_none());
        assert!(decoded["downloads"].get("client").is_none());
        assert_eq!(decoded["arguments"]["game"], json!([]));
        assert_eq!(decoded["future"], json!({"keep":null}));
        let decoded = VersionProfileDecoder.decode(json!({"arguments":{"game":[{"rules":null,"values":["--alias","literal"],"value":"ignored"},{"value":"--unconditional"}]}}), "test").unwrap();
        assert_eq!(
            crate::metadata::arguments(&decoded["arguments"]["game"]),
            ["--alias", "literal", "--unconditional"]
        );
    }
    #[test]
    fn library_rule_shapes_are_checked_even_for_other_platforms() {
        for rules in [
            json!("allow"),
            json!([null]),
            json!([{"action":"allow","os":{"name":"osx","version":10}}]),
            json!([{"action":"allow","features":{"future":0}}]),
        ] {
            let error = VersionProfileDecoder
                .decode(
                    json!({"libraries":[{"name":"test:lib:1","rules":rules}]}),
                    "broken-library",
                )
                .unwrap_err();
            assert!(format!("{error:#}").contains("libraries[0].rules"));
        }
        for rules in [
            Value::Null,
            json!([]),
            json!([{"action":"allow","os":{"name":"future-os"},"features":{"future":false}}]),
        ] {
            assert!(VersionProfileDecoder
                .decode(
                    json!({"libraries":[{"name":"test:lib:1","rules":rules}]}),
                    "valid-library"
                )
                .is_ok());
        }
    }
    #[test]
    fn argument_rule_shapes_fail_with_precise_location() {
        for rule in [
            json!(false),
            json!({"action":true}),
            json!({"action":"allow","features":{"demo":"false"}}),
            json!({"action":"allow","os":{"arch":64}}),
        ] {
            let error = VersionProfileDecoder
                .decode(
                    json!({"arguments":{"game":[{"rules":[rule],"value":"--example"}]}}),
                    "broken-rules",
                )
                .unwrap_err();
            assert!(format!("{error:#}").contains("arguments.game[0].rules[0]"));
        }
        assert!(VersionProfileDecoder.decode(json!({"arguments":{"game":[{"rules":[{"action":"allow","features":{"future_feature":true},"os":null}],"value":"--example"}]}}), "future").is_ok());
    }
    #[test]
    fn java_requirements_validate_values_without_requiring_optional_fields() {
        for major in [json!(0), json!(-1), json!("21"), json!(21.5), Value::Null] {
            let error = VersionProfileDecoder
                .decode(json!({"javaVersion":{"majorVersion":major}}), "broken-java")
                .unwrap_err();
            assert!(format!("{error:#}").contains("javaVersion.majorVersion"));
        }
        for component in [json!(""), json!("  "), json!("../runtime"), json!(21)] {
            assert!(VersionProfileDecoder
                .decode(
                    json!({"javaVersion":{"component":component}}),
                    "broken-java"
                )
                .is_err());
        }
        for java in [
            json!({}),
            json!({"component":null}),
            json!({"component":"custom-runtime"}),
            json!({"majorVersion":25}),
        ] {
            assert!(VersionProfileDecoder
                .decode(json!({"javaVersion":java}), "valid-java")
                .is_ok());
        }
    }
    #[test]
    fn malformed_containers_fail_with_profile_and_field_context() {
        for value in [
            json!({"inheritsFrom":3}),
            json!({"jar":[]}),
            json!({"arguments":"bad"}),
            json!({"arguments":{"jvm":{}}}),
            json!({"arguments":{"game":[3]}}),
            json!({"arguments":{"game":[{"rules":[],"value":[3]}]}}),
            json!({"arguments":{"game":[{"rules":3,"value":"x"}]}}),
            json!({"libraries":{}}),
            json!({"libraries":[3]}),
            json!({"downloads":[]}),
            json!({"downloads":{"client":"bad"}}),
        ] {
            let error = VersionProfileDecoder.decode(value, "broken").unwrap_err();
            assert!(format!("{error:#}").contains("broken"));
            assert!(format!("{error:#}").contains("字段"));
        }
    }
}
