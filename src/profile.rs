//! Creates service policy only. Never opens, copies or parses AGY auth state.
use crate::config::Config;
use serde_json::json;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

pub fn initialize(config: &Config) -> Result<(), String> {
    let directory = config.state_dir.join(".gemini/antigravity-cli");
    no_symlink_ancestors(&directory)?;
    fs::create_dir_all(&directory).map_err(|_| "Could not create service policy directory")?;
    no_symlink_ancestors(&directory)?;
    let path = directory.join("settings.json");
    if let Ok(meta) = fs::symlink_metadata(&path) {
        if !meta.is_file() || meta.nlink() != 1 {
            return Err("Service policy must be a regular private file".into());
        }
    }
    let settings = json!({"enableTerminalSandbox":true,"toolPermission":"proceed-in-sandbox",
        "permissions":{"allow":["mcp(ai-router/*)"],"deny":["unsandboxed(*)","read_url(*)","execute_url(*)",format!("read_file({})",config.state_dir.display()),format!("write_file({})",config.state_dir.display()),format!("read_file({})",config.telemetry_dir.display()),format!("write_file({})",config.telemetry_dir.display())],"ask":[]},
        "hooks":{}});
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|_| "Could not write service policy")?;
    // Only fixed service configuration is written; previous settings are not read.
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| "Could not protect service policy")?;
    file.write_all(settings.to_string().as_bytes())
        .map_err(|_| "Could not write service policy")?;
    file.sync_all()
        .map_err(|_| "Could not persist service policy")?;
    // Global MCP definitions live outside settings.json. Keep this dedicated
    // service profile empty; only the per-run inert relay is configured.
    let mcp_directory = config.state_dir.join(".gemini/config");
    no_symlink_ancestors(&mcp_directory)?;
    fs::create_dir_all(&mcp_directory).map_err(|_| "Could not create MCP policy directory")?;
    no_symlink_ancestors(&mcp_directory)?;
    let mcp_path = mcp_directory.join("mcp_config.json");
    if let Ok(meta) = fs::symlink_metadata(&mcp_path) {
        if !meta.is_file() || meta.nlink() != 1 {
            return Err("Global MCP policy must be a regular private file".into());
        }
    }
    let mut mcp = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(mcp_path)
        .map_err(|_| "Could not write fixed MCP policy")?;
    mcp.write_all(b"{\"mcpServers\":{}}")
        .map_err(|_| "Could not write fixed MCP policy")?;
    mcp.sync_all()
        .map_err(|_| "Could not persist fixed MCP policy")?;
    Ok(())
}

fn no_symlink_ancestors(path: &Path) -> Result<(), String> {
    for ancestor in path.ancestors() {
        if let Ok(meta) = fs::symlink_metadata(ancestor) {
            if meta.file_type().is_symlink() {
                return Err("Service policy directories cannot be symlinks".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_controlled_policy_is_created_without_loading_other_files() {
        let root = tempfile::tempdir().unwrap();
        let config = Config::for_test(root.path());
        initialize(&config).unwrap();
        let path = config
            .state_dir
            .join(".gemini/antigravity-cli/settings.json");
        let policy: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(policy["permissions"]["allow"], json!(["mcp(ai-router/*)"]));
        assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o600);
    }
    #[test]
    fn rejects_policy_symlink_to_a_synthetic_canary_without_reading_it() {
        let root = tempfile::tempdir().unwrap();
        let config = Config::for_test(root.path());
        initialize(&config).unwrap();
        let policy = config
            .state_dir
            .join(".gemini/antigravity-cli/settings.json");
        fs::remove_file(&policy).unwrap();
        let canary = root.path().join("synthetic-canary.txt");
        fs::write(&canary, b"synthetic").unwrap();
        std::os::unix::fs::symlink(&canary, &policy).unwrap();
        assert!(initialize(&config).is_err());
    }
    #[test]
    fn inherited_global_mcp_is_replaced_without_starting_a_canary() {
        let root = tempfile::tempdir().unwrap();
        let config = Config::for_test(root.path());
        initialize(&config).unwrap();
        let path = config.state_dir.join(".gemini/config/mcp_config.json");
        fs::write(
            &path,
            br#"{"mcpServers":{"synthetic":{"command":"never-execute-canary"}}}"#,
        )
        .unwrap();
        initialize(&config).unwrap();
        let fixed: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(fixed, json!({"mcpServers":{}}));
    }
}
