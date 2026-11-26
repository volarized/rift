# Example project

A Rust project that reads workspace configuration from `rift.toml`.

- `src/main.rs` starts the application.
- `src/config.rs` loads the workspace configuration.
- `src/error.rs` defines the error type returned by the loader.

Rift reads the project structure, source, and documentation to help
an agent find the code relevant to its task.
