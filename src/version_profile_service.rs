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
        if let Some(libraries) = optional(&mut object, "libraries") {
            let libraries = libraries.as_array().context("字段 libraries 必须为数组")?;
            for (index, library) in libraries.iter().enumerate() {
                if !library.is_object() {
                    bail!("字段 libraries[{index}] 必须为对象");
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
