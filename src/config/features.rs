use crate::runtime::host::HostCapabilities;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CgroupMode {
    #[default]
    Auto,
    Required,
    Disabled,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UserNamespaceMode {
    #[default]
    Enabled,
    Disabled,
}

impl UserNamespaceMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }
}

impl CgroupMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Required => "required",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FeaturesConfig {
    #[serde(default)]
    pub cgroups: CgroupMode,
    #[serde(default)]
    pub user_namespace: UserNamespaceMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedFeatures {
    pub cgroups: CgroupAction,
    pub user_namespace: UserNamespaceAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CgroupAction {
    Use,
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserNamespaceAction {
    Use(crate::runtime::host::UserNamespaceMapping),
    Ignore,
}

impl FeaturesConfig {
    pub fn resolve(&self, capabilities: &HostCapabilities) -> Result<ResolvedFeatures, String> {
        let cgroups = match self.cgroups {
            CgroupMode::Auto if capabilities.cgroups.memory_controller => CgroupAction::Use,
            CgroupMode::Auto => CgroupAction::Ignore,
            CgroupMode::Required if capabilities.cgroups.memory_controller => CgroupAction::Use,
            CgroupMode::Required => {
                return Err(concat!(
                    "cgroups are required but the memory controller is unavailable; remove ",
                    "cgroup_disable=memory from the kernel command line and reboot, or set ",
                    "[features].cgroups = \"disabled\""
                )
                .into())
            }
            CgroupMode::Disabled => CgroupAction::Ignore,
        };

        let user_namespace = match self.user_namespace {
            UserNamespaceMode::Disabled => UserNamespaceAction::Ignore,
            UserNamespaceMode::Enabled => {
                #[cfg(target_os = "linux")]
                {
                    if !capabilities.user_namespace_available() {
                        return Err(format!(
                            "user namespace is enabled but unavailable: {}",
                            capabilities
                                .user_namespace_unavailability_reasons()
                                .join(", ")
                        ));
                    }
                    UserNamespaceAction::Use(
                        capabilities
                            .user_namespace
                            .mapping
                            .expect("available user namespace has a mapping"),
                    )
                }
                #[cfg(not(target_os = "linux"))]
                {
                    UserNamespaceAction::Ignore
                }
            }
        };

        Ok(ResolvedFeatures {
            cgroups,
            user_namespace,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{CgroupMode, FeaturesConfig, UserNamespaceAction, UserNamespaceMode};
    use crate::runtime::host::HostCapabilities;

    fn capabilities(memory_cgroup: bool) -> HostCapabilities {
        HostCapabilities::for_test(memory_cgroup, true, true, true)
    }

    #[test]
    fn defaults_to_auto_and_uses_available_memory_controller() {
        let config = FeaturesConfig::default();
        assert_eq!(config.cgroups, CgroupMode::Auto);
        assert_eq!(config.user_namespace, UserNamespaceMode::Enabled);
        assert_eq!(
            config.resolve(&capabilities(true)).unwrap().cgroups,
            super::CgroupAction::Use
        );
        assert_eq!(
            config.resolve(&capabilities(false)).unwrap().cgroups,
            super::CgroupAction::Ignore
        );
        #[cfg(target_os = "linux")]
        assert!(matches!(
            config.resolve(&capabilities(true)).unwrap().user_namespace,
            UserNamespaceAction::Use(_)
        ));
        #[cfg(not(target_os = "linux"))]
        assert!(matches!(
            config.resolve(&capabilities(true)).unwrap().user_namespace,
            UserNamespaceAction::Ignore
        ));
    }

    #[test]
    fn required_fails_without_memory_controller() {
        let config = FeaturesConfig {
            cgroups: CgroupMode::Required,
            ..FeaturesConfig::default()
        };
        let error = config.resolve(&capabilities(false)).unwrap_err();
        assert!(error.contains("cgroup_disable=memory"));
    }

    #[test]
    fn disabled_never_requires_memory_controller() {
        let config = FeaturesConfig {
            cgroups: CgroupMode::Disabled,
            ..FeaturesConfig::default()
        };
        assert_eq!(
            config.resolve(&capabilities(false)).unwrap().cgroups,
            super::CgroupAction::Ignore
        );
    }

    #[test]
    fn parses_feature_modes_and_rejects_unknown_values() {
        let config: FeaturesConfig =
            toml::from_str("cgroups = 'required'\nuser_namespace = 'disabled'").unwrap();
        assert_eq!(config.cgroups, CgroupMode::Required);
        assert_eq!(config.user_namespace, UserNamespaceMode::Disabled);

        let error = toml::from_str::<FeaturesConfig>("cgroups = 'sometimes'").unwrap_err();
        assert!(error.to_string().contains("unknown variant"));
    }
}
