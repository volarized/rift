//! The opt-in bun corpus suite against the compiled CLI.

mod corpus;

#[tokio::test]
async fn test_bun_corpus() -> Result<(), Box<dyn std::error::Error>> {
    corpus::run("bun").await
}
