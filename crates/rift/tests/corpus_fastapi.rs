//! The opt-in fastapi corpus suite against the compiled CLI.

mod corpus;

#[tokio::test]
#[ignore = "requires the pinned corpus"]
async fn test_fastapi_corpus() -> Result<(), Box<dyn std::error::Error>> {
    corpus::run("fastapi", "workspace").await
}
