use crate::localization::UiLanguage;

pub(crate) trait SystemLocaleSource {
    fn environment(&self, name: &str) -> Option<String>;
    fn preferred_languages(&self) -> Vec<String>;
}
pub(crate) struct NativeLocales;
impl SystemLocaleSource for NativeLocales {
    fn environment(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
    fn preferred_languages(&self) -> Vec<String> {
        native_languages()
    }
}

pub(crate) struct SystemLocaleResolver<'a> {
    source: &'a dyn SystemLocaleSource,
}
impl<'a> SystemLocaleResolver<'a> {
    pub(crate) fn new(source: &'a dyn SystemLocaleSource) -> Self {
        Self { source }
    }
    pub(crate) fn resolve(&self) -> UiLanguage {
        let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
            .into_iter()
            .find_map(|name| {
                self.source
                    .environment(name)
                    .filter(|value| !value.trim().is_empty())
            });
        if locale.as_deref().is_some_and(is_c_locale) {
            return UiLanguage::English;
        }
        let mut candidates = vec![];
        if let Some(languages) = self
            .source
            .environment("LANGUAGE")
            .filter(|value| !value.trim().is_empty())
        {
            candidates.extend(languages.split(':').map(str::to_owned));
        }
        if let Some(locale) = locale {
            candidates.push(locale);
        }
        if candidates.is_empty() {
            candidates = self.source.preferred_languages();
        }
        let language = candidates
            .iter()
            .find_map(|locale| match_locale(locale))
            .unwrap_or(UiLanguage::English);
        // HMCL does not select experimental Arabic UI automatically. Explicit
        // selection continues to work; this avoids silently enabling RTL issues.
        if language == UiLanguage::Arabic {
            UiLanguage::English
        } else {
            language
        }
    }
}

fn is_c_locale(value: &str) -> bool {
    matches!(
        value
            .trim()
            .split(['.', '@'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "c" | "posix"
    )
}
fn match_locale(value: &str) -> Option<UiLanguage> {
    let normalized = value
        .trim()
        .split(['.', '@'])
        .next()?
        .replace('_', "-")
        .to_ascii_lowercase();
    let parts: Vec<_> = normalized.split('-').collect();
    if parts
        .iter()
        .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_alphanumeric()))
    {
        return None;
    }
    Some(match *parts.first()? {
        "zh" => {
            if parts.contains(&"hans") {
                UiLanguage::Chinese
            } else if parts
                .iter()
                .any(|part| matches!(*part, "hant" | "tw" | "hk" | "mo"))
            {
                UiLanguage::TraditionalChinese
            } else {
                UiLanguage::Chinese
            }
        }
        "en" | "c" | "posix" => UiLanguage::English,
        "ja" => UiLanguage::Japanese,
        "de" => UiLanguage::German,
        "es" => UiLanguage::Spanish,
        "ru" => UiLanguage::Russian,
        "uk" => UiLanguage::Ukrainian,
        "ar" => UiLanguage::Arabic,
        "lzh" => UiLanguage::ClassicalChinese,
        _ => return None,
    })
}

#[cfg(windows)]
fn native_languages() -> Vec<String> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetUserPreferredUILanguages(
            flags: u32,
            count: *mut u32,
            buffer: *mut u16,
            length: *mut u32,
        ) -> i32;
    }
    let mut count = 0;
    let mut length = 0;
    // SAFETY: writable scalar outputs, null buffer requests the required size.
    if unsafe { GetUserPreferredUILanguages(8, &mut count, std::ptr::null_mut(), &mut length) } == 0
        || !(2..=32768).contains(&length)
    {
        return vec![];
    }
    let mut buffer = vec![0_u16; length as usize];
    // SAFETY: the initialized buffer holds exactly `length` UTF-16 code units.
    if unsafe { GetUserPreferredUILanguages(8, &mut count, buffer.as_mut_ptr(), &mut length) } == 0
        || length as usize > buffer.len()
    {
        return vec![];
    }
    buffer[..length as usize]
        .split(|unit| *unit == 0)
        .filter(|part| !part.is_empty())
        .filter_map(|part| String::from_utf16(part).ok())
        .take(32)
        .collect()
}

