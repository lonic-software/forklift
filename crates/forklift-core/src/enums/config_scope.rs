use std::fmt::Display;

/// The configuration file a configuration operation targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigScope {
    /// The per-user configuration file in the user's home directory (`.forkliftconfig`).
    /// Values set here apply to every warehouse of the user.
    Global,

    /// The configuration of the current warehouse (`.forklift/config/warehouse.toml`).
    /// Values set here apply to this warehouse only and override the global configuration.
    Warehouse,
}

impl Display for ConfigScope {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let scope_str = match self {
            ConfigScope::Global    => "global",
            ConfigScope::Warehouse => "warehouse",
        };

        write!(f, "{}", scope_str)
    }
}
