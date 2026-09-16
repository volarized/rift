//! The opt-in nextjs corpus suite against the compiled CLI.

mod corpus;

#[tokio::test]
#[ignore = "requires the pinned corpus"]
async fn test_nextjs_corpus() -> Result<(), Box<dyn std::error::Error>> {
    corpus::run("nextjs", "workspace").await
}

#[tokio::test]
#[ignore = "requires the pinned corpus"]
async fn test_nextjs_churn() -> Result<(), Box<dyn std::error::Error>> {
    corpus::run("nextjs", "churn").await
}