#[cfg(target_os = "macos")]
fn native_languages() -> Vec<String> {
    use std::ffi::{c_char, c_void, CStr};
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFLocaleCopyPreferredLanguages() -> *const c_void;
        fn CFArrayGetCount(array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
        fn CFStringGetCString(
            string: *const c_void,
            buffer: *mut c_char,
            size: isize,
            encoding: u32,
        ) -> u8;
        fn CFRelease(value: *const c_void);
    }
    // SAFETY: Apple's API returns an owned array of CFString language IDs.
    unsafe {
        let array = CFLocaleCopyPreferredLanguages();
        if array.is_null() {
            return vec![];
        }
        let mut languages = vec![];
        for index in 0..CFArrayGetCount(array).clamp(0, 32) {
            let value = CFArrayGetValueAtIndex(array, index);
            let mut buffer = [0 as c_char; 256];
            if !value.is_null()
                && CFStringGetCString(
                    value,
                    buffer.as_mut_ptr(),
                    buffer.len() as isize,
                    0x08000100,
                ) != 0
            {
                if let Ok(language) = CStr::from_ptr(buffer.as_ptr()).to_str() {
                    languages.push(language.into());
                }
            }
        }
        CFRelease(array);
        languages
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn native_languages() -> Vec<String> {
    use std::io::Read;
    for path in ["/etc/locale.conf", "/etc/default/locale"] {
        let Ok(file) = std::fs::File::open(path) else {
            continue;
        };
        let mut text = String::new();
        if file.take(65537).read_to_string(&mut text).is_err() || text.len() > 65536 {
            continue;
        }
        if let Some(locale) = text.lines().rev().find_map(|line| {
            let (name, value) = line.trim().split_once('=')?;
            (name.trim() == "LANG").then(|| value.trim().trim_matches(['\'', '"']).to_owned())
        }) {
            return vec![locale];
        }
    }
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Source {
        env: std::collections::HashMap<String, String>,
        native: Vec<String>,
    }
    impl SystemLocaleSource for Source {
        fn environment(&self, name: &str) -> Option<String> {
            self.env.get(name).cloned()
        }
        fn preferred_languages(&self) -> Vec<String> {
            self.native.clone()
        }
    }
    fn resolve(env: &[(&str, &str)], native: &[&str]) -> UiLanguage {
        SystemLocaleResolver::new(&Source {
            env: env
                .iter()
                .map(|(key, value)| ((*key).into(), (*value).into()))
                .collect(),
            native: native.iter().map(|value| (*value).into()).collect(),
        })
        .resolve()
    }
    #[test]
    fn environment_precedence_c_locale_and_native_fallback_are_deterministic() {
        assert_eq!(
            resolve(
                &[
                    ("LC_ALL", "ja_JP.UTF-8"),
                    ("LC_MESSAGES", "de_DE"),
                    ("LANG", "es_ES")
                ],
                &["zh-CN"]
            ),
            UiLanguage::Japanese
        );
        assert_eq!(
            resolve(&[("LC_ALL", "C.UTF-8"), ("LANGUAGE", "zh_CN")], &["ja-JP"]),
            UiLanguage::English
        );
        assert_eq!(
            resolve(&[("LC_MESSAGES", "de_AT.UTF-8"), ("LANG", "es_ES")], &[]),
            UiLanguage::German
        );
        assert_eq!(resolve(&[], &["fr-FR", "ja-JP"]), UiLanguage::Japanese);
        assert_eq!(
            resolve(&[("LANG", "fr_FR")], &["zh-CN"]),
            UiLanguage::English
        );
        assert_eq!(resolve(&[], &[]), UiLanguage::English);
    }
    #[test]
    fn regional_script_and_preference_list_variants_match_existing_catalogs() {
        for (locale, expected) in [
            ("zh-Hant-CN", UiLanguage::TraditionalChinese),
            ("zh-Hans-HK", UiLanguage::Chinese),
            ("zh_MO.UTF-8", UiLanguage::TraditionalChinese),
            ("zh_SG", UiLanguage::Chinese),
            ("es-MX", UiLanguage::Spanish),
            ("en-AU", UiLanguage::English),
            ("uk_UA.UTF-8", UiLanguage::Ukrainian),
            ("de_DE@euro", UiLanguage::German),
        ] {
            assert_eq!(resolve(&[("LANG", locale)], &[]), expected, "{locale}");
        }
        assert_eq!(
            resolve(&[("LANGUAGE", "fr:de_DE:es"), ("LANG", "fr_FR")], &[]),
            UiLanguage::German
        );
        assert_eq!(resolve(&[], &["ar-SA"]), UiLanguage::English);
        assert_eq!(UiLanguage::parse("ar_sa").unwrap(), UiLanguage::Arabic);
        assert_eq!(
            resolve(&[("LANG", "../../zh_CN")], &[]),
            UiLanguage::English
        );
    }

    #[test]
    fn system_mode_reaches_native_provider_and_game_language_following() {
        let native = NativeLocales.preferred_languages();
        assert!(native
            .iter()
            .all(|value| !value.is_empty() && !value.contains('\0')));
        let source = Source {
            env: Default::default(),
            native,
        };
        assert!(UiLanguage::ALL.contains(&SystemLocaleResolver::new(&source).resolve()));
        let selected = SystemLocaleResolver::new(&NativeLocales).resolve();
        for alias in ["auto", "system", "default", "def", ""] {
            assert_eq!(UiLanguage::parse(alias).unwrap(), selected);
        }
        let game = crate::language_service::GameLanguage::parse("launcher")
            .unwrap()
            .resolve("auto")
            .unwrap();
        assert_eq!(
            game.for_version("1.21.1", &serde_json::json!({}))
                .as_deref(),
            Some(selected.game_code())
        );
    }
}
