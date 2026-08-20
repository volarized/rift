use std::any::{TypeId, type_name};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};

use rift_core::constants::{STAGE_NAME_BYTES_MAX, STAGE_NAME_PUNCTUATION};
use rift_core::{
    CompositionId, ErrorDescriptor, ErrorName, ErrorRegistry, ProviderId, is_canonical_ascii_name,
};

// Process-local builder identity token. Mutable allocation remains provider-owned.
static NEXT_BUILDER_TOKEN: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy)]
struct ErasedType {
    id: TypeId,
    name: &'static str,
}

impl ErasedType {
    fn of<T: 'static>() -> Self {
        Self {
            id: TypeId::of::<T>(),
            name: type_name::<T>(),
        }
    }
}

/// Stable path of one stage inside a composition.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StagePath(String);

impl StagePath {
    fn root(composition: &CompositionId, name: &str) -> Result<Self, CompositionError> {
        validate_name(name)?;
        Ok(Self(format!("{composition}.{name}")))
    }

    fn nested(
        composition: &CompositionId,
        scope: &str,
        name: &str,
    ) -> Result<Self, CompositionError> {
        validate_name(scope)?;
        validate_name(name)?;
        Ok(Self(format!("{composition}.{scope}.{name}")))
    }

    /// Returns dot-separated stage path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StagePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One typed component contract.
#[derive(Debug, Clone)]
pub struct Component<Input: 'static, Output: 'static> {
    id: ProviderId,
    // Encodes component input/output contract without storing either value.
    _types: PhantomData<fn(Input) -> Output>,
}

impl<Input: 'static, Output: 'static> Component<Input, Output> {
    /// Constructs component from validated implementation identity.
    #[must_use]
    pub const fn new(id: ProviderId) -> Self {
        Self {
            id,
            _types: PhantomData,
        }
    }

    /// Returns implementation identity.
    #[must_use]
    pub const fn id(&self) -> &ProviderId {
        &self.id
    }
}

/// Typed single-publication output handle.
#[derive(Debug)]
pub struct Flow<T: 'static> {
    owner: u64,
    stage: usize,
    _value: PhantomData<fn() -> T>,
}

impl<T: 'static> Copy for Flow<T> {}

impl<T: 'static> Clone for Flow<T> {
    fn clone(&self) -> Self {
        *self
    }
}

/// Typed keyed-publication output handle.
#[derive(Debug)]
pub struct KeyedFlow<K: 'static, T: 'static> {
    owner: u64,
    stage: usize,
    _value: PhantomData<fn() -> (K, T)>,
}

impl<K: 'static, T: 'static> Copy for KeyedFlow<K, T> {}

impl<K: 'static, T: 'static> Clone for KeyedFlow<K, T> {
    fn clone(&self) -> Self {
        *self
    }
}

/// Runtime cardinality retained after typed graph construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowCardinality {
    /// One immutable publication.
    Single,
    /// Bounded publications keyed by named Rust type.
    Keyed {
        /// Canonical Rust key type name.
        key_type: &'static str,
    },
}

#[derive(Debug, Clone)]
struct StageNode {
    path: StagePath,
    component: ProviderId,
    inputs: Vec<usize>,
    component_input: ErasedType,
    output: ErasedType,
    cardinality: FlowCardinality,
    key_join_policy: Option<KeyJoinPolicy>,
}

struct StageRegistration<'a> {
    path: StagePath,
    component: &'a ProviderId,
    inputs: &'a [usize],
    component_input: ErasedType,
    output: ErasedType,
    cardinality: FlowCardinality,
}

impl<'a> StageRegistration<'a> {
    fn single(
        path: StagePath,
        component: &'a ProviderId,
        inputs: &'a [usize],
        component_input: ErasedType,
        output: ErasedType,
    ) -> Self {
        Self {
            path,
            component,
            inputs,
            component_input,
            output,
            cardinality: FlowCardinality::Single,
        }
    }

    fn by_key<Key: 'static>(
        path: StagePath,
        component: &'a ProviderId,
        inputs: &'a [usize],
        component_input: ErasedType,
        output: ErasedType,
    ) -> Self {
        Self {
            path,
            component,
            inputs,
            component_input,
            output,
            cardinality: FlowCardinality::Keyed {
                key_type: type_name::<Key>(),
            },
        }
    }
}

/// Read-only compiled stage description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageDescriptor {
    path: StagePath,
    component: ProviderId,
    inputs: Box<[StagePath]>,
    component_input_type: &'static str,
    output_type: &'static str,
    cardinality: FlowCardinality,
    key_join_policy: Option<KeyJoinPolicy>,
}

impl StageDescriptor {
    /// Returns stable scoped path.
    #[must_use]
    pub const fn path(&self) -> &StagePath {
        &self.path
    }

    /// Returns implementation identity.
    #[must_use]
    pub const fn component(&self) -> &ProviderId {
        &self.component
    }

