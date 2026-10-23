use anyhow::{bail, Context, Result};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const WINDOWS_LIMIT: usize = 32767;

/// Owns the temporary classpath until the launched JVM has exited. Only the
/// generated classpath is written; game arguments and credentials stay in argv.
pub(crate) struct JavaCommandPlan {
    arguments: Vec<String>,
    classpath_file: Option<PathBuf>,
}
impl Drop for JavaCommandPlan {
    fn drop(&mut self) {
        if let Some(path) = &self.classpath_file {
            let _ = fs::remove_file(path);
        }
    }
}
impl JavaCommandPlan {
    pub(crate) fn prepare(
        java: &Path,
        arguments: Vec<String>,
        jvm_count: usize,
        major: u64,
        classpath: &str,
        game_dir: &Path,
    ) -> Result<Self> {
        Self::for_platform(
            std::env::consts::OS,
            java,
            arguments,
            jvm_count,
            major,
            classpath,
            game_dir,
        )
    }
    fn for_platform(
        os: &str,
        java: &Path,
        arguments: Vec<String>,
        jvm_count: usize,
        major: u64,
        classpath: &str,
        game_dir: &Path,
    ) -> Result<Self> {
        let mut plan = Self {
            arguments,
            classpath_file: None,
        };
        if os != "windows" || windows_size(java, &plan.arguments)? <= WINDOWS_LIMIT {
            return Ok(plan);
        }
        if major < 9 {
            plan.shorten_legacy(
                java,
                jvm_count,
                classpath,
                &crate::java_path_service::WindowsShortPaths,
            )?;
            return Ok(plan);
        }
        let jvm = plan
            .arguments
            .get(..jvm_count)
            .context("JVM 参数范围无效")?;
        if jvm
            .iter()
            .any(|arg| matches!(arg.as_str(), "--disable-@files" | "-Xdisable-@files"))
        {
            bail!("Windows 启动命令过长，但自定义 JVM 参数禁用了参数文件，请移除 disable-@files 或缩短游戏目录");
        }
        let mut ranges = vec![];
        let mut index = 0;
        while index < jvm.len() {
            if matches!(jvm[index].as_str(), "-cp" | "-classpath" | "--class-path")
                && jvm.get(index + 1).is_some_and(|arg| arg == classpath)
            {
                ranges.push(index..index + 2);
                index += 2;
            } else if jvm[index].strip_prefix("--class-path=") == Some(classpath) {
                ranges.push(index..index + 1);
                index += 1;
            } else {
                index += 1;
            }
        }
        if ranges.is_empty() {
            bail!("Windows 启动命令过长，未发现可缩短的标准类路径参数。请缩短游戏目录或自定义 JVM 参数");
        }
        let contents = encode_native(&format!("-classpath\n{}\n", quote_argfile(classpath)?))?;
        let mut random = [0_u8; 16];
        getrandom::getrandom(&mut random)
            .map_err(|_| anyhow::anyhow!("无法生成类路径参数文件名"))?;
        let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let name = format!(".tui-classpath-{suffix}.args");
        let path = game_dir.join(&name);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .context("无法创建类路径参数文件")?;
        plan.classpath_file = Some(path);
        // Close the handle before error cleanup, including on Windows.
        let written = file.write_all(&contents).and_then(|()| file.flush());
        drop(file);
        written.context("写入类路径参数文件失败")?;
        for range in ranges.into_iter().rev() {
            plan.arguments.splice(range, [format!("@{name}")]);
        }
        if windows_size(java, &plan.arguments)? > WINDOWS_LIMIT {
            bail!("使用类路径参数文件后 Windows 启动命令仍然过长，请缩短自定义 JVM / 游戏参数或游戏目录");
        }
        Ok(plan)
    }
    pub(crate) fn arguments(&self) -> &[String] {
        &self.arguments
    }
    fn shorten_legacy(
        &mut self,
        java: &Path,
        jvm_count: usize,
        classpath: &str,
        source: &dyn crate::java_path_service::ShortPathSource,
    ) -> Result<()> {
        crate::java_path_service::LegacyClasspath::new(source).rewrite(
            self.arguments
                .get_mut(..jvm_count)
                .context("JVM 参数范围无效")?,
            classpath,
        );
        if windows_size(java, &self.arguments)? > WINDOWS_LIMIT {
            bail!("Windows 启动命令仍超过长度上限，无法通过可用的短路径解决；Java 8 及以下不支持类路径参数文件。请缩短游戏目录路径或自定义参数，仅在游戏兼容时更换 Java 版本");
        }
        Ok(())
    }
}

