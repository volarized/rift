use example_project::{config, error::ConfigError};

fn main() -> Result<(), ConfigError> {
    let config = config::load_default_config()?;
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(print_root(config));
    Ok(())
}

async fn print_root(config: config::Config) {
    println!("Workspace root: {}", config.root.display());
}