    /// Returns ordered upstream stage paths.
    #[must_use]
    pub const fn inputs(&self) -> &[StagePath] {
        &self.inputs
    }

    /// Returns component input type recorded before erasure.
    #[must_use]
    pub const fn component_input_type(&self) -> &'static str {
        self.component_input_type
    }

    /// Returns concrete output type recorded before erasure.
    #[must_use]
    pub const fn output_type(&self) -> &'static str {
        self.output_type
    }

    /// Returns output cardinality.
    #[must_use]
    pub const fn cardinality(&self) -> FlowCardinality {
        self.cardinality
    }

    /// Returns keyed-join policy for join stages.
    #[must_use]
    pub const fn key_join_policy(&self) -> Option<KeyJoinPolicy> {
        self.key_join_policy
    }
}

/// Stable composition-build failure classification and context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompositionErrorKind {
    /// Stage name is empty or not canonical lowercase syntax.
    InvalidName,
    /// Scoped stage path already exists.
    DuplicateStage(StagePath),
    /// Handle belongs to another builder.
    ForeignFlow,
    /// Edited stage output type does not match replacement input.
    TypeMismatch(StagePath),
    /// Requested stage does not exist.
    StageNotFound(StagePath),
    /// Removed stage still has a consumer or is composition output.
    DanglingInput(StagePath),
    /// Composition has no selected output.
    MissingOutput,
}

/// Opaque composition-build failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositionError {
    kind: CompositionErrorKind,
}

impl CompositionError {
    fn new(kind: CompositionErrorKind) -> Self {
        Self { kind }
    }

    /// Returns stable failure classification and context.
    #[must_use]
    pub const fn kind(&self) -> &CompositionErrorKind {
        &self.kind
    }

    /// Returns stage path attached to failure when present.
    #[must_use]
    pub const fn stage(&self) -> Option<&StagePath> {
        match &self.kind {
            CompositionErrorKind::DuplicateStage(path)
            | CompositionErrorKind::TypeMismatch(path)
            | CompositionErrorKind::StageNotFound(path)
            | CompositionErrorKind::DanglingInput(path) => Some(path),
            CompositionErrorKind::InvalidName
            | CompositionErrorKind::ForeignFlow
            | CompositionErrorKind::MissingOutput => None,
        }
    }

    /// Returns canonical registry metadata.
    #[must_use]
    pub const fn descriptor(&self) -> ErrorDescriptor {
        ErrorRegistry::descriptor(ErrorName::InvalidConfiguration)
    }
}

impl fmt::Display for CompositionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.descriptor().explanation())
    }
}

impl std::error::Error for CompositionError {}

/// Typed composition builder. Concrete flow types erase only at [`build`](Self::build).
#[derive(Debug)]
pub struct CompositionBuilder {
    id: CompositionId,
    token: u64,
    stages: Vec<StageNode>,
    paths: BTreeSet<StagePath>,
    output: Option<usize>,
}

/// Borrowed builder for one stable nested stage path.
#[derive(Debug)]
pub struct CompositionScope<'a> {
    builder: &'a mut CompositionBuilder,
    name: String,
}

impl CompositionBuilder {
    /// Starts named composition.
    #[must_use]
    pub fn new(id: CompositionId) -> Self {
        Self {
            id,
            token: NEXT_BUILDER_TOKEN.fetch_add(1, Ordering::Relaxed),
            stages: Vec::new(),
            paths: BTreeSet::new(),
            output: None,
        }
    }

