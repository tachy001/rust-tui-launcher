use anyhow::Result;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

pub struct LogTail {
    file: File,
    pending: Vec<u8>,
}
impl LogTail {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            file: File::open(path)?,
            pending: vec![],
        })
    }
    pub fn read_lines(&mut self) -> Result<Vec<String>> {
        self.file.read_to_end(&mut self.pending)?;
        let mut lines = vec![];
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
        if self.pending.len() > 65536 {
            lines.push(
                String::from_utf8_lossy(&self.pending)
                    .chars()
                    .take(2000)
                    .collect(),
            );
            self.pending.clear();
        }
        Ok(lines)
    }
}
pub fn recent(path: &Path) -> Result<Vec<String>> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(256 * 1024);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![];
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text
        .lines()
        .skip(usize::from(start > 0))
        .filter_map(human_line)
        .collect::<Vec<_>>();
    if lines.len() > 300 {
        lines.drain(..lines.len() - 300);
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
}
