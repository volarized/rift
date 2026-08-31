//! The module layout contract a language rule serves for cross-unit refinement.

use crate::failure::BindingError;
use crate::limits::BindingLimits;
use crate::unit::UnitBindingFacts;

/// One language's module layout over the project path set.
///
/// A layout refines one unit's module declaration candidates against the project
/// path set the layout was built from. With no layout, the unit keeps the
/// extraction-time file-name-rule candidates. Downstream language crates implement
/// the trait; the caller that assembles many units into one graph stays
/// language-neutral behind it.
pub trait ModuleLayout {
    /// The unit's facts with every module declaration's candidates recomputed.
    ///
    /// # Errors
    ///
    /// Returns [`BindingError`] when the replaced declarations do not validate
    /// against `limits`.
    fn refined_facts(
        &self,
        unit_path: &str,
        facts: &UnitBindingFacts,
        limits: &BindingLimits,
    ) -> Result<UnitBindingFacts, BindingError>;
}

#[cfg(test)]
mod tests {
    use rift_core::{ExactKind, SourceRange};

    use super::ModuleLayout;
    use crate::graph::{DefinitionOrder, Name, ScopeKind, VisibilitySpelling};
    use crate::limits::BindingLimits;
    use crate::unit::{UnitBindingFacts, UnitDefinition, UnitModuleDeclaration};

    /// A layout placing every module body under one `generated/` directory.
    #[derive(Debug)]
    struct GeneratedLayout;

    impl ModuleLayout for GeneratedLayout {
        fn refined_facts(
            &self,
            unit_path: &str,
            facts: &UnitBindingFacts,
            limits: &BindingLimits,
        ) -> Result<UnitBindingFacts, crate::failure::BindingError> {
            let declarations = facts
                .module_declarations()
                .iter()
                .map(|declaration| {
                    UnitModuleDeclaration::new(
                        declaration.definition(),
                        vec![format!("generated/{unit_path}")],
                    )
                })
                .collect();
            facts.with_module_declarations(declarations, limits)
        }
    }

    /// One module definition with one extraction-time candidate.
    fn declared_facts(limits: BindingLimits) -> UnitBindingFacts {
        let mut builder = UnitBindingFacts::builder(limits);
        let range = SourceRange::new(0, 8).expect("fixture range");
        let root = builder
            .scope(ScopeKind::Module, range, None)
            .expect("root scope accepted");
        let name = Name::new("x").expect("fixture name");
        let definition = UnitDefinition::new(
            root,
            name,
            range,
            ExactKind("stub.module".to_owned()),
            DefinitionOrder::Item,
            VisibilitySpelling::Private,
        );
        let definition = builder.definition(definition).expect("definition accepted");
        let declaration = UnitModuleDeclaration::new(definition, vec!["src/x.rs".to_owned()]);
        builder
            .module_declaration(declaration)
            .expect("declaration accepted");
        builder.build()
    }

    #[test]
    fn test_module_layout_impl_outside_rust_rules_replaces_candidates() {
        let limits = BindingLimits::default();
        let facts = declared_facts(limits);
        let layout: Box<dyn ModuleLayout + Send + Sync> = Box::new(GeneratedLayout);
        let refined = layout
            .refined_facts("src/lib.rs", &facts, &limits)
            .expect("facts refine");
        assert_eq!(
            refined.module_declarations()[0].candidates(),
            ["generated/src/lib.rs".to_owned()],
            "a hand-built layout owns the candidate rule"
        );
    }

    #[test]
    fn test_module_layout_refusal_reaches_the_caller_typed() {
        /// A layout emptying every declaration's candidates, which validation refuses.
        #[derive(Debug)]
        struct EmptyingLayout;

        impl ModuleLayout for EmptyingLayout {
            fn refined_facts(
                &self,
                _unit_path: &str,
                facts: &UnitBindingFacts,
                limits: &BindingLimits,
            ) -> Result<UnitBindingFacts, crate::failure::BindingError> {
                let declarations = facts
                    .module_declarations()
                    .iter()
                    .map(|declaration| {
                        UnitModuleDeclaration::new(declaration.definition(), Vec::new())
                    })
                    .collect();
                facts.with_module_declarations(declarations, limits)
            }
        }

        let limits = BindingLimits::default();
        let facts = declared_facts(limits);
        let refused = EmptyingLayout.refined_facts("src/lib.rs", &facts, &limits);
        let error = refused.expect_err("an empty candidate list is refused");
        assert_eq!(
            error.fault().violation(),
            crate::failure::BindingViolation::InvalidPath
        );
    }
}
