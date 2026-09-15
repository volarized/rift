//! The opt-in nextjs corpus suite against the compiled CLI.

mod corpus;

#[tokio::test]
async fn test_nextjs_corpus() -> Result<(), Box<dyn std::error::Error>> {
    corpus::run("nextjs").await
}
