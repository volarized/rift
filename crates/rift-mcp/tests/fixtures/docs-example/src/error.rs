/// An error encountered while reading or parsing configuration.
pub type ConfigError = Box<dyn std::error::Error + Send + Sync>;
