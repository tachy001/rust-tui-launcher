use crate::config::Config;
use anyhow::Result;
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::SystemTime,
};

pub struct Diagnosis {
    pub sources: Vec<PathBuf>,
    pub findings: Vec<(&'static str, &'static str)>,
}
impl Diagnosis {
    pub fn summary(&self) -> String {
        let mut lines = vec!["启动诊断（根据日志特征提供排查建议）".to_owned()];
        if self.sources.is_empty() {
            lines.push("没有找到可分析的游戏日志或崩溃报告。".into());
        } else if self.findings.is_empty() {
            lines.push("未识别到明确的常见错误，请结合完整日志和崩溃报告排查。".into());
        }
        for (title, advice) in &self.findings {
            lines.push(format!("{title}：{advice}"));
        }
        for path in &self.sources {
            lines.push(format!("日志：{}", path.display()));
        }
        lines.join("\n")
    }
}

fn classify(text: &str) -> Vec<(&'static str, &'static str)> {
    let text = text.to_lowercase();
    let rules = [
        (&["unsupportedclassversionerror", "unsupported class file major version"][..], "Java 版本不兼容", "将实例 Java 设为自动，或选择游戏与加载器要求的 Java；若错误来自 Mod，也检查它支持的游戏和 Java 版本。"),
        (&["could not reserve enough space", "invalid maximum heap size", "native memory allocation (malloc) failed", "there is insufficient memory for the java runtime environment"][..], "Java 无法分配内存", "降低实例内存，关闭其他占用内存的程序；检查 Java 位数和自定义 -Xmx / -Xms 参数。"),
        (&["outofmemoryerror: java heap space", "outofmemoryerror: gc overhead limit exceeded"][..], "Java 堆内存不足", "在机器可用内存允许的范围内提高实例内存；大型整合包也应检查异常占用内存的 Mod。"),
        (&["outofmemoryerror: unable to create native thread"][..], "系统无法创建更多线程", "检查系统可用内存与线程限制，减少后台程序和创建大量线程的 Mod。"),
        (&["incompatible mods found", "modresolutionexception", "missing mandatory dependencies", "modloadingexception", "loadingfailedexception"][..], "加载器报告 Mod 或依赖问题", "打开日志中的依赖说明，检查游戏版本、加载器、前置 Mod 和冲突版本；可逐批禁用最近添加的 Mod。"),
        (&["duplicate mods", "duplicate mod id", "found duplicate mods"][..], "重复 Mod", "检查 mods 目录是否同时放入了同一 Mod 的多个版本，保留需要的版本，其余禁用或移出。"),
        (&["could not find or load main class", "classnotfoundexception", "noclassdeffounderror"][..], "缺少类或启动依赖", "先使用实例修复；若错误类属于 Mod，检查前置依赖与版本兼容性。也检查自定义 classpath 参数。"),
        (&["glfw error 65542", "glfw error 65543", "pixel format not accelerated", "failed to create window", "opengl is not supported"][..], "图形环境初始化失败", "检查显卡驱动、硬件对该游戏版本的支持，以及远程桌面或虚拟机的图形环境。"),
        (&["zipexception", "invalid or corrupt jarfile", "zip end header not found"][..], "JAR / ZIP 文件读取失败", "根据日志中的文件位置修复或重新下载对应库 / Mod；移除最近添加的损坏文件后再试。"),
        (&["unrecognized vm option", "improperly specified vm option", "unrecognized option:"][..], "Java 启动参数不兼容", "检查并暂时清空实例自定义 JVM 参数，确认它们适用于当前 Java 版本。"),
        (&["exception_access_violation", "sigsegv", "a fatal error has been detected by the java runtime environment"][..], "Java 或原生组件崩溃", "查看 hs_err_pid 报告中的 Problematic frame；检查 Java、显卡驱动和原生 Mod。仅凭退出码无法确定原因。"),
    ];
    rules
        .into_iter()
        .filter(|(patterns, _, _)| patterns.iter().any(|pattern| text.contains(pattern)))
        .map(|(_, title, advice)| (title, advice))
        .collect()
}