    /// Opens one named nested scope.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionError`] when scope name is not canonical.
    pub fn scope(&mut self, name: &str) -> Result<CompositionScope<'_>, CompositionError> {
        validate_name(name)?;
        Ok(CompositionScope {
            builder: self,
            name: name.into(),
        })
    }

    /// Adds one single source stage.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionError`] for invalid or duplicate name.
    pub fn source<O: 'static>(
        &mut self,
        name: &str,
        component: &Component<(), O>,
    ) -> Result<Flow<O>, CompositionError> {
        let stage = self.add(StageRegistration::single(
            StagePath::root(&self.id, name)?,
            component.id(),
            &[],
            ErasedType::of::<()>(),
            ErasedType::of::<O>(),
        ))?;
        Ok(self.flow(stage))
    }

    /// Adds one keyed source stage.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionError`] for invalid or duplicate name.
    pub fn source_many<K: 'static, O: 'static>(
        &mut self,
        name: &str,
        component: &Component<(), O>,
    ) -> Result<KeyedFlow<K, O>, CompositionError> {
        let stage = self.add(StageRegistration::by_key::<K>(
            StagePath::root(&self.id, name)?,
            component.id(),
            &[],
            ErasedType::of::<()>(),
            ErasedType::of::<O>(),
        ))?;
        Ok(self.keyed_flow(stage))
    }

    /// Connects one typed single input to next stage.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionError`] for foreign handle, invalid name, or duplicate stage.
    pub fn then<I: 'static, O: 'static>(
        &mut self,
        input: Flow<I>,
        name: &str,
        component: &Component<I, O>,
    ) -> Result<Flow<O>, CompositionError> {
        self.validate_owner(input.owner)?;
        let stage = self.add(StageRegistration::single(
            StagePath::root(&self.id, name)?,
            component.id(),
            &[input.stage],
            ErasedType::of::<I>(),
            ErasedType::of::<O>(),
        ))?;
        Ok(self.flow(stage))
    }

    /// Connects two heterogeneous typed inputs to one stage.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionError`] for foreign handle, invalid name, or duplicate stage.
    pub fn combine<Left: 'static, Right: 'static, Output: 'static>(
        &mut self,
        left: Flow<Left>,
        right: Flow<Right>,
        name: &str,
        component: &Component<(Left, Right), Output>,
    ) -> Result<Flow<Output>, CompositionError> {
        self.validate_owner(left.owner)?;
        self.validate_owner(right.owner)?;
        let stage = self.add(StageRegistration::single(
            StagePath::root(&self.id, name)?,
            component.id(),
            &[left.stage, right.stage],
            ErasedType::of::<(Left, Right)>(),
            ErasedType::of::<Output>(),
        ))?;
        Ok(self.flow(stage))
    }

    /// Maps one nested typed stage over every key.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionError`] for foreign handle, invalid name, or duplicate stage.
    pub fn map_each<K: 'static, I: 'static, O: 'static>(
        &mut self,
        input: KeyedFlow<K, I>,
        name: &str,
        component: &Component<I, O>,
    ) -> Result<KeyedFlow<K, O>, CompositionError> {
        self.validate_owner(input.owner)?;
        let stage = self.add(StageRegistration::by_key::<K>(
            StagePath::root(&self.id, name)?,
            component.id(),
            &[input.stage],
            ErasedType::of::<I>(),
            ErasedType::of::<O>(),
        ))?;
        Ok(self.keyed_flow(stage))
    }

    /// Joins two keyed flows under one explicit missing-key policy.
    ///
    /// Policy is execution metadata; key type is enforced by signature.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionError`] for foreign handle, invalid name, or duplicate stage.
    pub fn join_by_key<K: 'static, A: 'static, B: 'static>(
        &mut self,
        left: KeyedFlow<K, A>,
        right: KeyedFlow<K, B>,
        name: &str,
        policy: KeyJoinPolicy,
    ) -> Result<KeyedFlow<K, JoinItem<A, B>>, CompositionError> {
        self.validate_owner(left.owner)?;
        self.validate_owner(right.owner)?;
        let component = ProviderId::new(format!("join.{name}"))
            .map_err(|_| CompositionError::new(CompositionErrorKind::InvalidName))?;
        let stage = self.add(StageRegistration::by_key::<K>(
            StagePath::root(&self.id, name)?,
            &component,
            &[left.stage, right.stage],
            ErasedType::of::<(A, B)>(),
            ErasedType::of::<JoinItem<A, B>>(),
        ))?;
        self.stages[stage].key_join_policy = Some(policy);
        Ok(self.keyed_flow(stage))
    }

    /// Aggregates keyed flow into one typed publication.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionError`] for foreign handle, invalid name, or duplicate stage.
    pub fn aggregate<K: 'static, I: 'static, O: 'static>(
        &mut self,
        input: KeyedFlow<K, I>,
        name: &str,
        component: &Component<Vec<I>, O>,
    ) -> Result<Flow<O>, CompositionError> {
        self.validate_owner(input.owner)?;
        let stage = self.add(StageRegistration::single(
            StagePath::root(&self.id, name)?,
            component.id(),
            &[input.stage],
            ErasedType::of::<Vec<I>>(),
            ErasedType::of::<O>(),
        ))?;
        Ok(self.flow(stage))
    }

    /// Selects composition output.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionError`] when handle belongs to another builder.
    pub fn output<T: 'static>(&mut self, output: Flow<T>) -> Result<(), CompositionError> {
        self.validate_owner(output.owner)?;
        self.output = Some(output.stage);
        Ok(())
    }

    /// Compiles immutable type-erased graph metadata.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionError`] when no output was selected.
    pub fn build(self) -> Result<ProviderComposition, CompositionError> {
        let output = self
            .output
            .ok_or_else(|| CompositionError::new(CompositionErrorKind::MissingOutput))?;
        Ok(ProviderComposition::from_nodes(
            self.id,
            self.stages,
            output,
        ))
    }

    fn add(&mut self, registration: StageRegistration<'_>) -> Result<usize, CompositionError> {
        if !self.paths.insert(registration.path.clone()) {
            return Err(CompositionError::new(CompositionErrorKind::DuplicateStage(
                registration.path,
            )));
        }
        let stage = self.stages.len();
        self.stages.push(StageNode {
            path: registration.path,
            component: registration.component.clone(),
            inputs: registration.inputs.to_vec(),
            component_input: registration.component_input,
            output: registration.output,
            cardinality: registration.cardinality,
            key_join_policy: None,
        });
        Ok(stage)
    }

    fn validate_owner(&self, owner: u64) -> Result<(), CompositionError> {
        if owner != self.token {
            return Err(CompositionError::new(CompositionErrorKind::ForeignFlow));
        }
        Ok(())
    }

    const fn flow<T: 'static>(&self, stage: usize) -> Flow<T> {
        Flow {
            owner: self.token,
            stage,
            _value: PhantomData,
        }
    }

    const fn keyed_flow<K: 'static, T: 'static>(&self, stage: usize) -> KeyedFlow<K, T> {
        KeyedFlow {
            owner: self.token,
            stage,
            _value: PhantomData,
        }
    }
}

