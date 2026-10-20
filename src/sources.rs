use anyhow::{bail, Result};

pub fn validate(source: &str) -> Result<()> {
    if matches!(source, "official" | "bmclapi") {
        return Ok(());
    }
    let url = reqwest::Url::parse(source)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("下载源应为 official、bmclapi 或不含账号、参数的 HTTPS 镜像根地址");
    }
    Ok(())
}
pub fn urls(original: &str, source: &str) -> Vec<String> {
    if source == "official" {
        return vec![original.into()];
    }
    let base = if source == "bmclapi" {
        "https://bmclapi2.bangbang93.com"
    } else {
        source
    };
    if validate(base).is_err() {
        return vec![original.into()];
    }
    let Ok(url) = reqwest::Url::parse(original) else {
        return vec![original.into()];
    };
    let prefix = match url.host_str() {
        Some("resources.download.minecraft.net") => "assets",
        Some("libraries.minecraft.net") => "maven",
        Some(
            "piston-meta.mojang.com"
            | "piston-data.mojang.com"
            | "launchermeta.mojang.com"
            | "launcher.mojang.com",
        ) => "",
        _ => return vec![original.into()],
    };
    if url.scheme() != "https" {
        return vec![original.into()];
    }
    let suffix = if prefix.is_empty() {
        url.path().trim_start_matches('/').to_owned()
    } else {
        format!("{prefix}/{}", url.path().trim_start_matches('/'))
    };
    vec![
        format!("{}/{suffix}", base.trim_end_matches('/')),
        original.into(),
    ]
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_only_public_download_hosts() {
        assert_eq!(
            urls(
                "https://resources.download.minecraft.net/ab/abcdef",
                "bmclapi"
            )[0],
            "https://bmclapi2.bangbang93.com/assets/ab/abcdef"
        );
        assert_eq!(
            urls(
                "https://libraries.minecraft.net/a/b.jar",
                "https://mirror.example/root"
            )[0],
            "https://mirror.example/root/maven/a/b.jar"
        );
        assert_eq!(
            urls(
                "https://api.minecraftservices.com/minecraft/profile",
                "bmclapi"
            ),
            vec!["https://api.minecraftservices.com/minecraft/profile"]
        );
        assert!(validate("https://user:password@example.com").is_err());
    }
}
