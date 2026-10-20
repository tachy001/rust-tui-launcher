mod accounts;
mod asset_layout;
mod auth;
mod backend;
mod cli;
mod config;
mod curseforge;
mod diagnostics;
mod download;
mod download_progress;
mod fabric_check;
mod forge;
mod instances;
mod java;
mod launch_options;
mod loader;
mod loader_catalog;
mod logs;
mod metadata;
mod modrinth;
mod mods;
mod pack;
mod pack_state;
mod paths;
mod sources;
mod storage;
mod task;
#[cfg(test)]
mod test_support;
mod ui;
mod version;
mod worlds;

use anyhow::Result;
fn main() -> Result<()> {
    let config = config::Config::load()?;
    cli::run(config, std::env::args().skip(1).collect())
}