impl CompositionScope<'_> {
    /// Connects one typed input inside this scope.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionError`] for foreign handle, invalid name, or duplicate stage.
    pub fn then<I: 'static, O: 'static>(
        &mut self,
        input: Flow<I>,
        name: &str,
        component: &Component<I, O>,
    ) -> Result<Flow<O>, CompositionError> {
        self.builder.validate_owner(input.owner)?;
        let path = StagePath::nested(&self.builder.id, &self.name, name)?;
        let stage = self.builder.add(StageRegistration::single(
            path,
            component.id(),
            &[input.stage],
            ErasedType::of::<I>(),
            ErasedType::of::<O>(),
        ))?;
        Ok(self.builder.flow(stage))
    }

    /// Connects two typed inputs inside this scope.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionError`] for foreign handle, invalid name, or duplicate stage.
    pub fn combine<Left: 'static, Right: 'static, Output: 'static>(
        &mut self,
        left: Flow<Left>,
        right: Flow<Right>,
        name: &str,
        component: &Component<(Left, Right), Output>,
    ) -> Result<Flow<Output>, CompositionError> {
        self.builder.validate_owner(left.owner)?;
        self.builder.validate_owner(right.owner)?;
        let path = StagePath::nested(&self.builder.id, &self.name, name)?;
        let stage = self.builder.add(StageRegistration::single(
            path,
            component.id(),
            &[left.stage, right.stage],
            ErasedType::of::<(Left, Right)>(),
            ErasedType::of::<Output>(),
        ))?;
        Ok(self.builder.flow(stage))
    }
}

/// Immutable compiled provider composition.
#[derive(Debug, Clone)]
pub struct ProviderComposition {
    id: CompositionId,
    stages: Box<[StageDescriptor]>,
    nodes: Box<[StageNode]>,
    output: usize,
}

impl ProviderComposition {
    fn from_nodes(id: CompositionId, nodes: Vec<StageNode>, output: usize) -> Self {
        let stages = nodes
            .iter()
            .map(|node| StageDescriptor {
                path: node.path.clone(),
                component: node.component.clone(),
                inputs: node
                    .inputs
                    .iter()
                    .map(|index| nodes[*index].path.clone())
                    .collect(),
                component_input_type: node.component_input.name,
                output_type: node.output.name,
                cardinality: node.cardinality,
                key_join_policy: node.key_join_policy,
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            id,
            stages,
            nodes: nodes.into_boxed_slice(),
            output,
        }
    }

    /// Returns composition identity.
    #[must_use]
    pub const fn id(&self) -> &CompositionId {
        &self.id
    }

    /// Returns topologically ordered stages.
    #[must_use]
    pub const fn steps(&self) -> &[StageDescriptor] {
        &self.stages
    }

    /// Looks up exact scoped stage path.
    #[must_use]
    pub fn stage(&self, path: &str) -> Option<&StageDescriptor> {
        self.stages.iter().find(|stage| stage.path.as_str() == path)
    }

    /// Starts immutable recipe edit.
    #[must_use]
    pub fn edit(&self) -> CompositionEditor {
        CompositionEditor {
            id: self.id.clone(),
            nodes: self.nodes.to_vec(),
            output: self.output,
        }
    }
}

/// Immutable composition recipe editor.
#[derive(Debug)]
pub struct CompositionEditor {
    id: CompositionId,
    nodes: Vec<StageNode>,
    output: usize,
}

impl CompositionEditor {
    /// Inserts one type-preserving transform after named stage and rewires consumers.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionError`] for missing stage, invalid/duplicate name, or type mismatch.
    pub fn insert_after<T: 'static>(
        &mut self,
        path: &str,
        name: &str,
        component: &Component<T, T>,
    ) -> Result<(), CompositionError> {
        let index = self.index(path)?;
        if self.nodes[index].output.id != TypeId::of::<T>() {
            return Err(CompositionError::new(CompositionErrorKind::TypeMismatch(
                self.nodes[index].path.clone(),
            )));
        }
        let new_path = StagePath::root(&self.id, name)?;
        if self.nodes.iter().any(|node| node.path == new_path) {
            return Err(CompositionError::new(CompositionErrorKind::DuplicateStage(
                new_path,
            )));
        }
        for node in &mut self.nodes {
            for input in &mut node.inputs {
                if *input == index {
                    *input = index + 1;
                } else if *input > index {
                    *input += 1;
                }
            }
        }
        if self.output == index {
            self.output = index + 1;
        } else if self.output > index {
            self.output += 1;
        }
        self.nodes.insert(
            index + 1,
            StageNode {
                path: new_path,
                component: component.id().clone(),
                inputs: vec![index],
                component_input: ErasedType::of::<T>(),
                output: ErasedType::of::<T>(),
                cardinality: FlowCardinality::Single,
                key_join_policy: None,
            },
        );
        Ok(())
    }

