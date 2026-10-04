/// Extracts registered evidence from a domain value into an error builder.
pub trait EvidenceFor<Target, Output, Tag = ()> {
    /// Applies the value's evidence to a generated error builder.
    fn apply_evidence(&self, target: Target) -> Output;
}

impl<Value, Target, Output, Tag> EvidenceFor<Target, Output, Tag> for &Value
where
    Value: EvidenceFor<Target, Output, Tag>,
{
    fn apply_evidence(&self, target: Target) -> Output {
        (**self).apply_evidence(target)
    }
}

impl<Value, Target, Tag> EvidenceFor<Target, Target, Tag> for Option<Value>
where
    Value: EvidenceFor<Target, Target, Tag>,
{
    fn apply_evidence(&self, target: Target) -> Target {
        match self {
            Some(value) => value.apply_evidence(target),
            None => target,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Eq, PartialEq)]
    struct Evidence(&'static str);

    #[derive(Debug, Eq, PartialEq)]
    struct Builder(Vec<&'static str>);

    impl EvidenceFor<Builder, Builder> for Evidence {
        fn apply_evidence(&self, mut target: Builder) -> Builder {
            target.0.push(self.0);
            target
        }
    }

    #[test]
    fn references_and_optional_values_share_evidence_mapping() {
        let target = Builder(Vec::new());
        let value = Evidence("present");
        assert_eq!(value.apply_evidence(target), Builder(vec!["present"]));

        let target = Builder(Vec::new());
        let value = Some(Evidence("optional"));
        assert_eq!(value.apply_evidence(target), Builder(vec!["optional"]));

        let target = Builder(Vec::new());
        let value: Option<Evidence> = None;
        assert_eq!(value.apply_evidence(target), Builder(Vec::new()));

        let target = Builder(Vec::new());
        let value = Some(Evidence("borrowed"));
        let value_ref = &value;
        assert_eq!(value_ref.apply_evidence(target), Builder(vec!["borrowed"]));
    }
}
