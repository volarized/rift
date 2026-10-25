use rift_core::{
    ContributionKey, ContributionReference, DeclarationBinding, NodeId, ProviderId,
    ProviderRevision, ProviderSymbolId, SourceRange, SourceUnitId,
};
use rift_provider::NormalizedTarget;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub(super) mod optional_context {
    use super::{Deserialize, Deserializer, Serialize, Serializer};
    use crate::query::QueryProviderContext;
    use std::sync::Arc;

    pub(crate) fn serialize<'value, S: Serializer>(
        value: impl Into<Option<&'value Arc<QueryProviderContext>>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.into().map(Arc::as_ref).serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Arc<QueryProviderContext>>, D::Error> {
        Ok(Option::<QueryProviderContext>::deserialize(deserializer)?.map(Arc::new))
    }
}

macro_rules! checked_text {
    ($module:ident, $kind:ty) => {
        pub(super) mod $module {
            use super::{Deserialize, Deserializer, Serializer};
            pub(crate) fn serialize<S: Serializer>(
                value: &$kind,
                serializer: S,
            ) -> Result<S::Ok, S::Error> {
                serializer.collect_str(value)
            }
            pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
                deserializer: D,
            ) -> Result<$kind, D::Error> {
                <$kind>::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}
checked_text!(workspace, rift_core::WorkspaceId);

macro_rules! revision {
    ($module:ident, $kind:ty) => {
        pub(super) mod $module {
            use super::{Deserialize, Deserializer, Serializer};
            pub(crate) fn serialize<S: Serializer>(
                value: &$kind,
                serializer: S,
            ) -> Result<S::Ok, S::Error> {
                serializer.serialize_u64(value.get())
            }
            pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
                deserializer: D,
            ) -> Result<$kind, D::Error> {
                <$kind>::new(u64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}
revision!(index_revision, rift_core::IndexRevision);
revision!(source_revision, rift_core::SourceRevision);
revision!(tree_revision, rift_core::TreeRevision);

struct Key<'key>(&'key ContributionKey);
impl Serialize for Key<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (
            self.0.reference().provider().as_str(),
            self.0.publication().get(),
            self.0.reference().symbol().as_str(),
        )
            .serialize(serializer)
    }
}
fn decode_key<E: serde::de::Error>(value: (String, u64, String)) -> Result<ContributionKey, E> {
    Ok(ContributionKey::new(
        ProviderId::new(value.0).map_err(E::custom)?,
        ProviderRevision::new(value.1).map_err(E::custom)?,
        ProviderSymbolId::new(value.2).map_err(E::custom)?,
    ))
}
pub(super) mod key {
    use super::{
        ContributionKey, Deserialize, Deserializer, Key, Serialize, Serializer, decode_key,
    };
    pub(crate) fn serialize<S: Serializer>(
        value: &ContributionKey,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        Key(value).serialize(serializer)
    }
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<ContributionKey, D::Error> {
        decode_key(<(String, u64, String)>::deserialize(deserializer)?)
    }
}
pub(super) mod keys {
    use super::{ContributionKey, Deserialize, Deserializer, Key, Serializer, decode_key};
    pub(crate) fn serialize<S: Serializer>(
        values: &[ContributionKey],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(values.iter().map(Key))
    }
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<ContributionKey>, D::Error> {
        Vec::<(String, u64, String)>::deserialize(deserializer)?
            .into_iter()
            .map(decode_key)
            .collect()
    }
}
struct Binding<'binding>(&'binding DeclarationBinding);
impl Serialize for Binding<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeTuple;
        let mut tuple = serializer.serialize_tuple(4)?;
        tuple.serialize_element(&Display(self.0.unit()))?;
        tuple.serialize_element(&self.0.range().start())?;
        tuple.serialize_element(&self.0.range().end())?;
        tuple.serialize_element(&self.0.node())?;
        tuple.end()
    }
}
struct Display<'value, T>(&'value T);
impl<T: std::fmt::Display> Serialize for Display<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self.0)
    }
}
fn decode_binding<E: serde::de::Error>(
    value: (String, u64, u64, Option<NodeId>),
) -> Result<DeclarationBinding, E> {
    Ok(DeclarationBinding::new(
        SourceUnitId::parse(&value.0).map_err(E::custom)?,
        SourceRange::new(value.1, value.2).map_err(E::custom)?,
        value.3,
    ))
}
pub(super) mod binding {
    use super::{
        Binding, DeclarationBinding, Deserialize, Deserializer, NodeId, Serialize, Serializer,
        decode_binding,
    };
    pub(crate) fn serialize<S: Serializer>(
        value: &DeclarationBinding,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        Binding(value).serialize(serializer)
    }
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<DeclarationBinding, D::Error> {
        decode_binding(<(String, u64, u64, Option<NodeId>)>::deserialize(
            deserializer,
        )?)
    }
}
pub(super) mod optional_binding {
    use super::{
        Binding, DeclarationBinding, Deserialize, Deserializer, NodeId, Serialize, Serializer,
        decode_binding,
    };
    pub(crate) fn serialize<'value, S: Serializer>(
        value: impl Into<Option<&'value DeclarationBinding>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.into().map(Binding).serialize(serializer)
    }
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<DeclarationBinding>, D::Error> {
        Option::<(String, u64, u64, Option<NodeId>)>::deserialize(deserializer)?
            .map(decode_binding)
            .transpose()
    }
}
pub(super) mod optional_symbol {
    use super::{Deserialize, Deserializer, Serialize, Serializer};
    pub(crate) fn serialize<'value, S: Serializer>(
        value: impl Into<Option<&'value rift_core::SymbolId>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value
            .into()
            .map(rift_core::SymbolId::as_str)
            .serialize(serializer)
    }
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<rift_core::SymbolId>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(rift_core::SymbolId::new)
            .transpose()
            .map_err(serde::de::Error::custom)
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Target {
    Symbol(String),
    Contribution((String, String)),
}
impl Target {
    fn checked<E: serde::de::Error>(self) -> Result<NormalizedTarget, E> {
        match self {
            Self::Symbol(value) => Ok(NormalizedTarget::Symbol(
                rift_core::SymbolId::new(value).map_err(E::custom)?,
            )),
            Self::Contribution((provider, symbol)) => {
                Ok(NormalizedTarget::Contribution(ContributionReference::new(
                    ProviderId::new(provider).map_err(E::custom)?,
                    ProviderSymbolId::new(symbol).map_err(E::custom)?,
                )))
            }
        }
    }
}
struct TargetRef<'target>(&'target NormalizedTarget);
impl Serialize for TargetRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            NormalizedTarget::Symbol(identity) => {
                serializer.serialize_newtype_variant("Target", 0, "symbol", identity.as_str())
            }
            NormalizedTarget::Contribution(reference) => serializer.serialize_newtype_variant(
                "Target",
                1,
                "contribution",
                &(reference.provider().as_str(), reference.symbol().as_str()),
            ),
        }
    }
}
pub(super) mod target {
    use super::{
        Deserialize, Deserializer, NormalizedTarget, Serialize, Serializer, Target, TargetRef,
    };
    pub(crate) fn serialize<S: Serializer>(
        value: &NormalizedTarget,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        TargetRef(value).serialize(serializer)
    }
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<NormalizedTarget, D::Error> {
        Target::deserialize(deserializer)?.checked()
    }
}
pub(super) mod targets {
    use super::{Deserialize, Deserializer, NormalizedTarget, Serializer, Target, TargetRef};
    pub(crate) fn serialize<S: Serializer>(
        value: &[NormalizedTarget],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(value.iter().map(TargetRef))
    }
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<NormalizedTarget>, D::Error> {
        Vec::<Target>::deserialize(deserializer)?
            .into_iter()
            .map(Target::checked)
            .collect()
    }
}
pub(super) mod optional_target {
    use super::{
        Deserialize, Deserializer, NormalizedTarget, Serialize, Serializer, Target, TargetRef,
    };
    pub(crate) fn serialize<'value, S: Serializer>(
        value: impl Into<Option<&'value NormalizedTarget>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.into().map(TargetRef).serialize(serializer)
    }
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<NormalizedTarget>, D::Error> {
        Option::<Target>::deserialize(deserializer)?
            .map(Target::checked)
            .transpose()
    }
}
macro_rules! portable_enum {
    ($module:ident,$kind:ty,{$($variant:ident => $text:literal),+ $(,)?}) => {
        pub(super) mod $module {
            use super::{Deserialize, Deserializer, Serializer};
            pub(crate) fn serialize<S: Serializer>(value: &$kind, serializer: S) -> Result<S::Ok,S::Error> {
                serializer.serialize_str(match value { $(<$kind>::$variant => $text),+ })
            }
            pub(crate) fn deserialize<'de,D:Deserializer<'de>>(deserializer:D)->Result<$kind,D::Error>{
                match String::deserialize(deserializer)?.as_str(){
                    $($text => Ok(<$kind>::$variant),)+
                    other => Err(serde::de::Error::unknown_variant(other,&[$($text),+])),
                }
            }
        }
    };
}
portable_enum!(role,rift_core::ReferenceRole,{Definition=>"definition",Read=>"read",Write=>"write",Import=>"import",Call=>"call",Type=>"type",Unknown=>"unknown"});
portable_enum!(kind,rift_core::RelationshipKind,{Reference=>"reference",Definition=>"definition",Implementation=>"implementation",TypeDefinition=>"type_definition",Alias=>"alias"});