    /// Replaces implementation while preserving exact output type.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionError`] for missing stage or incompatible output type.
    pub fn replace<I: 'static, O: 'static>(
        &mut self,
        path: &str,
        component: &Component<I, O>,
    ) -> Result<(), CompositionError> {
        let index = self.index(path)?;
        if self.nodes[index].component_input.id != TypeId::of::<I>()
            || self.nodes[index].output.id != TypeId::of::<O>()
        {
            return Err(CompositionError::new(CompositionErrorKind::TypeMismatch(
                self.nodes[index].path.clone(),
            )));
        }
        self.nodes[index].component = component.id().clone();
        Ok(())
    }

    /// Removes an unused non-output stage.
    ///
    /// # Errors
    ///
    /// Returns [`CompositionError`] when stage is missing or still referenced.
    pub fn remove(&mut self, path: &str) -> Result<(), CompositionError> {
        let index = self.index(path)?;
        if index == self.output || self.nodes.iter().any(|node| node.inputs.contains(&index)) {
            return Err(CompositionError::new(CompositionErrorKind::DanglingInput(
                self.nodes[index].path.clone(),
            )));
        }
        self.nodes.remove(index);
        for node in &mut self.nodes {
            for input in &mut node.inputs {
                if *input > index {
                    *input -= 1;
                }
            }
        }
        if self.output > index {
            self.output -= 1;
        }
        Ok(())
    }

    /// Builds edited immutable composition.
    #[must_use]
    pub fn build(self) -> ProviderComposition {
        ProviderComposition::from_nodes(self.id, self.nodes, self.output)
    }

    fn index(&self, path: &str) -> Result<usize, CompositionError> {
        self.nodes
            .iter()
            .position(|node| node.path.as_str() == path)
            .ok_or_else(|| {
                CompositionError::new(CompositionErrorKind::StageNotFound(StagePath(path.into())))
            })
    }
}

fn validate_name(name: &str) -> Result<(), CompositionError> {
    if !is_canonical_ascii_name(name, STAGE_NAME_BYTES_MAX, STAGE_NAME_PUNCTUATION) {
        return Err(CompositionError::new(CompositionErrorKind::InvalidName));
    }
    Ok(())
}

/// Explicit keyed join policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyJoinPolicy {
    /// Require matching key sets; mismatches remain coverage entries.
    Exact,
    /// Emit only keys present on both sides.
    Inner,
    /// Emit every left key.
    Left,
    /// Emit union of both key sets.
    Union,
}

/// Side absent from one joined key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingSide {
    /// Left input lacks key.
    Left,
    /// Right input lacks key.
    Right,
}

/// One joined key with explicit optional sides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinItem<L, R> {
    /// Left value when present.
    pub left: Option<L>,
    /// Right value when present.
    pub right: Option<R>,
}

/// Join output plus missing-item coverage retained under every policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinCoverage<K, L, R> {
    /// Policy-selected output items.
    pub items: BTreeMap<K, JoinItem<L, R>>,
    /// Every absent side, including keys excluded by inner/left policy.
    pub missing: Vec<(K, MissingSide)>,
}