// A safe upper bound for Rust's normal Windows argv quoting, measured in UTF-16
// code units including the terminating NUL. No shell is involved.
fn windows_size(java: &Path, arguments: &[String]) -> Result<usize> {
    fn quoted(value: &str) -> usize {
        let mut size = 2usize;
        let mut slashes = 0usize;
        for character in value.chars() {
            if character == '\\' {
                slashes += 1;
            } else {
                size = size.saturating_add(slashes);
                if character == '"' {
                    size = size.saturating_add(slashes + 1);
                }
                size = size.saturating_add(character.len_utf16());
                slashes = 0;
            }
        }
        size.saturating_add(slashes.saturating_mul(2))
    }
    let mut size = quoted(java.to_str().context("Java 路径无法表示为 Unicode")?).saturating_add(1);
    for argument in arguments {
        size = size.saturating_add(1).saturating_add(quoted(argument));
    }
    Ok(size)
}
fn quote_argfile(value: &str) -> Result<String> {
    let mut output = String::from("\"");
    for character in value.chars() {
        output.push_str(match character {
            '\0' => bail!("类路径不能包含 NUL"),
            '\\' => "\\\\",
            '"' => "\\\"",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            '\x0c' => "\\f",
            _ => {
                output.push(character);
                continue;
            }
        });
    }
    output.push('"');
    Ok(output)
}
#[cfg(not(windows))]
fn encode_native(value: &str) -> Result<Vec<u8>> {
    Ok(value.as_bytes().to_vec())
}
#[cfg(windows)]
fn encode_native(value: &str) -> Result<Vec<u8>> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetACP() -> u32;
        fn WideCharToMultiByte(
            cp: u32,
            flags: u32,
            input: *const u16,
            length: i32,
            output: *mut u8,
            capacity: i32,
            default: *const u8,
            used: *mut i32,
        ) -> i32;
    }
    let wide: Vec<u16> = value.encode_utf16().collect();
    let length = i32::try_from(wide.len()).context("类路径参数文件过大")?;
    let codepage = unsafe { GetACP() };
    if codepage == 65001 {
        return Ok(value.as_bytes().to_vec());
    }
    let mut used = 0;
    let required = unsafe {
        WideCharToMultiByte(
            codepage,
            0x400,
            wide.as_ptr(),
            length,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            &mut used,
        )
    };
    if required <= 0 || used != 0 {
        bail!("系统编码不能无损表示类路径，请使用当前系统编码支持的游戏目录");
    }
    let mut bytes = vec![0; required as usize];
    let written = unsafe {
        WideCharToMultiByte(
            codepage,
            0x400,
            wide.as_ptr(),
            length,
            bytes.as_mut_ptr(),
            required,
            std::ptr::null(),
            &mut used,
        )
    };
    if written != required || used != 0 {
        bail!("系统编码不能无损表示类路径，请使用当前系统编码支持的游戏目录");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_plan_reduces_command_without_an_argfile_or_game_argument_changes() {
        struct Short(std::cell::Cell<usize>);
        impl crate::java_path_service::ShortPathSource for Short {
            fn short_path(&self, _: &str) -> Option<String> {
                self.0.set(self.0.get() + 1);
                Some("C:\\x.jar".into())
            }
        }
        let cp = std::iter::repeat_n(
            "C:\\long game directory\\long library directory\\library.jar",
            700,
        )
        .collect::<Vec<_>>()
        .join(";");
        let original = arguments(&cp);
        let mut plan = JavaCommandPlan {
            arguments: original.clone(),
            classpath_file: None,
        };
        assert!(windows_size(Path::new("java"), plan.arguments()).unwrap() > WINDOWS_LIMIT);
        let source = Short(std::cell::Cell::new(0));
        plan.shorten_legacy(Path::new("java"), 3, &cp, &source)
            .unwrap();
        assert!(windows_size(Path::new("java"), plan.arguments()).unwrap() <= WINDOWS_LIMIT);
        assert_eq!(&plan.arguments()[3..], &original[3..]);
        assert!(plan.classpath_file.is_none());
        assert_eq!(source.0.get(), 1);
    }
    #[test]
    fn argfile_quotes_paths_and_controls_without_shell_syntax() {
        assert_eq!(
            quote_argfile("C:\\space #dir\\a\"b.jar;中文\n\t").unwrap(),
            "\"C:\\\\space #dir\\\\a\\\"b.jar;中文\\n\\t\""
        );
        assert!(quote_argfile("bad\0path").is_err());
        assert_eq!(
            encode_native("-classpath\n\"ascii\"\n").unwrap(),
            b"-classpath\n\"ascii\"\n"
        );
        assert_eq!(windows_size(Path::new("java"), &["😀".into()]).unwrap(), 12);
    }
    fn arguments(cp: &str) -> Vec<String> {
        vec![
            "-Xmx1G".into(),
            "-cp".into(),
            cp.into(),
            "example.Main".into(),
            "--accessToken".into(),
            "secret-token-for-test".into(),
        ]
    }
    #[test]
    fn long_classpath_is_shortened_and_file_is_removed_on_drop() {
        let temp = crate::test_support::Workspace::new();
        let cp = "C:\\library\\a.jar;".repeat(2300);
        let plan = JavaCommandPlan::for_platform(
            "windows",
            Path::new("java"),
            arguments(&cp),
            3,
            17,
            &cp,
            &temp.path,
        )
        .unwrap();
        let path = plan.classpath_file.clone().unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.starts_with("-classpath\n"));
        assert!(!contents.contains("secret-token"));
        assert_eq!(
            &plan.arguments()[2..],
            ["example.Main", "--accessToken", "secret-token-for-test"]
        );
        assert!(plan.arguments()[1].starts_with('@'));
        drop(plan);
        assert!(!path.exists());
    }
    #[test]
    fn direct_arguments_and_other_platforms_do_not_create_files() {
        let temp = crate::test_support::Workspace::new();
        for (os, cp) in [
            ("windows", "short".into()),
            ("linux", "x".repeat(40000)),
            ("macos", "x".repeat(40000)),
        ] {
            let args = arguments(&cp);
            let plan = JavaCommandPlan::for_platform(
                os,
                Path::new("java"),
                args.clone(),
                3,
                8,
                &cp,
                &temp.path,
            )
            .unwrap();
            assert_eq!(plan.arguments(), args);
            assert!(plan.classpath_file.is_none());
        }
        assert_eq!(fs::read_dir(&temp.path).unwrap().count(), 0);
    }
    #[test]
    fn unsupported_or_still_oversized_commands_leave_no_files() {
        let temp = crate::test_support::Workspace::new();
        let cp = "x".repeat(40000);
        for (major, args, count) in [
            (8, arguments(&cp), 3),
            (
                17,
                vec!["--disable-@files".into(), "-cp".into(), cp.clone()],
                3,
            ),
            (17, vec![cp.clone()], 1),
            (
                17,
                vec!["-cp".into(), cp.clone(), "main".into(), "y".repeat(40000)],
                2,
            ),
        ] {
            assert!(JavaCommandPlan::for_platform(
                "windows",
                Path::new("java"),
                args,
                count,
                major,
                &cp,
                &temp.path
            )
            .is_err());
            assert_eq!(fs::read_dir(&temp.path).unwrap().count(), 0);
        }
    }
    #[test]
    #[ignore = "requires TUI_TEST_JAVA pointing to a Java 11+ executable"]
    fn real_java_preserves_long_classpath_and_literal_game_argument() {
        let java = PathBuf::from(std::env::var_os("TUI_TEST_JAVA").expect("TUI_TEST_JAVA"));
        let temp = crate::test_support::Workspace::new();
        let source = temp.path.join("ClasspathProbe.java");
        fs::write(&source, "class ClasspathProbe { public static void main(String[] args) { System.out.print(System.getProperty(\"java.class.path\")); System.out.print(\"\\nARG=\" + args[0]); } }").unwrap();
        let cp = format!(
            "{}C:\\space #dir\\中文\\library.jar",
            "C:\\library\\a.jar;".repeat(2300)
        );
        let args = vec![
            "-cp".into(),
            cp.clone(),
            "-Dstdout.encoding=UTF-8".into(),
            source.to_str().unwrap().into(),
            "literal # argument".into(),
        ];
        let plan =
            JavaCommandPlan::for_platform("windows", &java, args, 3, 26, &cp, &temp.path).unwrap();
        let output = std::process::Command::new(&java)
            .args(plan.arguments())
            .current_dir(&temp.path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{cp}\nARG=literal # argument")
        );
    }
}
