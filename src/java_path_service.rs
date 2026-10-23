use std::collections::HashMap;

pub(crate) trait ShortPathSource {
    fn short_path(&self, path: &str) -> Option<String>;
}
pub(crate) struct WindowsShortPaths;
impl ShortPathSource for WindowsShortPaths {
    fn short_path(&self, path: &str) -> Option<String> {
        native_short_path(path)
    }
}
pub(crate) struct LegacyClasspath<'a> {
    source: &'a dyn ShortPathSource,
}
impl<'a> LegacyClasspath<'a> {
    pub(crate) fn new(source: &'a dyn ShortPathSource) -> Self {
        Self { source }
    }
    pub(crate) fn rewrite(&self, arguments: &mut [String], classpath: &str) {
        let mut slots = vec![];
        let mut index = 0;
        while index < arguments.len() {
            if matches!(
                arguments[index].as_str(),
                "-cp" | "-classpath" | "--class-path"
            ) && arguments
                .get(index + 1)
                .is_some_and(|value| value == classpath)
            {
                slots.push((index + 1, ""));
                index += 2;
                continue;
            }
            for prefix in ["--class-path=", "-DlegacyClassPath="] {
                if arguments[index].strip_prefix(prefix) == Some(classpath) {
                    slots.push((index, prefix));
                }
            }
            index += 1;
        }
        if slots.is_empty() {
            return;
        }
        let mut cache = HashMap::<&str, String>::new();
        let shorter = classpath
            .split(';')
            .map(|path| {
                cache
                    .entry(path)
                    .or_insert_with(|| {
                        self.source
                            .short_path(path)
                            .filter(|short| {
                                !short.is_empty()
                                    && !short.contains([';', '\0'])
                                    && short.encode_utf16().count() < path.encode_utf16().count()
                            })
                            .unwrap_or_else(|| path.into())
                    })
                    .clone()
            })
            .collect::<Vec<_>>()
            .join(";");
        if shorter.encode_utf16().count() >= classpath.encode_utf16().count() {
            return;
        }
        for (index, prefix) in slots {
            arguments[index] = format!("{prefix}{shorter}");
        }
    }
}
#[cfg(not(windows))]
fn native_short_path(_path: &str) -> Option<String> {
    None
}
#[cfg(windows)]
fn native_short_path(path: &str) -> Option<String> {
    use std::{
        ffi::OsString,
        os::windows::ffi::{OsStrExt, OsStringExt},
        path::PathBuf,
    };
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetShortPathNameW(long: *const u16, short: *mut u16, capacity: u32) -> u32;
    }
    // Canonicalization supplies an absolute verbatim path for long-path APIs;
    // convert the result back to Java-compatible drive / UNC spelling.
    if !std::path::Path::new(path).is_absolute() {
        return None;
    }
    let original = std::fs::canonicalize(path).ok()?;
    let mut input: Vec<u16> = original.as_os_str().encode_wide().collect();
    if input.contains(&0) {
        return None;
    }
    input.push(0);
    let required = unsafe { GetShortPathNameW(input.as_ptr(), std::ptr::null_mut(), 0) };
    if required == 0 || required > 32768 {
        return None;
    }
    let mut buffer = vec![0; required as usize];
    let written = unsafe { GetShortPathNameW(input.as_ptr(), buffer.as_mut_ptr(), required) };
    if written == 0 || written >= required {
        return None;
    }
    let short = PathBuf::from(OsString::from_wide(&buffer[..written as usize]));
    crate::paths::for_java(&short).to_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Source;
    impl ShortPathSource for Source {
        fn short_path(&self, path: &str) -> Option<String> {
            match path {
                "C:\\long library\\one.jar" => Some("C:\\LIB~1\\one.jar".into()),
                "C:\\long library\\two.jar" => Some("C:\\LIB~1\\two.jar".into()),
                _ => None,
            }
        }
    }
    #[test]
    fn rewrites_only_generated_classpath_slots_without_changing_order() {
        let cp = "C:\\long library\\one.jar;unchanged.jar;C:\\long library\\two.jar";
        let short = "C:\\LIB~1\\one.jar;unchanged.jar;C:\\LIB~1\\two.jar";
        let mut args = vec![
            "-cp".into(),
            cp.into(),
            format!("-DlegacyClassPath={cp}"),
            format!("-Dcustom={cp}"),
            "-cp".into(),
            "user-choice.jar".into(),
        ];
        LegacyClasspath::new(&Source).rewrite(&mut args, cp);
        assert_eq!(
            args,
            [
                "-cp",
                short,
                &format!("-DlegacyClassPath={short}"),
                &format!("-Dcustom={cp}"),
                "-cp",
                "user-choice.jar"
            ]
        );
    }
    #[test]
    fn unavailable_longer_or_invalid_alias_preserves_original() {
        struct Invalid;
        impl ShortPathSource for Invalid {
            fn short_path(&self, path: &str) -> Option<String> {
                Some(
                    match path {
                        "one" => "bad;path",
                        "two" => "bad\0",
                        _ => "longer than original",
                    }
                    .into(),
                )
            }
        }
        let mut args = vec!["-cp".into(), "one;two;three".into()];
        LegacyClasspath::new(&Invalid).rewrite(&mut args, "one;two;three");
        assert_eq!(args, ["-cp", "one;two;three"]);
        LegacyClasspath::new(&WindowsShortPaths).rewrite(&mut args, "not-a-generated-classpath");
        assert_eq!(args, ["-cp", "one;two;three"]);
    }
    #[cfg(windows)]
    #[test]
    fn native_alias_reads_the_same_file_when_short_names_are_available() {
        let temp = crate::test_support::Workspace::new();
        let directory = temp.path.join("long spaced library folder");
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("long-library-name.jar");
        std::fs::write(&path, b"fixture").unwrap();
        let alias = WindowsShortPaths
            .short_path(path.to_str().unwrap())
            .expect("GetShortPathName must accept this existing local path");
        // NTFS may have 8.3 naming disabled, in which case the long spelling
        // is valid but cannot help shorten the command.
        assert_eq!(std::fs::read(alias).unwrap(), b"fixture");
    }
}