/// Aligns keyed values without silently dropping missing-item coverage.
#[must_use]
pub fn join_keyed<K, L, R>(
    policy: KeyJoinPolicy,
    left: &BTreeMap<K, L>,
    right: &BTreeMap<K, R>,
) -> JoinCoverage<K, L, R>
where
    K: Clone + Ord,
    L: Clone,
    R: Clone,
{
    let keys = left
        .keys()
        .chain(right.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut items = BTreeMap::new();
    let mut missing = Vec::new();
    for key in keys {
        let left_value = left.get(&key).cloned();
        let right_value = right.get(&key).cloned();
        if left_value.is_none() {
            missing.push((key.clone(), MissingSide::Left));
        }
        if right_value.is_none() {
            missing.push((key.clone(), MissingSide::Right));
        }
        let include = match policy {
            KeyJoinPolicy::Exact | KeyJoinPolicy::Union => true,
            KeyJoinPolicy::Inner => left_value.is_some() && right_value.is_some(),
            KeyJoinPolicy::Left => left_value.is_some(),
        };
        if include {
            items.insert(
                key,
                JoinItem {
                    left: left_value,
                    right: right_value,
                },
            );
        }
    }
    JoinCoverage { items, missing }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CacheEntry<V> {
    revision: u64,
    value: V,
}

/// Per-key cache reuse counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheUpdate {
    /// Entries reused with unchanged revision.
    pub reused: usize,
    /// Entries recomputed after insertion or revision change.
    pub recomputed: usize,
    /// Entries removed because input key disappeared.
    pub removed: usize,
}

/// Per-key cache failure classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheViolation {
    /// Requested key set exceeds configured cache bound.
    TooManyKeys,
}

/// Opaque per-key cache failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheError {
    violation: CacheViolation,
}

impl CacheError {
    /// Returns stable failure classification.
    #[must_use]
    pub const fn violation(self) -> CacheViolation {
        self.violation
    }

    /// Returns canonical registry metadata.
    #[must_use]
    pub const fn descriptor(self) -> ErrorDescriptor {
        ErrorRegistry::descriptor(ErrorName::LimitExceeded)
    }
}

impl fmt::Display for CacheError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.descriptor().explanation())
    }
}

impl std::error::Error for CacheError {}

/// Bounded-composition proof of revision-keyed reuse.
#[derive(Debug, Clone)]
pub struct PerKeyCache<K, V> {
    entries: BTreeMap<K, CacheEntry<V>>,
    max_entries: NonZeroUsize,
}

impl<K: Clone + Ord, V: Clone> PerKeyCache<K, V> {
    /// Constructs empty cache with explicit key bound.
    #[must_use]
    pub const fn new(max_entries: NonZeroUsize) -> Self {
        Self {
            entries: BTreeMap::new(),
            max_entries,
        }
    }

    /// Reconciles exact key/revision set, computing only changed entries.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError`] before mutation when key set exceeds configured bound.
    pub fn update(
        &mut self,
        revisions: &BTreeMap<K, u64>,
        mut compute: impl FnMut(&K) -> V,
    ) -> Result<CacheUpdate, CacheError> {
        if revisions.len() > self.max_entries.get() {
            return Err(CacheError {
                violation: CacheViolation::TooManyKeys,
            });
        }
        let removed = self
            .entries
            .keys()
            .filter(|key| !revisions.contains_key(*key))
            .count();
        self.entries.retain(|key, _| revisions.contains_key(key));
        let mut reused = 0;
        let mut recomputed = 0;
        for (key, revision) in revisions {
            if self
                .entries
                .get(key)
                .is_some_and(|entry| entry.revision == *revision)
            {
                reused += 1;
                continue;
            }
            self.entries.insert(
                key.clone(),
                CacheEntry {
                    revision: *revision,
                    value: compute(key),
                },
            );
            recomputed += 1;
        }
        Ok(CacheUpdate {
            reused,
            recomputed,
            removed,
        })
    }

    /// Returns cached value.
    #[must_use]
    pub fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|entry| &entry.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Sources;
    #[derive(Debug)]
    struct Syntax;
    #[derive(Debug)]
    struct Semantics;
    #[derive(Debug)]
    struct Facts;
    #[derive(Debug)]
    struct Metadata;
    #[derive(Debug)]
    struct Templates;
    #[derive(Debug)]
    struct GeneratedSource;

    fn component<I: 'static, O: 'static>(id: &str) -> Component<I, O> {
        Component::new(ProviderId::new(id).expect("valid component fixture"))
    }

    #[test]
    fn linear_and_multi_input_flows_compile_without_visible_downcast() {
        let mut builder = CompositionBuilder::new(
            CompositionId::new("python-context").expect("valid composition fixture"),
        );
        let source = builder
            .source("project", &component::<(), Sources>("project"))
            .expect("source stage must build");
        let syntax = builder
            .then(source, "syntax", &component::<Sources, Syntax>("syntax"))
            .expect("syntax stage must build");
        let semantics = builder
            .scope("semantics")
            .expect("nested scope")
            .combine(
                source,
                syntax,
                "ty",
                &component::<(Sources, Syntax), Semantics>("semantics"),
            )
            .expect("multi-input stage must build");
        let facts = builder
            .combine(
                syntax,
                semantics,
                "merge",
                &component::<(Syntax, Semantics), Facts>("merge"),
            )
            .expect("merge stage must build");
        builder.output(facts).expect("owned output");
        let composition = builder.build().expect("complete graph");

        assert_eq!(composition.steps().len(), 4);
        assert_eq!(composition.steps()[2].inputs().len(), 2);
        assert!(composition.stage("python-context.semantics.ty").is_some());
        assert!(
            composition
                .steps()
                .iter()
                .all(|stage| !stage.output_type().contains("Any"))
        );
    }

