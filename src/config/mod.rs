mod features;
mod load;
mod model;
mod network;
mod runtime;

pub use features::{CgroupAction, CgroupMode, FeaturesConfig};
pub use load::ConfigError;
pub use model::{ApiConfig, Config, SandboxConfig, ToolConfig};
pub use network::{Ipv4Cidr, NetworkMode, NetworkSettings};
pub use runtime::{RuntimeBackend, SandboxSettings};

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    #[test]
    fn config_file_without_parent_uses_current_directory() {
        let current_dir = fs::canonicalize(".").unwrap();
        assert_eq!(
            super::load::config_directory(Path::new("config.toml")).unwrap(),
            current_dir
        );
    }

    #[test]
    fn load_resolves_mount_paths_relative_to_config_file_without_preparing_sources() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("rootfs")).unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
[api]
listen_addr = "127.0.0.1:0"
token = "secret"

[sandbox]
rootfs_dir = "rootfs"

[mounts]
"/workspace" = "workspace"
"#,
        )
        .unwrap();

        let config = Config::load(&config_path).unwrap();
        let config_dir = fs::canonicalize(temp.path()).unwrap();
        assert_eq!(config.sandbox.rootfs_dir, config_dir.join("rootfs"));
        assert_eq!(
            config.mounts.get(&PathBuf::from("/workspace")),
            Some(&config_dir.join("workspace"))
        );
        assert_eq!(config.sandbox.managed_bundle_dir, config_dir.join("bundle"));
        assert_eq!(config.sandbox.backend, RuntimeBackend::Runsc);
        assert_eq!(config.sandbox.network_mode, NetworkMode::Nat);
        assert_eq!(config.features.cgroups, CgroupMode::Auto);
        assert!(!temp.path().join("workspace").exists());
        assert!(config.tools.is_empty());
    }

    #[test]
    fn load_resolves_and_parses_tools() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("rootfs")).unwrap();
        let tool = temp.path().join("tool");
        fs::write(&tool, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            temp.path().join("tool.env"),
            "# secret\nKEY=VALUE\nQUOTED='hello'\n",
        )
        .unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
[api]
listen_addr = "127.0.0.1:0"
token = "secret"

[sandbox]
rootfs_dir = "rootfs"

[tools.'something-cli']
path = "tool"
env = "tool.env"
"#,
        )
        .unwrap();
        let config = Config::load(&config_path).unwrap();
        let tool = config.tools.get("something-cli").unwrap();
        let config_dir = fs::canonicalize(temp.path()).unwrap();
        assert_eq!(tool.path, config_dir.join("tool"));
        assert_eq!(tool.env, Some(config_dir.join("tool.env")));
    }

    #[test]
    fn load_allows_tools_without_env_file() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("rootfs")).unwrap();
        let tool = temp.path().join("tool");
        fs::write(&tool, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
[api]
listen_addr = "127.0.0.1:0"
token = "token"

[sandbox]
rootfs_dir = "rootfs"

[tools.'something-cli']
path = "tool"
"#,
        )
        .unwrap();
        let config = Config::load(&config_path).unwrap();
        assert_eq!(config.tools["something-cli"].env, None);
    }

    #[test]
    fn old_bundle_configuration_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
[api]
listen_addr = "127.0.0.1:0"
token = "secret"

[sandbox]
bundle_dir = "."
"#,
        )
        .unwrap();
        assert!(matches!(
            Config::load(&config_path),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn old_workspace_configuration_is_rejected() {
        let parsed: Result<SandboxConfig, _> = toml::from_str(
            r#"
rootfs_dir = "."
workspace_dir = "./workspace"
"#,
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn mount_destinations_must_be_safe_absolute_paths() {
        let mut config: Config = toml::from_str(
            r#"
[api]
listen_addr = "127.0.0.1:0"
token = "secret"

[sandbox]
rootfs_dir = "."

[mounts]
"relative" = "/tmp/source"
"#,
        )
        .unwrap();
        let error = config
            .mounts
            .keys()
            .next()
            .map(|path| super::load::validate_mount_destination(path))
            .unwrap();
        assert!(error.is_err());
        config.mounts.clear();
        assert!(super::load::validate_mount_destination(Path::new("/proc")).is_err());
        assert!(super::load::validate_mount_destination(Path::new("/safe/path")).is_ok());
    }
}
