use anyhow::{bail, Result};
use std::{cell::Cell, collections::HashMap, fmt::Display, sync::OnceLock};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum UiLanguage {
    Chinese,
    English,
    TraditionalChinese,
    Japanese,
    German,
    Spanish,
    Russian,
    Ukrainian,
    Arabic,
    ClassicalChinese,
}
impl UiLanguage {
    pub(crate) const ALL: [Self; 10] = [
        Self::Chinese,
        Self::English,
        Self::TraditionalChinese,
        Self::Japanese,
        Self::German,
        Self::Spanish,
        Self::Russian,
        Self::Ukrainian,
        Self::Arabic,
        Self::ClassicalChinese,
    ];
    pub(crate) fn parse(value: &str) -> Result<Self> {
        if Self::is_system(value) {
            return Ok(crate::system_locale_service::SystemLocaleResolver::new(
                &crate::system_locale_service::NativeLocales,
            )
            .resolve());
        }
        match value.trim().to_lowercase().replace('-', "_").as_str() {
            "zh" | "zh_cn" | "zh_hans" | "简体中文" => Ok(Self::Chinese),
            "en" | "en_us" | "en_gb" | "english" => Ok(Self::English),
            "zh_tw" | "zh_hk" | "zh_hant" | "繁體中文" => Ok(Self::TraditionalChinese),
            "ja" | "ja_jp" => Ok(Self::Japanese),
            "de" | "de_de" => Ok(Self::German),
            "es" | "es_es" => Ok(Self::Spanish),
            "ru" | "ru_ru" => Ok(Self::Russian),
            "uk" | "uk_ua" => Ok(Self::Ukrainian),
            "ar" | "ar_sa" => Ok(Self::Arabic),
            "lzh" => Ok(Self::ClassicalChinese),
            _ => bail!("Unknown UI language. Supported: auto, zh_cn, en_us, zh_tw, ja_jp, de_de, es_es, ru_ru, uk_ua, ar_sa, lzh"),
        }
    }
    pub(crate) fn is_system(value: &str) -> bool {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "auto" | "system" | "default" | "def"
        )
    }
    pub(crate) fn game_code(self) -> &'static str {
        match self {
            Self::Chinese => "zh_cn",
            Self::English => "en_us",
            Self::TraditionalChinese => "zh_tw",
            Self::Japanese => "ja_jp",
            Self::German => "de_de",
            Self::Spanish => "es_es",
            Self::Russian => "ru_ru",
            Self::Ukrainian => "uk_ua",
            Self::Arabic => "ar_sa",
            Self::ClassicalChinese => "lzh",
        }
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Chinese => "简体中文",
            Self::English => "English",
            Self::TraditionalChinese => "繁體中文",
            Self::Japanese => "日本語",
            Self::German => "Deutsch",
            Self::Spanish => "Español",
            Self::Russian => "Русский",
            Self::Ukrainian => "Українська",
            Self::Arabic => "العربية (experimental RTL)",
            Self::ClassicalChinese => "文言",
        }
    }
    fn catalog(self) -> &'static HashMap<String, String> {
        static CATALOGS: OnceLock<Vec<HashMap<String, String>>> = OnceLock::new();
        let catalogs = CATALOGS.get_or_init(|| {
            let mut catalogs: Vec<HashMap<String, String>> = [
                include_str!("../assets/lang/zh_cn.json"),
                include_str!("../assets/lang/en_us.json"),
                include_str!("../assets/lang/zh_tw.json"),
                include_str!("../assets/lang/ja_jp.json"),
                include_str!("../assets/lang/de_de.json"),
                include_str!("../assets/lang/es_es.json"),
                include_str!("../assets/lang/ru_ru.json"),
                include_str!("../assets/lang/uk_ua.json"),
                include_str!("../assets/lang/ar_sa.json"),
                include_str!("../assets/lang/lzh.json"),
            ]
            .into_iter()
            .map(|source| {
                serde_json::from_str(source).expect("validated built-in language catalog")
            })
            .collect();
            let english = catalogs[1].clone();
            for (index, catalog) in catalogs.iter_mut().enumerate() {
                if index != 1 {
                    for (key, value) in &english {
                        catalog.entry(key.clone()).or_insert_with(|| value.clone());
                    }
                }
            }
            catalogs
        });
        &catalogs[Self::ALL
            .iter()
            .position(|language| *language == self)
            .unwrap()]
    }
    pub(crate) fn coverage(self) -> (usize, usize) {
        (self.catalog().len(), Self::English.catalog().len())
    }
}

thread_local! { static CURRENT: Cell<UiLanguage> = const { Cell::new(UiLanguage::Chinese) }; }
pub(crate) fn set(value: &str) {
    CURRENT.set(UiLanguage::parse(value).unwrap_or(UiLanguage::Chinese));
}
pub(crate) fn tr(source: &str) -> &str {
    CURRENT
        .get()
        .catalog()
        .get(source)
        .or_else(|| UiLanguage::English.catalog().get(source))
        .map_or(source, String::as_str)
}

