pub use rift_analysis::{EDGES_MAX, Edge, EdgeStore};

#[cfg(test)]
mod tests {
    use super::EdgeStore;

    #[test]
    fn a_real_workspace_fixture_yields_no_edge() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        std::fs::create_dir(directory.path().join("src"))?;
        std::fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn alpha() {}\npub fn beta() { alpha(); }\n",
        )?;
        let index = crate::WorkspaceIndex::build(
            directory.path(),
            crate::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )?;

        assert!(index.edges().is_empty());
        assert!(index.edges().is_complete());
        let edges: &EdgeStore = index.edges();
        assert_eq!(edges.dropped_edges(), 0);
        Ok(())
    }
}