// Read both ends: JVM headers are near the start, nested causes often near the end.
fn sample(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut head = vec![];
    (&mut file).take(128 * 1024).read_to_end(&mut head)?;
    if len > 128 * 1024 {
        file.seek(SeekFrom::Start(
            len.saturating_sub(128 * 1024).max(128 * 1024),
        ))?;
        head.push(b'\n');
        file.take(128 * 1024).read_to_end(&mut head)?;
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

fn eligible(path: &Path, since: Option<SystemTime>) -> bool {
    fs::symlink_metadata(path).ok().is_some_and(|metadata| {
        metadata.file_type().is_file()
            && since.is_none_or(|start| metadata.modified().is_ok_and(|modified| modified >= start))
    })
}

fn newest(dir: &Path, prefix: &str, suffix: &str, since: Option<SystemTime>) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let path = entry.path();
            if !name.starts_with(prefix) || !name.ends_with(suffix) || !eligible(&path, since) {
                return None;
            }
            Some((entry.metadata().ok()?.modified().ok()?, path))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

pub fn analyze(config: &Config, id: &str, since: Option<SystemTime>) -> Result<Diagnosis> {
    analyze_dir(&config.effective(id).instance_dir(id)?, since)
}

pub fn analyze_dir(dir: &Path, since: Option<SystemTime>) -> Result<Diagnosis> {
    let mut candidates = vec![dir.join("tui-mc-game.log"), dir.join("logs/latest.log")];
    if let Some(path) = newest(&dir.join("crash-reports"), "crash-", ".txt", since) {
        candidates.push(path);
    }
    if let Some(path) = newest(dir, "hs_err_pid", ".log", since) {
        candidates.push(path);
    }
    let mut result = Diagnosis {
        sources: vec![],
        findings: vec![],
    };
    for path in candidates {
        if !eligible(&path, since) {
            continue;
        }
        let text = sample(&path)?;
        for finding in classify(&text) {
            if !result.findings.iter().any(|(title, _)| *title == finding.0) {
                result.findings.push(finding);
            }
        }
        result.sources.push(path);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classifies_specific_errors_without_echoing_secrets_or_guessing_from_exit_codes() {
        let found = classify(
            "java.lang.UnsupportedClassVersionError: mod\nsecret-token\nIncompatible mods found!",
        );
        assert_eq!(found.len(), 2);
        assert!(!format!("{found:?}").contains("secret-token"));
        assert!(classify(
            "Process exited with code -1\nAllocated 4096 MB heap\nOpenGL initialized"
        )
        .is_empty());
        assert_eq!(
            classify("java.lang.OutOfMemoryError: Java heap space")[0].0,
            "Java 堆内存不足"
        );
    }
    #[test]
    fn reads_head_tail_and_ignores_stale_crashes() {
        let temp = crate::test_support::Workspace::new();
        let mut data = b"UnsupportedClassVersionError\n".to_vec();
        data.resize(400 * 1024, b' ');
        data.extend_from_slice(b"\nIncompatible mods found!");
        fs::write(temp.path.join("tui-mc-game.log"), data).unwrap();
        fs::create_dir(temp.path.join("crash-reports")).unwrap();
        fs::write(
            temp.path.join("crash-reports/crash-fixture.txt"),
            "java.util.zip.ZipException: zip END header not found",
        )
        .unwrap();
        let result = analyze_dir(&temp.path, None).unwrap();
        assert_eq!(result.findings.len(), 3);
        assert_eq!(result.sources.len(), 2);
        let future = SystemTime::now() + std::time::Duration::from_secs(5);
        assert!(analyze_dir(&temp.path, Some(future))
            .unwrap()
            .sources
            .is_empty());
    }
}
