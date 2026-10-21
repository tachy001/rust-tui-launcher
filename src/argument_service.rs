use anyhow::{bail, Context, Result};

#[derive(Clone, Copy)]
enum Dialect {
    Windows,
    Posix,
}

/// Parses launcher settings into literal argv values; never invokes a shell.
pub(crate) struct ArgumentParser {
    dialect: Dialect,
}
impl ArgumentParser {
    pub(crate) fn current() -> Self {
        Self::for_platform(std::env::consts::OS)
    }
    pub(crate) fn for_platform(os: &str) -> Self {
        Self {
            dialect: if os == "windows" {
                Dialect::Windows
            } else {
                Dialect::Posix
            },
        }
    }
    pub(crate) fn parse(&self, input: &str) -> Result<Vec<String>> {
        if input.contains('\0') {
            bail!("参数不能包含 NUL 字符");
        }
        match self.dialect {
            Dialect::Posix => shlex::split(input).context("参数引号或转义不完整"),
            Dialect::Windows => self.windows(input),
        }
    }
    fn windows(&self, input: &str) -> Result<Vec<String>> {
        let mut chars = input.chars().peekable();
        let mut result = vec![];
        let mut word = String::new();
        let mut quote = None;
        let mut started = false;
        while let Some(character) = chars.next() {
            if quote == Some('\'') {
                if character == '\'' {
                    quote = None;
                } else {
                    word.push(character);
                }
                continue;
            }
            match character {
                character if character.is_whitespace() && quote.is_none() => {
                    if started {
                        result.push(std::mem::take(&mut word));
                        started = false;
                    }
                }
                '\'' if quote.is_none() => {
                    quote = Some('\'');
                    started = true;
                }
                '"' => {
                    started = true;
                    if quote == Some('"') && chars.peek() == Some(&'"') {
                        chars.next();
                        word.push('"');
                    } else {
                        quote = if quote.is_some() { None } else { Some('"') };
                    }
                }
                '\\' => {
                    started = true;
                    let mut count = 1;
                    while chars.peek() == Some(&'\\') {
                        chars.next();
                        count += 1;
                    }
                    let escaped_quote = chars
                        .peek()
                        .copied()
                        .filter(|next| *next == '"' || (*next == '\'' && quote.is_none()));
                    if let Some(next) = escaped_quote {
                        word.extend(std::iter::repeat_n('\\', count / 2));
                        chars.next();
                        if count % 2 == 1 {
                            word.push(next);
                        } else if next == '"' && quote == Some('"') && chars.peek() == Some(&'"') {
                            chars.next();
                            word.push('"');
                        } else {
                            quote = if quote.is_some() { None } else { Some(next) };
                        }
                    } else {
                        word.extend(std::iter::repeat_n('\\', count));
                    }
                }
                character => {
                    word.push(character);
                    started = true;
                }
            }
        }
        if quote.is_some() {
            bail!("参数引号不匹配");
        }
        if started {
            result.push(word);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_preserves_drive_unc_and_unicode_paths() {
        let args = ArgumentParser::for_platform("windows").parse(r#"-Dfolder="C:\Program Files\Java" -Dcache=C:\Users\玩家\.minecraft -Dunc=\\server\share\mods"#).unwrap();
        assert_eq!(
            args,
            [
                r"-Dfolder=C:\Program Files\Java",
                r"-Dcache=C:\Users\玩家\.minecraft",
                r"-Dunc=\\server\share\mods"
            ]
        );
    }
    #[test]
    fn windows_handles_empty_quotes_literal_quotes_and_trailing_backslashes() {
        let parser = ArgumentParser::for_platform("windows");
        assert_eq!(
            parser
                .parse(r#"-Dname='hello world' "" -Dvalue="a\"b" "C:\folder\\""#)
                .unwrap(),
            ["-Dname=hello world", "", "-Dvalue=a\"b", r"C:\folder\"]
        );
        assert_eq!(parser.parse(r#""a""b""#).unwrap(), ["a\"b"]);
        assert!(parser.parse(r#""C:\path\""#).is_err());
    }
    #[test]
    fn posix_quoting_is_preserved_without_shell_expansion() {
        for platform in ["linux", "macos"] {
            assert_eq!(
                ArgumentParser::for_platform(platform)
                    .parse(r#"-Dpath='/tmp/space dir' one\ two "${HOME}" $(command)"#)
                    .unwrap(),
                ["-Dpath=/tmp/space dir", "one two", "${HOME}", "$(command)"]
            );
        }
    }
    #[test]
    fn malformed_settings_fail_before_a_process_can_be_created() {
        for platform in ["windows", "linux", "macos"] {
            let parser = ArgumentParser::for_platform(platform);
            assert!(parser.parse("-Dvalue=\0").is_err());
            assert!(parser.parse("\"unfinished").is_err());
            assert!(parser.parse("'unfinished").is_err());
            assert!(parser.parse("  ").unwrap().is_empty());
        }
    }
}
