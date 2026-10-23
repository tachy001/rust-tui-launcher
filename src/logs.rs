use anyhow::Result;
use std::{
    collections::VecDeque,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

const READ_BUDGET: usize = 256 * 1024;
const MAX_PENDING: usize = 64 * 1024;
const MAX_LINES: usize = 300;
const MAX_LAG: u64 = 4 * READ_BUDGET as u64;
#[derive(Default)]
struct LogBatch {
    lines: VecDeque<String>,
    dropped: usize,
}
impl LogBatch {
    fn push(&mut self, line: String) {
        if self.lines.len() == MAX_LINES {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back(line);
    }
    fn finish(self) -> Vec<String> {
        let mut lines: Vec<_> = self.lines.into();
        if self.dropped > 0 {
            lines.insert(
                0,
                format!(
                    "[日志输出过多，本轮省略 {} 行；完整内容保存在日志文件]",
                    self.dropped
                ),
            );
        }
        lines
    }
}

pub struct LogTail {
    file: File,
    pending: Vec<u8>,
    position: u64,
    discarding_line: bool,
}
impl LogTail {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            file: File::open(path)?,
            pending: vec![],
            position: 0,
            discarding_line: false,
        })
    }
    pub fn read_lines(&mut self) -> Result<Vec<String>> {
        let mut lines = LogBatch::default();
        let length = self.file.metadata()?.len();
        if length < self.position {
            self.file.seek(SeekFrom::Start(0))?;
            self.position = 0;
            self.pending.clear();
            self.discarding_line = false;
            lines.push("[日志文件已截短，重新读取]".into());
        } else if length.saturating_sub(self.position) > MAX_LAG {
            self.position = length - READ_BUDGET as u64;
            self.file.seek(SeekFrom::Start(self.position))?;
            self.pending.clear();
            self.discarding_line = true;
            lines.push("[日志产生过快，已跳过部分未读内容；完整内容保存在日志文件]".into());
        }
        let before = self.pending.len();
        let read = (&mut self.file)
            .take(READ_BUDGET as u64)
            .read_to_end(&mut self.pending);
        self.position += (self.pending.len() - before) as u64;
        read?;
        if self.discarding_line {
            if let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
                self.pending.drain(..=newline);
                self.discarding_line = false;
            } else {
                self.pending.clear();
                return Ok(lines.finish());
            }
        }
        let mut start = 0;
        for (i, byte) in self.pending.iter().enumerate() {
            if *byte == b'\n' {
                if let Some(line) = human_line(&String::from_utf8_lossy(&self.pending[start..i])) {
                    lines.push(line);
                }
                start = i + 1;
            }
        }
        self.pending.drain(..start);
        if self.pending.len() > MAX_PENDING {
            if let Some(line) = human_line(&String::from_utf8_lossy(&self.pending)) {
                lines.push(line);
            }
            lines.push("[单行日志过长，已截断显示；后续内容保留在日志文件]".into());
            self.pending.clear();
            self.discarding_line = true;
        }
        Ok(lines.finish())
    }
    /// Flush an unterminated final line only after the writer has exited.
    pub fn finish(&mut self) -> Result<Vec<String>> {
        let length = self.file.metadata()?.len();
        let skipped = length.saturating_sub(self.position) > READ_BUDGET as u64;
        if skipped {
            self.position = length - READ_BUDGET as u64;
            self.file.seek(SeekFrom::Start(self.position))?;
            self.pending.clear();
            self.discarding_line = true;
        }
        let mut lines = self.read_lines()?;
        if !self.discarding_line {
            if let Some(line) = human_line(&String::from_utf8_lossy(&self.pending)) {
                lines.push(line);
            }
        }
        self.pending.clear();
        if skipped {
            lines.insert(
                0,
                "[进程已退出，未读日志过多，仅显示末尾片段；完整内容保存在日志文件]".into(),
            );
        }
        Ok(lines)
    }
}
pub fn recent(path: &Path) -> Result<Vec<String>> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(READ_BUDGET as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![];
    file.take(length - start).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text
        .lines()
        .skip(usize::from(start > 0))
        .filter_map(human_line)
        .collect::<Vec<_>>();
    if lines.len() > MAX_LINES {
        lines.drain(..lines.len() - MAX_LINES);
    }
    Ok(lines)
}
fn human_line(raw: &str) -> Option<String> {
    let line = raw.trim_end_matches('\r');
    let body = if let Some((_, body)) = line.split_once("<![CDATA[") {
        body
    } else {
        line
    };
    let body = body
        .split_once("]]>")
        .map(|(content, _)| content)
        .unwrap_or(body);
    let trimmed = body.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("<log4j:")
        || trimmed.starts_with("</log4j:")
        || trimmed.starts_with("<?xml")
    {
        return None;
    }
    Some(body.chars().take(2000).collect())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write};

    #[test]
    fn flood_output_is_sampled_without_changing_the_original_file() {
        let temp = crate::test_support::Workspace::new();
        let path = temp.path.join("flood.log");
        let text: String = (0..500).map(|i| format!("line {i}\n")).collect();
        fs::write(&path, &text).unwrap();
        let mut tail = LogTail::open(&path).unwrap();
        let lines = tail.read_lines().unwrap();
        assert_eq!(lines.len(), MAX_LINES + 1);
        assert!(lines[0].contains("省略 200 行"));
        assert_eq!(lines.last().unwrap(), "line 499");
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        assert!(tail.read_lines().unwrap().is_empty());
    }

    #[test]
    fn huge_unterminated_line_has_bounded_reads_and_is_not_replayed_as_fragments() {
        let temp = crate::test_support::Workspace::new();
        let path = temp.path.join("huge.log");
        let mut bytes = vec![b'x'; READ_BUDGET * 2];
        bytes.extend_from_slice(b"\nafter\n");
        fs::write(&path, bytes).unwrap();
        let mut tail = LogTail::open(&path).unwrap();
        assert_eq!(tail.read_lines().unwrap().len(), 2);
        assert_eq!(tail.position, READ_BUDGET as u64);
        assert!(tail.pending.len() <= MAX_PENDING);
        assert!(tail.read_lines().unwrap().is_empty());
        assert_eq!(tail.read_lines().unwrap(), ["after"]);
        assert!(tail.finish().unwrap().is_empty());
    }

    #[test]
    fn truncate_clears_pending_and_resumes_from_new_start() {
        let temp = crate::test_support::Workspace::new();
        let path = temp.path.join("truncate.log");
        fs::write(&path, "first\nold unfinished text").unwrap();
        let mut tail = LogTail::open(&path).unwrap();
        assert_eq!(tail.read_lines().unwrap(), ["first"]);
        fs::write(&path, "新\n").unwrap();
        let lines = tail.read_lines().unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("已截短"));
        assert_eq!(lines[1], "新");
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"continued\n")
            .unwrap();
        assert_eq!(tail.read_lines().unwrap(), ["continued"]);
    }

    #[test]
    fn exited_process_prioritizes_final_diagnostics_in_a_bounded_window() {
        let temp = crate::test_support::Workspace::new();
        let path = temp.path.join("final.log");
        let mut bytes = vec![b'x'; READ_BUDGET * 2];
        bytes.extend_from_slice("\n最终错误：fixture".as_bytes());
        fs::write(&path, bytes).unwrap();
        let mut tail = LogTail::open(&path).unwrap();
        let lines = tail.finish().unwrap();
        assert!(lines[0].contains("未读日志过多"));
        assert_eq!(lines.last().unwrap(), "最终错误：fixture");
        assert!(tail.finish().unwrap().is_empty());
        assert_eq!(recent(&path).unwrap().last().unwrap(), "最终错误：fixture");
    }

    #[test]
    fn slow_consumer_skips_backlog_and_displays_latest_complete_lines() {
        let temp = crate::test_support::Workspace::new();
        let path = temp.path.join("lag.log");
        let mut bytes = vec![b'x'; MAX_LAG as usize + READ_BUDGET];
        bytes.extend_from_slice(b"\nlatest\n");
        fs::write(&path, bytes).unwrap();
        let mut tail = LogTail::open(&path).unwrap();
        let lines = tail.read_lines().unwrap();
        assert!(lines[0].contains("产生过快"));
        assert_eq!(lines.last().unwrap(), "latest");
        assert!(tail.pending.is_empty());
    }
    #[test]
    fn strips_mojang_console_xml_but_preserves_messages() {
        assert_eq!(
            human_line(" <log4j:Message><![CDATA[Loading 4 mods:").unwrap(),
            "Loading 4 mods:"
        );
        assert_eq!(
            human_line(" - minecraft 1.21.1]]></log4j:Message>").unwrap(),
            " - minecraft 1.21.1"
        );
        assert!(human_line(" </log4j:Event>").is_none());
        assert_eq!(
            human_line("[main/INFO] Hello").unwrap(),
            "[main/INFO] Hello"
        );
    }
    #[test]
    fn preserves_incomplete_utf8_lines() {
        let path = std::env::temp_dir().join(format!("tui-tail-{}.log", std::process::id()));
        fs::write(&path, b"first\n").unwrap();
        let mut tail = LogTail::open(&path).unwrap();
        assert_eq!(tail.read_lines().unwrap(), ["first"]);
        let bytes = "中文\n".as_bytes();
        let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writer.write_all(&bytes[..2]).unwrap();
        assert!(tail.read_lines().unwrap().is_empty());
        writer.write_all(&bytes[2..]).unwrap();
        assert_eq!(tail.read_lines().unwrap(), ["中文"]);
        drop(writer);
        drop(tail);
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn finish_preserves_final_line_without_newline_only_once() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("final.log");
        fs::write(&path, "ready\n最后一行").unwrap();
        let mut tail = LogTail::open(&path).unwrap();
        assert_eq!(tail.read_lines().unwrap(), ["ready"]);
        assert_eq!(tail.finish().unwrap(), ["最后一行"]);
        assert!(tail.finish().unwrap().is_empty());
    }
}
