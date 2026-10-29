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
    // Only mirror public, canonical artifact URLs. Query parameters may contain
    // origin-bound signatures; forwarding or dropping them changes the request.
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port().is_some()
    {
        return vec![original.into()];
    }
    let mut path = url.path().trim_start_matches('/');
    let prefix = match url.host_str() {
        Some("resources.download.minecraft.net") => "assets",
        Some("libraries.minecraft.net") => "maven",
        Some("maven.minecraftforge.net" | "maven.fabricmc.net") => "maven",
        Some("meta.fabricmc.net") => "fabric-meta",
        Some("maven.neoforged.net") if path.starts_with("releases/") => {
            path = &path["releases/".len()..];
            "maven"
        }
        Some("files.minecraftforge.net") if path.starts_with("maven/") => {
            path = &path["maven/".len()..];
            "maven"
        }
        Some(
            "piston-meta.mojang.com"
            | "piston-data.mojang.com"
            | "launchermeta.mojang.com"
            | "launcher.mojang.com",
        ) => "",
        _ => return vec![original.into()],
    };
    let suffix = if prefix.is_empty() {
        path.to_owned()
    } else {
        format!("{prefix}/{path}")
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
    fn noncanonical_requests_keep_the_exact_original_url() {
        for original in [
            "https://libraries.minecraft.net/a.jar?version=2",
            "https://libraries.minecraft.net/a.jar?sig=a%2Fb&expires=123",
            "https://user:secret@libraries.minecraft.net/a.jar",
            "https://libraries.minecraft.net:8443/a.jar",
            "https://libraries.minecraft.net/a.jar#part",
            "http://libraries.minecraft.net/a.jar",
        ] {
            assert_eq!(urls(original, "bmclapi"), [original]);
            assert_eq!(urls(original, "https://mirror.example/root"), [original]);
        }
    }
    #[test]
    fn loader_sources_follow_hmcl_public_mirror_paths_and_keep_official_fallback() {
        for (original, path) in [
            (
                "https://maven.minecraftforge.net/net/minecraftforge/a.jar",
                "maven/net/minecraftforge/a.jar",
            ),
            (
                "https://files.minecraftforge.net/maven/net/minecraftforge/a.jar",
                "maven/net/minecraftforge/a.jar",
            ),
            (
                "https://maven.fabricmc.net/net/fabricmc/a.jar",
                "maven/net/fabricmc/a.jar",
            ),
            (
                "https://meta.fabricmc.net/v2/versions/loader/1.21.1",
                "fabric-meta/v2/versions/loader/1.21.1",
            ),
            (
                "https://maven.neoforged.net/releases/net/neoforged/a.jar.sha1",
                "maven/net/neoforged/a.jar.sha1",
            ),
        ] {
            assert_eq!(
                urls(original, "https://mirror.example/root"),
                [
                    format!("https://mirror.example/root/{path}"),
                    original.to_owned()
                ]
            );
            assert_eq!(urls(original, "official"), [original]);
        }
        for original in [
            "https://maven.neoforged.net/api/maven/versions",
            "https://files.minecraftforge.net/promotions_slim.json",
            "https://meta.fabricmc.net.evil.example/v2",
        ] {
            assert_eq!(urls(original, "bmclapi"), [original]);
        }
    }
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
