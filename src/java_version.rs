use anyhow::{bail, Context, Result};
use std::cmp::Ordering;

/// Numeric release components are retained independently of the original vendor text.
#[derive(Clone, Debug)]
pub struct JavaVersion {
    numbers: Vec<u64>,
    preview: bool,
    build: Option<u64>,
    text: String,
}
impl JavaVersion {
    fn number(value: &str) -> Result<u64> {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("Java 版本号必须为十进制数字");
        }
        value.parse().context("Java 版本号超出范围")
    }
    pub fn parse(text: &str) -> Result<Self> {
        if text.is_empty() || text.chars().any(|c| c.is_whitespace() || c.is_control()) {
            bail!("Java 版本字符串无效");
        }
        let (release, build_suffix) = match text.split_once('+') {
            Some((release, build)) => (release, Some(build)),
            None => (text, None),
        };
        let (base, suffix) = release
            .split_once('-')
            .map_or((release, None), |(base, suffix)| (base, Some(suffix)));
        let legacy = base.starts_with("1.") || base.contains('u');
        let normalized = if let Some(legacy) = base.strip_prefix("1.") {
            legacy.replace('_', ".")
        } else if let Some((major, update)) = base.split_once('u') {
            format!("{major}.0.{update}")
        } else {
            base.to_owned()
        };
        let numbers = normalized
            .split('.')
            .map(Self::number)
            .collect::<Result<Vec<_>>>()
            .context("Java 版本数字格式错误")?;
        if numbers.first().is_none_or(|major| *major == 0) || base == "1" {
            bail!("Java 主版本无效");
        }
        let valid_suffix = |value: &str| {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_'))
        };
        if suffix.is_some_and(|suffix| !valid_suffix(suffix)) {
            bail!("Java 预览版本标记无效");
        }
        let legacy_build = suffix
            .and_then(|suffix| suffix.strip_prefix('b'))
            .filter(|build| {
                legacy && !build.is_empty() && build.bytes().all(|c| c.is_ascii_digit())
            });
        let build = if let Some(build) = build_suffix {
            let (number, vendor) = build
                .split_once('-')
                .map_or((build, None), |(number, vendor)| (number, Some(vendor)));
            if vendor.is_some_and(|vendor| !valid_suffix(vendor)) {
                bail!("Java 构建版本标记无效");
            }
            Some(Self::number(number).context("Java 构建号无效")?)
        } else {
            legacy_build
                .map(str::parse::<u64>)
                .transpose()
                .context("Java 构建号无效")?
        };
        Ok(Self {
            numbers,
            preview: suffix.is_some() && legacy_build.is_none(),
            build,
            text: text.into(),
        })
    }
    pub fn from_output(output: &str) -> Result<(Self, String)> {
        for line in output.lines().map(str::trim) {
            let Some(version) = line
                .strip_prefix("java version ")
                .or_else(|| line.strip_prefix("openjdk version "))
                .or_else(|| line.strip_prefix("openjdk "))
            else {
                continue;
            };
            let token = if let Some(quoted) = version.strip_prefix('"') {
                quoted.split_once('"').context("Java 版本缺少结束引号")?.0
            } else {
                version.split_whitespace().next().context("Java 版本为空")?
            };
            return Ok((Self::parse(token)?, line.into()));
        }
        bail!("无法识别 Java 版本");
    }
    pub fn major(&self) -> u64 {
        self.numbers[0]
    }
    pub fn is_preview(&self) -> bool {
        self.preview
    }
    pub fn compare(&self, other: &Self) -> Ordering {
        let release = (0..self.numbers.len().max(other.numbers.len()))
            .map(|index| {
                self.numbers
                    .get(index)
                    .unwrap_or(&0)
                    .cmp(other.numbers.get(index).unwrap_or(&0))
            })
            .find(|order| *order != Ordering::Equal)
            .unwrap_or(Ordering::Equal);
        release
            .then_with(|| other.preview.cmp(&self.preview))
            .then_with(|| self.build.unwrap_or(0).cmp(&other.build.unwrap_or(0)))
    }
}
impl std::fmt::Display for JavaVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_legacy_update_modern_build_and_early_access_forms() {
        for (text, major, preview) in [
            ("1.8.0_321-b07", 8, false),
            ("8u451", 8, false),
            ("17.0.12+7-LTS", 17, false),
            ("21-ea+35", 21, true),
            ("25.0.1", 25, false),
        ] {
            let version = JavaVersion::parse(text).unwrap();
            assert_eq!(version.major(), major);
            assert_eq!(version.is_preview(), preview);
            assert_eq!(version.to_string(), text);
        }
    }
    #[test]
    fn compares_update_numbers_numerically_and_ignores_trailing_zero_components() {
        for (older, newer) in [
            ("1.8.0_51", "1.8.0_321"),
            ("17.0.9", "17.0.10"),
            ("21-ea+99", "21"),
            ("17.0.12+7", "17.0.12+8"),
        ] {
            assert_eq!(
                JavaVersion::parse(older)
                    .unwrap()
                    .compare(&JavaVersion::parse(newer).unwrap()),
                Ordering::Less
            );
        }
        assert_eq!(
            JavaVersion::parse("17")
                .unwrap()
                .compare(&JavaVersion::parse("17.0.0").unwrap()),
            Ordering::Equal
        );
    }
    #[test]
    fn ignores_environment_warning_lines_when_reading_the_version_header() {
        let (version, description) = JavaVersion::from_output("Picked up JAVA_TOOL_OPTIONS: -Dlabel=version \"99\"\nopenjdk version \"21.0.7\" 2025-04-15\nOpenJDK Runtime Environment\n").unwrap();
        assert_eq!(version.major(), 21);
        assert!(description.starts_with("openjdk version"));
        assert_eq!(
            JavaVersion::from_output("openjdk 17.0.10 2024-01-16")
                .unwrap()
                .0
                .major(),
            17
        );
    }
    #[test]
    fn malformed_versions_and_truncated_headers_are_rejected() {
        for text in [
            "",
            "1",
            "0.8",
            "17..1",
            "17+",
            "17++1",
            "17-ea+abc",
            "17-",
            "17 0",
            "8u",
            "1.8.0_bad",
        ] {
            assert!(JavaVersion::parse(text).is_err(), "{text}");
        }
        assert!(JavaVersion::from_output("java version \"21").is_err());
        assert!(JavaVersion::from_output("warning: version \"21\"").is_err());
    }
}
