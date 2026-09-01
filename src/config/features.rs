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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedFeatures {
    pub cgroups: CgroupAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CgroupAction {
    Use,
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

        Ok(ResolvedFeatures { cgroups })
    }
}

#[cfg(test)]
mod tests {
    use super::{CgroupMode, FeaturesConfig};
    use crate::runtime::host::HostCapabilities;

    fn capabilities(memory_cgroup: bool) -> HostCapabilities {
        HostCapabilities::for_test(memory_cgroup, true, true, true)
    }

    #[test]
    fn defaults_to_auto_and_uses_available_memory_controller() {
        let config = FeaturesConfig::default();
        assert_eq!(config.cgroups, CgroupMode::Auto);
        assert_eq!(
            config.resolve(&capabilities(true)).unwrap().cgroups,
            super::CgroupAction::Use
        );
        assert_eq!(
            config.resolve(&capabilities(false)).unwrap().cgroups,
            super::CgroupAction::Ignore
        );
    }

    #[test]
    fn required_fails_without_memory_controller() {
        let config = FeaturesConfig {
            cgroups: CgroupMode::Required,
        };
        let error = config.resolve(&capabilities(false)).unwrap_err();
        assert!(error.contains("cgroup_disable=memory"));
    }

    #[test]
    fn disabled_never_requires_memory_controller() {
        let config = FeaturesConfig {
            cgroups: CgroupMode::Disabled,
        };
        assert_eq!(
            config.resolve(&capabilities(false)).unwrap().cgroups,
            super::CgroupAction::Ignore
        );
    }

    #[test]
    fn parses_feature_modes_and_rejects_unknown_values() {
        let config: FeaturesConfig = toml::from_str("cgroups = 'required'").unwrap();
        assert_eq!(config.cgroups, CgroupMode::Required);

        let error = toml::from_str::<FeaturesConfig>("cgroups = 'sometimes'").unwrap_err();
        assert!(error.to_string().contains("unknown variant"));
    }
}
