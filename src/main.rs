mod account_service;
mod accounts;
mod argument_service;
mod asset_layout;
mod auth;
mod backend;
mod classpath_service;
mod cli;
mod config;
mod curseforge;
mod diagnostics;
mod download;
mod download_lock;
mod download_progress;
mod fabric_check;
mod forge;
mod instance_config_service;
mod instance_service;
mod instances;
mod java;
mod java_version;
mod launch_options;
mod library_service;
mod loader;
mod loader_catalog;
mod logs;
mod metadata;
mod mod_dependency_service;
mod mod_install_service;
mod mod_plan_service;
mod modrinth;
mod mods;
mod native_service;
mod pack;
mod pack_state;
mod paths;
mod platform_rule_service;
mod process_service;
mod runtime_service;
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
