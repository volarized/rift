pub use rift_analysis::{
    RELATIONSHIP_EDGES_MAX, RelationshipEdge, RelationshipStore, produced_relationship_facets,
};

#[cfg(test)]
mod tests {
    use super::RelationshipStore;

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

        assert!(index.relationships().is_empty());
        assert!(index.relationships().is_complete());
        let relationships: &RelationshipStore = index.relationships();
        assert_eq!(relationships.dropped_edges(), 0);
        Ok(())
    }
}
