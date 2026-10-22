use crate::{metadata::safe_path, task::TaskControl};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::path::Path;

pub(crate) struct BundledArtifact {
    name: &'static str,
    bytes: &'static [u8],
}
pub(crate) const OPTIFINE_DISCOVERY: BundledArtifact = BundledArtifact {
    name: "app.tuilauncher:optifine-discovery:1.0",
    bytes: include_bytes!("../assets/compat/optifine-discovery-1.0.jar"),
};
impl BundledArtifact {
    pub(crate) fn descriptor(&self) -> Value {
        json!({"name":self.name,"tuiMcBundled":self.name,"downloads":{"artifact":{
            "path":crate::version::maven_path(self.name).expect("constant Maven coordinate"),
            "url":"https://example.invalid/tui-launcher/bundled/optifine-discovery-1.0.jar",
            "sha1":format!("{:x}",Sha1::digest(self.bytes)),"size":self.bytes.len()}}})
    }
    pub(crate) fn prepare(
        &self,
        root: &Path,
        library: &Value,
        control: &TaskControl,
    ) -> Result<()> {
        if library["name"] != self.name && library.get("tuiMcBundled").is_none() {
            return Ok(());
        }
        control.check()?;
        let expected = self.descriptor();
        if library["name"] != self.name
            || library["tuiMcBundled"] != self.name
            || library["downloads"] != expected["downloads"]
        {
            bail!("内置兼容组件记录不匹配，请重新安装该 OptiFine 实例");
        }
        let artifact = &expected["downloads"]["artifact"];
        let path = safe_path(
            &root.join("libraries"),
            artifact["path"].as_str().context("内置组件路径缺失")?,
        )?;
        let _lease = crate::download_lock::DownloadLease::acquire(&path, control, || {})?;
        if !crate::download::valid_file(
            &path,
            artifact["sha1"].as_str(),
            artifact["size"].as_u64(),
        )? {
            control.check()?;
            crate::storage::atomic_write(&path, self.bytes)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_bridge_contains_the_spi_registration_and_java_eight_class() {
        use std::io::Read;
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(OPTIFINE_DISCOVERY.bytes)).unwrap();
        let mut provider = String::new();
        archive
            .by_name(
                "META-INF/services/cpw.mods.modlauncher.serviceapi.ITransformerDiscoveryService",
            )
            .unwrap()
            .read_to_string(&mut provider)
            .unwrap();
        assert_eq!(provider.trim(), "app.tuilauncher.compat.OptiFineDiscovery");
        let mut header = [0; 8];
        archive
            .by_name("app/tuilauncher/compat/OptiFineDiscovery.class")
            .unwrap()
            .read_exact(&mut header)
            .unwrap();
        assert_eq!(&header[..4], &[0xca, 0xfe, 0xba, 0xbe]);
        assert_eq!(u16::from_be_bytes([header[6], header[7]]), 52);
    }
    #[test]
    fn restores_missing_or_corrupted_bundled_files_without_network() {
        let workspace = crate::test_support::Workspace::new();
        let descriptor = OPTIFINE_DISCOVERY.descriptor();
        let control = TaskControl::default();
        OPTIFINE_DISCOVERY
            .prepare(&workspace.path, &descriptor, &control)
            .unwrap();
        let path = workspace.path.join("libraries").join(
            descriptor["downloads"]["artifact"]["path"]
                .as_str()
                .unwrap(),
        );
        assert_eq!(fs::read(&path).unwrap(), OPTIFINE_DISCOVERY.bytes);
        fs::write(&path, b"corrupt").unwrap();
        OPTIFINE_DISCOVERY
            .prepare(&workspace.path, &descriptor, &control)
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), OPTIFINE_DISCOVERY.bytes);
        let mut wrong = descriptor;
        wrong["downloads"]["artifact"]["sha1"] = "0".repeat(40).into();
        assert!(OPTIFINE_DISCOVERY
            .prepare(&workspace.path, &wrong, &control)
            .is_err());
        assert_eq!(fs::read(&path).unwrap(), OPTIFINE_DISCOVERY.bytes);
    }
}
