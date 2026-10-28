use crate::{
    download::valid_file_with_control,
    metadata::{safe_path, valid_id},
    storage::atomic_copy_controlled,
    task::TaskControl,
};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Reconstruct named legacy layouts from the content-addressed object cache.
/// The modern layout remains untouched and needs no per-object scan here.
pub fn reconstruct(
    root: &Path,
    game_dir: &Path,
    index_id: &str,
    index: &Value,
    control: &TaskControl,
) -> Result<PathBuf> {
    control.check()?;
    valid_id(index_id)?;
    let assets = root.join("assets");
    let virtual_layout = index["virtual"].as_bool() == Some(true);
    let resources_layout = index["map_to_resources"].as_bool() == Some(true);
    let game_assets = if virtual_layout {
        assets.join("virtual").join(index_id)
    } else {
        assets.clone()
    };
    if !virtual_layout && !resources_layout {
        return Ok(game_assets);
    }
    let objects = index["objects"].as_object().context("资源索引无 objects")?;
    let mut plan = vec![];
    for (name, object) in objects {
        control.check()?;
        let hash = object["hash"].as_str().context("资源缺少 hash")?;
        if hash.len() != 40 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
            bail!("无效资源 hash");
        }
        let source = assets
            .join("objects")
            .join(hash[..2].to_ascii_lowercase())
            .join(hash.to_ascii_lowercase());
        let size = object["size"].as_u64();
        if virtual_layout {
            plan.push((source.clone(), safe_path(&game_assets, name)?, hash, size));
        }
        if resources_layout {
            plan.push((
                source,
                safe_path(&game_dir.join("resources"), name)?,
                hash,
                size,
            ));
        }
    }
    // Validate the complete plan before writing any mapped paths.
    for (source, dest, hash, size) in plan {
        control.check()?;
        if dest.is_file() && valid_file_with_control(&dest, Some(hash), size, control)? {
            continue;
        }
        if !source.is_file() || !valid_file_with_control(&source, Some(hash), size, control)? {
            bail!("旧版资源缺失或损坏，请先修复实例：{}", source.display());
        }
        atomic_copy_controlled(&dest, &mut fs::File::open(&source)?, control)?;
    }
    if virtual_layout {
        fs::create_dir_all(&game_assets)?;
    }
    Ok(game_assets)
}

#[cfg(test)]
mod tests {
    use super::*;
    const HASH: &str = "a9993e364706816aba3e25717850c26c9cd0d89d";
    fn index() -> Value {
        serde_json::json!({"virtual":true,"map_to_resources":true,
            "objects":{"sounds/test.ogg":{"hash":HASH,"size":3}}})
    }
    #[test]
    fn reconstructs_new_isolation_directory_and_repairs_same_size_corruption() {
        let temp = crate::test_support::Workspace::new();
        let root = &temp.path;
        let source = root.join("assets/objects/a9").join(HASH);
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"abc").unwrap();
        let control = TaskControl::default();
        let first = root.join("versions/first");
        let virtual_dir = reconstruct(root, &first, "legacy", &index(), &control).unwrap();
        assert_eq!(
            fs::read(virtual_dir.join("sounds/test.ogg")).unwrap(),
            b"abc"
        );
        fs::write(virtual_dir.join("sounds/test.ogg"), b"bad").unwrap();
        let second = root.join("versions/second");
        reconstruct(root, &second, "legacy", &index(), &control).unwrap();
        assert_eq!(
            fs::read(second.join("resources/sounds/test.ogg")).unwrap(),
            b"abc"
        );
        assert_eq!(
            fs::read(virtual_dir.join("sounds/test.ogg")).unwrap(),
            b"abc"
        );
        fs::write(&source, b"bad").unwrap();
        // Already verified mapped files remain usable without copying corrupt objects.
        reconstruct(root, &second, "legacy", &index(), &control).unwrap();
        fs::write(virtual_dir.join("sounds/test.ogg"), b"old").unwrap();
        assert!(reconstruct(root, &second, "legacy", &index(), &control).is_err());
        assert_eq!(
            fs::read(virtual_dir.join("sounds/test.ogg")).unwrap(),
            b"old"
        );
    }
    #[test]
    fn modern_layout_uses_assets_root_without_creating_virtual_paths() {
        let temp = crate::test_support::Workspace::new();
        let dir = reconstruct(
            &temp.path,
            &temp.path,
            "modern",
            &serde_json::json!({"objects":{}}),
            &TaskControl::default(),
        )
        .unwrap();
        assert_eq!(dir, temp.path.join("assets"));
        assert!(!temp.path.join("assets/virtual").exists());
    }
    #[test]
    fn malformed_paths_and_cancellation_do_not_write_mapped_files() {
        let temp = crate::test_support::Workspace::new();
        let bad =
            serde_json::json!({"virtual":true,"objects":{"../escape":{"hash":HASH,"size":3}}});
        assert!(reconstruct(
            &temp.path,
            &temp.path,
            "legacy",
            &bad,
            &TaskControl::default()
        )
        .is_err());
        assert!(!temp.path.join("assets").exists());
        let control = TaskControl::default();
        control.cancel();
        assert!(reconstruct(&temp.path, &temp.path, "legacy", &index(), &control).is_err());
        assert!(!temp.path.join("assets").exists());
    }
}
