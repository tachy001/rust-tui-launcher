use std::path::{Path, PathBuf};

/// Java 8/17/21 classpath parsing does not consistently accept Win32 verbatim paths.
pub fn for_java(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::{
            ffi::OsString,
            os::windows::ffi::{OsStrExt, OsStringExt},
        };
        let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        let unc = "\\\\?\\UNC\\".encode_utf16().collect::<Vec<_>>();
        let prefix = "\\\\?\\".encode_utf16().collect::<Vec<_>>();
        if wide.starts_with(&unc) {
            let mut normal = "\\\\".encode_utf16().collect::<Vec<_>>();
            normal.extend_from_slice(&wide[unc.len()..]);
            return PathBuf::from(OsString::from_wide(&normal));
        }
        if wide.starts_with(&prefix) {
            return PathBuf::from(OsString::from_wide(&wide[prefix.len()..]));
        }
    }
    path.to_path_buf()
}
#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn removes_verbatim_prefix_without_losing_unicode() {
        assert_eq!(
            for_java(Path::new(r"\\?\D:\游戏\instance")),
            PathBuf::from(r"D:\游戏\instance")
        );
        assert_eq!(
            for_java(Path::new(r"\\?\UNC\server\share\游戏")),
            PathBuf::from(r"\\server\share\游戏")
        );
    }
}