struct FormatValue {
    display: String,
    alternate: String,
}
impl FormatValue {
    fn new(value: &impl Display) -> Self {
        Self {
            display: format!("{value}"),
            alternate: format!("{value:#}"),
        }
    }
}
#[derive(Default)]
pub(crate) struct FormatArguments {
    positional: Vec<FormatValue>,
    named: HashMap<&'static str, FormatValue>,
}
impl FormatArguments {
    pub(crate) fn push(&mut self, value: &impl Display) {
        self.positional.push(FormatValue::new(value));
    }
    pub(crate) fn insert(&mut self, name: &'static str, value: &impl Display) {
        self.named.insert(name, FormatValue::new(value));
    }
    pub(crate) fn render(&mut self, source: &str) -> String {
        self.render_template(tr(source))
            .or_else(|| {
                UiLanguage::English
                    .catalog()
                    .get(source)
                    .and_then(|s| self.render_template(s))
            })
            .or_else(|| self.render_template(source))
            .unwrap_or_else(|| source.into())
    }
    fn render_template(&self, template: &str) -> Option<String> {
        let mut output = String::new();
        let mut chars = template.chars().peekable();
        let mut position = 0;
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    output.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    output.push('}');
                }
                '}' => return None,
                '{' => {
                    let mut key = String::new();
                    loop {
                        match chars.next()? {
                            '}' => break,
                            '{' => return None,
                            c => key.push(c),
                        }
                    }
                    let (key, alternate) = if let Some((key, spec)) = key.split_once(':') {
                        if spec != "#" {
                            return None;
                        }
                        (key, true)
                    } else {
                        (key.as_str(), false)
                    };
                    let value = if key.is_empty() {
                        let value = self.positional.get(position)?;
                        position += 1;
                        value
                    } else if let Ok(index) = key.parse::<usize>() {
                        self.positional.get(index)?
                    } else {
                        self.named.get(key)?
                    };
                    output.push_str(if alternate {
                        &value.alternate
                    } else {
                        &value.display
                    });
                }
                c => output.push(c),
            }
        }
        Some(output)
    }
}

// Capture each argument once; translate the template before inserting user data.
macro_rules! tformat {
    (@args $values:ident;) => {};
    (@args $values:ident; $name:ident = $value:expr $(, $($rest:tt)*)?) => {
        $values.insert(stringify!($name), &$value);
        tformat!(@args $values; $($($rest)*)?);
    };
    (@args $values:ident; $value:expr $(, $($rest:tt)*)?) => {
        $values.push(&$value);
        tformat!(@args $values; $($($rest)*)?);
    };
    ($source:literal $(, $($args:tt)*)?) => {{
        let mut values = $crate::localization::FormatArguments::default();
        tformat!(@args values; $($($args)*)?);
        values.render($source)
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn languages_aliases_and_game_mapping() {
        for language in UiLanguage::ALL {
            assert_eq!(UiLanguage::parse(language.game_code()).unwrap(), language);
            set(language.game_code());
            assert_ne!(tr("账户"), "");
            assert_eq!(tr("我的账户.json"), "我的账户.json");
        }
        assert_eq!(
            UiLanguage::parse("zh-Hant").unwrap(),
            UiLanguage::TraditionalChinese
        );
        assert_eq!(UiLanguage::parse("EN-US").unwrap(), UiLanguage::English);
        assert!(UiLanguage::parse("../en_us").is_err());
    }
    fn placeholders(text: &str) -> Vec<String> {
        let mut result: Vec<_> = regex::Regex::new(r"\{([^{}]*)\}")
            .unwrap()
            .captures_iter(text)
            .map(|c| c[1].to_owned())
            .collect();
        result.sort();
        result
    }
    #[test]
    fn every_catalog_preserves_template_arguments() {
        let english = UiLanguage::English.catalog();
        for language in UiLanguage::ALL {
            assert_eq!(
                language.catalog().len(),
                english.len(),
                "incomplete built-in catalog: {language:?}"
            );
            for (key, value) in language.catalog() {
                assert!(english.contains_key(key), "unknown key: {language:?} {key}");
                assert!(!value.trim().is_empty(), "empty value: {language:?} {key}");
                assert_eq!(
                    placeholders(key),
                    placeholders(value),
                    "{language:?}: {key}"
                );
            }
        }
    }
    #[test]
    fn formats_named_positional_alternate_and_user_text_without_replacing_it() {
        set("en_us");
        let id = "账户/{name}.json";
        assert_eq!(
            tformat!("已恢复实例：{id}", id = id),
            "Instance restored: 账户/{name}.json"
        );
        let error = anyhow::anyhow!("inner").context("outer");
        assert_eq!(tformat!("错误 · {e:#}", e = error), "Error: outer: inner");
        let mut args = FormatArguments::default();
        args.push(&12);
        assert_eq!(args.render_template("{{{0}}}"), Some("{12}".into()));
        assert_eq!(args.render_template("{missing}"), None);
        assert_eq!(args.render_template("{0:?}"), None);
        let mut count = 0;
        assert_eq!(
            tformat!("{} 安装完成", {
                count += 1;
                count
            }),
            "1 installed"
        );
        assert_eq!(count, 1);
    }
}