    #[test]
    fn keyed_mapping_join_and_aggregate_retain_cardinality() {
        let mut builder = CompositionBuilder::new(
            CompositionId::new("history").expect("valid composition fixture"),
        );
        let revisions = builder
            .source_many::<u64, Sources>("revisions", &component::<(), Sources>("git"))
            .expect("keyed source");
        let syntax = builder
            .map_each(revisions, "syntax", &component::<Sources, Syntax>("syntax"))
            .expect("keyed map");
        let metadata = builder
            .map_each(
                revisions,
                "metadata",
                &component::<Sources, Metadata>("metadata"),
            )
            .expect("second keyed map");
        let joined = builder
            .join_by_key(syntax, metadata, "join", KeyJoinPolicy::Exact)
            .expect("typed keyed join");
        let aggregate = builder
            .aggregate(
                joined,
                "aggregate",
                &component::<Vec<JoinItem<Syntax, Metadata>>, Facts>("aggregate"),
            )
            .expect("aggregate");
        builder.output(aggregate).expect("owned output");
        let composition = builder.build().expect("complete graph");

        assert!(matches!(
            composition
                .stage("history.join")
                .expect("join stage")
                .cardinality(),
            FlowCardinality::Keyed { .. }
        ));
        assert_eq!(
            composition
                .stage("history.join")
                .expect("join stage")
                .key_join_policy(),
            Some(KeyJoinPolicy::Exact)
        );
        assert_eq!(
            composition
                .stage("history.aggregate")
                .expect("aggregate stage")
                .cardinality(),
            FlowCardinality::Single
        );
    }

    #[test]
    fn join_policies_report_every_missing_side() {
        let left = BTreeMap::from([(1, "left-1"), (2, "left-2")]);
        let right = BTreeMap::from([(2, "right-2"), (3, "right-3")]);
        for (policy, expected_keys) in [
            (KeyJoinPolicy::Exact, vec![1, 2, 3]),
            (KeyJoinPolicy::Inner, vec![2]),
            (KeyJoinPolicy::Left, vec![1, 2]),
            (KeyJoinPolicy::Union, vec![1, 2, 3]),
        ] {
            let joined = join_keyed(policy, &left, &right);
            assert_eq!(
                joined.items.keys().copied().collect::<Vec<_>>(),
                expected_keys
            );
            assert_eq!(
                joined.missing,
                vec![(1, MissingSide::Right), (3, MissingSide::Left)]
            );
        }
    }

    #[test]
    fn per_key_cache_reuses_unchanged_items() {
        let mut cache = PerKeyCache::<u64, String>::new(NonZeroUsize::new(3).expect("non-zero"));
        let first = cache
            .update(
                &BTreeMap::from([(1, 10), (2, 20)]),
                std::string::ToString::to_string,
            )
            .expect("within cache bound");
        assert_eq!(
            first,
            CacheUpdate {
                reused: 0,
                recomputed: 2,
                removed: 0
            }
        );
        let second = cache
            .update(&BTreeMap::from([(1, 10), (2, 21), (3, 30)]), |key| {
                key.to_string()
            })
            .expect("at cache bound");
        assert_eq!(
            second,
            CacheUpdate {
                reused: 1,
                recomputed: 2,
                removed: 0
            }
        );
        let third = cache
            .update(
                &BTreeMap::from([(2, 21), (3, 30)]),
                std::string::ToString::to_string,
            )
            .expect("within cache bound");
        assert_eq!(
            third,
            CacheUpdate {
                reused: 2,
                recomputed: 0,
                removed: 1
            }
        );

        let error = cache
            .update(
                &BTreeMap::from([(1, 1), (2, 2), (3, 3), (4, 4)]),
                std::string::ToString::to_string,
            )
            .expect_err("cache bound must reject before mutation");
        assert_eq!(error.violation(), CacheViolation::TooManyKeys);
        assert_eq!(cache.get(&2).map(String::as_str), Some("2"));
    }

