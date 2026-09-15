//! The opt-in bun corpus suite against the compiled CLI.

mod corpus;

#[tokio::test]
#[ignore = "requires the pinned corpus"]
async fn test_bun_workspace() -> Result<(), Box<dyn std::error::Error>> {
    corpus::run("bun", "workspace").await
}

#[tokio::test]
#[ignore = "requires the pinned corpus"]
async fn test_bun_stop() -> Result<(), Box<dyn std::error::Error>> {
    corpus::run("bun", "stop").await
}