    #[test]
    fn editing_replaces_compatible_stage_and_rejects_dangling_remove() {
        let mut builder = CompositionBuilder::new(
            CompositionId::new("core-syntax").expect("valid composition fixture"),
        );
        let source = builder
            .source("project", &component::<(), Sources>("project"))
            .expect("source");
        let _spare = builder
            .source("spare", &component::<(), Sources>("spare"))
            .expect("unused source");
        let syntax = builder
            .then(source, "syntax", &component::<Sources, Syntax>("syntax"))
            .expect("syntax");
        builder.output(syntax).expect("output");
        let composition = builder.build().expect("complete graph");
        let mut editor = composition.edit();
        editor
            .insert_after(
                "core-syntax.syntax",
                "normalize-docs",
                &component::<Syntax, Syntax>("normalize-docs"),
            )
            .expect("type-preserving transform insertion");
        let mismatch = editor
            .replace(
                "core-syntax.syntax",
                &component::<Metadata, Syntax>("wrong-input"),
            )
            .expect_err("replacement input type must match");
        assert!(matches!(
            mismatch.kind(),
            CompositionErrorKind::TypeMismatch(_)
        ));
        editor
            .replace(
                "core-syntax.syntax",
                &component::<Sources, Syntax>("custom-syntax"),
            )
            .expect("same output type replacement");
        editor
            .remove("core-syntax.spare")
            .expect("unused stage can be removed");
        let error = editor
            .remove("core-syntax.project")
            .expect_err("used source cannot be removed");
        assert!(matches!(
            error.kind(),
            CompositionErrorKind::DanglingInput(_)
        ));
        assert_eq!(error.to_string(), error.descriptor().explanation());
        let edited = editor.build();
        assert_eq!(
            edited
                .stage("core-syntax.syntax")
                .expect("edited stage")
                .component()
                .as_str(),
            "custom-syntax"
        );
        assert_eq!(
            edited
                .stage("core-syntax.normalize-docs")
                .expect("inserted transform")
                .inputs()[0]
                .as_str(),
            "core-syntax.syntax"
        );
    }

    #[test]
    fn routed_external_output_reenters_typed_syntax_flow() {
        let mut builder = CompositionBuilder::new(
            CompositionId::new("tailwind").expect("valid composition fixture"),
        );
        let authored = builder
            .source("css", &component::<(), Sources>("css-source"))
            .expect("authored css source");
        let templates = builder
            .source("templates", &component::<(), Templates>("templates"))
            .expect("template source");
        let generated = builder
            .scope("external")
            .expect("external scope")
            .then(
                templates,
                "tailwind",
                &component::<Templates, GeneratedSource>("tailwind-command"),
            )
            .expect("fake external command stage");
        let generated_syntax = builder
            .then(
                generated,
                "generated-syntax",
                &component::<GeneratedSource, Syntax>("css-syntax"),
            )
            .expect("generated source returns to syntax");
        let authored_syntax = builder
            .then(
                authored,
                "authored-syntax",
                &component::<Sources, Syntax>("css-syntax"),
            )
            .expect("authored syntax");
        let merged = builder
            .combine(
                authored_syntax,
                generated_syntax,
                "merge",
                &component::<(Syntax, Syntax), Facts>("css-merge"),
            )
            .expect("typed merge");
        builder.output(merged).expect("owned output");
        let composition = builder.build().expect("complete route");

        assert_eq!(
            composition
                .stage("tailwind.external.tailwind")
                .expect("external stage")
                .output_type(),
            type_name::<GeneratedSource>()
        );
    }

    #[test]
    fn builder_rejects_foreign_flow_duplicate_stage_and_missing_output() {
        let mut first = CompositionBuilder::new(
            CompositionId::new("first").expect("valid composition fixture"),
        );
        let flow = first
            .source("source", &component::<(), Sources>("source"))
            .expect("source");
        let duplicate = first
            .source("source", &component::<(), Sources>("other"))
            .expect_err("duplicate scoped path must fail");
        assert!(matches!(
            duplicate.kind(),
            CompositionErrorKind::DuplicateStage(_)
        ));

        let mut second = CompositionBuilder::new(
            CompositionId::new("second").expect("valid composition fixture"),
        );
        let foreign = second
            .then(flow, "syntax", &component::<Sources, Syntax>("syntax"))
            .expect_err("foreign handle must fail");
        assert_eq!(foreign.kind(), &CompositionErrorKind::ForeignFlow);
        let missing = second.build().expect_err("output is required");
        assert_eq!(missing.kind(), &CompositionErrorKind::MissingOutput);
    }

    #[test]
    fn stage_names_are_bounded_canonical_ascii() {
        let invalid = ["", "Uppercase", "two.words"];
        for name in invalid {
            let mut builder = CompositionBuilder::new(
                CompositionId::new("names").expect("valid composition fixture"),
            );
            let error = builder
                .source(name, &component::<(), Sources>("source"))
                .expect_err("non-canonical stage name must fail");
            assert_eq!(error.kind(), &CompositionErrorKind::InvalidName);
        }

        let mut builder = CompositionBuilder::new(
            CompositionId::new("names").expect("valid composition fixture"),
        );
        let over_limit = "x".repeat(STAGE_NAME_BYTES_MAX + 1);
        let error = builder
            .source(&over_limit, &component::<(), Sources>("source"))
            .expect_err("oversized stage name must fail");
        assert_eq!(error.kind(), &CompositionErrorKind::InvalidName);
    }
}
