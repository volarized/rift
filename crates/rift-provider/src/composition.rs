use std::any::{TypeId, type_name};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};

use rift_core::constants::{STAGE_NAME_BYTES_MAX, STAGE_NAME_PUNCTUATION};
use rift_core::{
    CompositionId, Error, ErrorCode, ErrorContext, ErrorName, Fault, ProviderId, fault_label,
    is_canonical_ascii_name,
};
use serde::Serialize;

// Process-local builder origin. Mutable allocation remains provider-owned.
static NEXT_BUILDER_ORIGIN: AtomicU64 = AtomicU64::new(1);

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
    fn root(composition: &CompositionId, name: &str) -> Self {
        Self(format!("{composition}.{name}"))
    }

    fn nested(composition: &CompositionId, scope: &str, name: &str) -> Self {
        Self(format!("{composition}.{scope}.{name}"))
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
    origin: u64,
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
    origin: u64,
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
pub enum CompositionFault {
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

impl CompositionFault {
    /// Returns stage path attached to failure when present.
    #[must_use]
    pub const fn stage(&self) -> Option<&StagePath> {
        match self {
            Self::DuplicateStage(path)
            | Self::TypeMismatch(path)
            | Self::StageNotFound(path)
            | Self::DanglingInput(path) => Some(path),
            Self::InvalidName | Self::ForeignFlow | Self::MissingOutput => None,
        }
    }
}

impl Fault for CompositionFault {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::ConfigurationInvalid)
    }

    fn context(&self) -> Vec<ErrorContext> {
        let rule = match self {
            Self::InvalidName => "invalid_name",
            Self::DuplicateStage(_) => "duplicate_stage",
            Self::ForeignFlow => "foreign_flow",
            Self::TypeMismatch(_) => "type_mismatch",
            Self::StageNotFound(_) => "stage_not_found",
            Self::DanglingInput(_) => "dangling_input",
            Self::MissingOutput => "missing_output",
        };
        let mut context = vec![ErrorContext::new("rule", rule)];
        if let Some(stage) = self.stage() {
            context.push(ErrorContext::new("stage", stage.to_string()));
        }
        context
    }
}

/// Composition-build failure.
pub type CompositionError = Error<CompositionFault>;

/// Typed composition builder. Concrete flow types erase only at [`build`](Self::build).
///
/// Stage methods chain without intermediate results; every recorded
/// construction failure surfaces once, at [`build`](Self::build).
#[derive(Debug)]
pub struct CompositionBuilder {
    id: CompositionId,
    origin: u64,
    stages: Vec<StageNode>,
    paths: BTreeSet<StagePath>,
    output: Option<usize>,
    errors: Vec<CompositionError>,
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
            origin: NEXT_BUILDER_ORIGIN.fetch_add(1, Ordering::Relaxed),
            stages: Vec::new(),
            paths: BTreeSet::new(),
            output: None,
            errors: Vec::new(),
        }
    }

    /// Opens one named nested scope.
    ///
    /// A non-canonical scope name is reported by [`build`](Self::build).
    #[must_use]
    pub fn scope(&mut self, name: &str) -> CompositionScope<'_> {
        record_failure(&mut self.errors, validate_name(name));
        CompositionScope {
            builder: self,
            name: name.into(),
        }
    }

    /// Adds one single source stage.
    ///
    /// An invalid or duplicate name is reported by [`build`](Self::build).
    #[must_use]
    pub fn source<O: 'static>(&mut self, name: &str, component: &Component<(), O>) -> Flow<O> {
        record_failure(&mut self.errors, validate_name(name));
        let stage = self.add(StageRegistration::single(
            StagePath::root(&self.id, name),
            component.id(),
            &[],
            ErasedType::of::<()>(),
            ErasedType::of::<O>(),
        ));
        self.flow(stage)
    }

    /// Adds one keyed source stage.
    ///
    /// An invalid or duplicate name is reported by [`build`](Self::build).
    #[must_use]
    pub fn source_many<K: 'static, O: 'static>(
        &mut self,
        name: &str,
        component: &Component<(), O>,
    ) -> KeyedFlow<K, O> {
        record_failure(&mut self.errors, validate_name(name));
        let stage = self.add(StageRegistration::by_key::<K>(
            StagePath::root(&self.id, name),
            component.id(),
            &[],
            ErasedType::of::<()>(),
            ErasedType::of::<O>(),
        ));
        self.keyed_flow(stage)
    }

    /// Connects one typed single input to next stage.
    ///
    /// A foreign handle, invalid name, or duplicate stage is reported by
    /// [`build`](Self::build).
    #[must_use]
    pub fn then<I: 'static, O: 'static>(
        &mut self,
        input: Flow<I>,
        name: &str,
        component: &Component<I, O>,
    ) -> Flow<O> {
        record_failure(&mut self.errors, validate_origin(input.origin, self.origin));
        record_failure(&mut self.errors, validate_name(name));
        let stage = self.add(StageRegistration::single(
            StagePath::root(&self.id, name),
            component.id(),
            &[input.stage],
            ErasedType::of::<I>(),
            ErasedType::of::<O>(),
        ));
        self.flow(stage)
    }

    /// Connects two heterogeneous typed inputs to one stage.
    ///
    /// A foreign handle, invalid name, or duplicate stage is reported by
    /// [`build`](Self::build).
    #[must_use]
    pub fn combine<Left: 'static, Right: 'static, Output: 'static>(
        &mut self,
        left: Flow<Left>,
        right: Flow<Right>,
        name: &str,
        component: &Component<(Left, Right), Output>,
    ) -> Flow<Output> {
        record_failure(&mut self.errors, validate_origin(left.origin, self.origin));
        record_failure(&mut self.errors, validate_origin(right.origin, self.origin));
        record_failure(&mut self.errors, validate_name(name));
        let stage = self.add(StageRegistration::single(
            StagePath::root(&self.id, name),
            component.id(),
            &[left.stage, right.stage],
            ErasedType::of::<(Left, Right)>(),
            ErasedType::of::<Output>(),
        ));
        self.flow(stage)
    }

    /// Maps one nested typed stage over every key.
    ///
    /// A foreign handle, invalid name, or duplicate stage is reported by
    /// [`build`](Self::build).
    #[must_use]
    pub fn map_each<K: 'static, I: 'static, O: 'static>(
        &mut self,
        input: KeyedFlow<K, I>,
        name: &str,
        component: &Component<I, O>,
    ) -> KeyedFlow<K, O> {
        record_failure(&mut self.errors, validate_origin(input.origin, self.origin));
        record_failure(&mut self.errors, validate_name(name));
        let stage = self.add(StageRegistration::by_key::<K>(
            StagePath::root(&self.id, name),
            component.id(),
            &[input.stage],
            ErasedType::of::<I>(),
            ErasedType::of::<O>(),
        ));
        self.keyed_flow(stage)
    }

    /// Joins two keyed flows under one explicit missing-key policy.
    ///
    /// Policy is execution metadata; key type is enforced by signature.
    /// A foreign handle, invalid name, or duplicate stage is reported by
    /// [`build`](Self::build).
    #[must_use]
    pub fn join_by_key<K: 'static, A: 'static, B: 'static>(
        &mut self,
        left: KeyedFlow<K, A>,
        right: KeyedFlow<K, B>,
        name: &str,
        policy: KeyJoinPolicy,
    ) -> KeyedFlow<K, JoinItem<A, B>> {
        record_failure(&mut self.errors, validate_origin(left.origin, self.origin));
        record_failure(&mut self.errors, validate_origin(right.origin, self.origin));
        record_failure(&mut self.errors, validate_name(name));
        let Ok(component) = ProviderId::new(format!("join.{name}")) else {
            // Reachable only for a non-canonical name, already recorded above;
            // build reports it before any stage index is read.
            return self.keyed_flow(self.stages.len());
        };
        let stage = self.add(StageRegistration::by_key::<K>(
            StagePath::root(&self.id, name),
            &component,
            &[left.stage, right.stage],
            ErasedType::of::<(A, B)>(),
            ErasedType::of::<JoinItem<A, B>>(),
        ));
        self.stages[stage].key_join_policy = Some(policy);
        self.keyed_flow(stage)
    }

    /// Aggregates keyed flow into one typed publication.
    ///
    /// A foreign handle, invalid name, or duplicate stage is reported by
    /// [`build`](Self::build).
    #[must_use]
    pub fn aggregate<K: 'static, I: 'static, O: 'static>(
        &mut self,
        input: KeyedFlow<K, I>,
        name: &str,
        component: &Component<Vec<I>, O>,
    ) -> Flow<O> {
        record_failure(&mut self.errors, validate_origin(input.origin, self.origin));
        record_failure(&mut self.errors, validate_name(name));
        let stage = self.add(StageRegistration::single(
            StagePath::root(&self.id, name),
            component.id(),
            &[input.stage],
            ErasedType::of::<Vec<I>>(),
            ErasedType::of::<O>(),
        ));
        self.flow(stage)
    }

    /// Selects composition output.
    ///
    /// A handle from another builder is reported by [`build`](Self::build).
    #[must_use]
    pub fn output<T: 'static>(mut self, output: Flow<T>) -> Self {
        record_failure(
            &mut self.errors,
            validate_origin(output.origin, self.origin),
        );
        self.output = Some(output.stage);
        self
    }

    /// Compiles immutable type-erased graph metadata.
    ///
    /// # Errors
    ///
    /// Returns the first recorded construction failure: non-canonical name,
    /// duplicate stage path, foreign flow handle, or missing output selection.
    pub fn build(self) -> Result<ProviderComposition, CompositionError> {
        if let Some(error) = self.errors.into_iter().next() {
            return Err(error);
        }
        let output = self
            .output
            .ok_or_else(|| CompositionError::new(CompositionFault::MissingOutput))?;
        Ok(ProviderComposition::from_nodes(
            self.id,
            self.stages,
            output,
        ))
    }

    // Registers the stage unconditionally so flow indices stay consistent;
    // a duplicate path is recorded and later reported by build.
    fn add(&mut self, registration: StageRegistration<'_>) -> usize {
        if !self.paths.insert(registration.path.clone()) {
            self.errors
                .push(CompositionError::new(CompositionFault::DuplicateStage(
                    registration.path.clone(),
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
        stage
    }

    const fn flow<T: 'static>(&self, stage: usize) -> Flow<T> {
        Flow {
            origin: self.origin,
            stage,
            _value: PhantomData,
        }
    }

    const fn keyed_flow<K: 'static, T: 'static>(&self, stage: usize) -> KeyedFlow<K, T> {
        KeyedFlow {
            origin: self.origin,
            stage,
            _value: PhantomData,
        }
    }
}

impl CompositionScope<'_> {
    /// Connects one typed input inside this scope.
    ///
    /// A foreign handle, invalid name, or duplicate stage is reported by the
    /// builder's [`build`](CompositionBuilder::build).
    #[must_use]
    pub fn then<I: 'static, O: 'static>(
        &mut self,
        input: Flow<I>,
        name: &str,
        component: &Component<I, O>,
    ) -> Flow<O> {
        record_failure(
            &mut self.builder.errors,
            validate_origin(input.origin, self.builder.origin),
        );
        record_failure(&mut self.builder.errors, validate_name(name));
        let path = StagePath::nested(&self.builder.id, &self.name, name);
        let stage = self.builder.add(StageRegistration::single(
            path,
            component.id(),
            &[input.stage],
            ErasedType::of::<I>(),
            ErasedType::of::<O>(),
        ));
        self.builder.flow(stage)
    }

    /// Connects two typed inputs inside this scope.
    ///
    /// A foreign handle, invalid name, or duplicate stage is reported by the
    /// builder's [`build`](CompositionBuilder::build).
    #[must_use]
    pub fn combine<Left: 'static, Right: 'static, Output: 'static>(
        &mut self,
        left: Flow<Left>,
        right: Flow<Right>,
        name: &str,
        component: &Component<(Left, Right), Output>,
    ) -> Flow<Output> {
        record_failure(
            &mut self.builder.errors,
            validate_origin(left.origin, self.builder.origin),
        );
        record_failure(
            &mut self.builder.errors,
            validate_origin(right.origin, self.builder.origin),
        );
        record_failure(&mut self.builder.errors, validate_name(name));
        let path = StagePath::nested(&self.builder.id, &self.name, name);
        let stage = self.builder.add(StageRegistration::single(
            path,
            component.id(),
            &[left.stage, right.stage],
            ErasedType::of::<(Left, Right)>(),
            ErasedType::of::<Output>(),
        ));
        self.builder.flow(stage)
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
            errors: Vec::new(),
        }
    }
}

/// Immutable composition recipe editor.
///
/// Edits chain by value; every recorded edit failure surfaces once, at
/// [`build`](Self::build).
#[derive(Debug)]
pub struct CompositionEditor {
    id: CompositionId,
    nodes: Vec<StageNode>,
    output: usize,
    errors: Vec<CompositionError>,
}

impl CompositionEditor {
    /// Inserts one type-preserving transform after named stage and rewires consumers.
    ///
    /// A missing stage, invalid or duplicate name, or type mismatch is
    /// reported by [`build`](Self::build).
    #[must_use]
    pub fn insert_after<T: 'static>(
        mut self,
        path: &str,
        name: &str,
        component: &Component<T, T>,
    ) -> Self {
        let result = self.try_insert_after::<T>(path, name, component);
        record_failure(&mut self.errors, result);
        self
    }

    /// Replaces implementation while preserving exact output type.
    ///
    /// A missing stage or incompatible type is reported by [`build`](Self::build).
    #[must_use]
    pub fn replace<I: 'static, O: 'static>(
        mut self,
        path: &str,
        component: &Component<I, O>,
    ) -> Self {
        let result = self.try_replace::<I, O>(path, component);
        record_failure(&mut self.errors, result);
        self
    }

    /// Removes an unused non-output stage.
    ///
    /// A missing or still-referenced stage is reported by [`build`](Self::build).
    #[must_use]
    pub fn remove(mut self, path: &str) -> Self {
        let result = self.try_remove(path);
        record_failure(&mut self.errors, result);
        self
    }

    /// Builds edited immutable composition.
    ///
    /// # Errors
    ///
    /// Returns the first recorded edit failure: missing stage, non-canonical or
    /// duplicate name, incompatible types, or removal of a referenced stage.
    pub fn build(self) -> Result<ProviderComposition, CompositionError> {
        if let Some(error) = self.errors.into_iter().next() {
            return Err(error);
        }
        Ok(ProviderComposition::from_nodes(
            self.id,
            self.nodes,
            self.output,
        ))
    }

    fn try_insert_after<T: 'static>(
        &mut self,
        path: &str,
        name: &str,
        component: &Component<T, T>,
    ) -> Result<(), CompositionError> {
        let index = self.index(path)?;
        if self.nodes[index].output.id != TypeId::of::<T>() {
            return Err(CompositionError::new(CompositionFault::TypeMismatch(
                self.nodes[index].path.clone(),
            )));
        }
        validate_name(name)?;
        let new_path = StagePath::root(&self.id, name);
        if self.nodes.iter().any(|node| node.path == new_path) {
            return Err(CompositionError::new(CompositionFault::DuplicateStage(
                new_path,
            )));
        }
        self.remap_stage_references(|stage| if stage >= index { stage + 1 } else { stage });
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

    fn try_replace<I: 'static, O: 'static>(
        &mut self,
        path: &str,
        component: &Component<I, O>,
    ) -> Result<(), CompositionError> {
        let index = self.index(path)?;
        if self.nodes[index].component_input.id != TypeId::of::<I>()
            || self.nodes[index].output.id != TypeId::of::<O>()
        {
            return Err(CompositionError::new(CompositionFault::TypeMismatch(
                self.nodes[index].path.clone(),
            )));
        }
        self.nodes[index].component = component.id().clone();
        Ok(())
    }

    fn try_remove(&mut self, path: &str) -> Result<(), CompositionError> {
        let index = self.index(path)?;
        if index == self.output || self.nodes.iter().any(|node| node.inputs.contains(&index)) {
            return Err(CompositionError::new(CompositionFault::DanglingInput(
                self.nodes[index].path.clone(),
            )));
        }
        self.nodes.remove(index);
        self.remap_stage_references(|stage| if stage > index { stage - 1 } else { stage });
        Ok(())
    }

    fn index(&self, path: &str) -> Result<usize, CompositionError> {
        self.nodes
            .iter()
            .position(|node| node.path.as_str() == path)
            .ok_or_else(|| {
                CompositionError::new(CompositionFault::StageNotFound(StagePath(path.into())))
            })
    }

    // Every stage index lives in node inputs and the output selector; one remap
    // keeps both consistent when insertion or removal shifts positions.
    fn remap_stage_references(&mut self, remap: impl Fn(usize) -> usize) {
        for node in &mut self.nodes {
            for input in &mut node.inputs {
                *input = remap(*input);
            }
        }
        self.output = remap(self.output);
    }
}

fn validate_name(name: &str) -> Result<(), CompositionError> {
    if !is_canonical_ascii_name(name, STAGE_NAME_BYTES_MAX, STAGE_NAME_PUNCTUATION) {
        return Err(CompositionError::new(CompositionFault::InvalidName));
    }
    Ok(())
}

fn validate_origin(flow_origin: u64, builder_origin: u64) -> Result<(), CompositionError> {
    if flow_origin != builder_origin {
        return Err(CompositionError::new(CompositionFault::ForeignFlow));
    }
    Ok(())
}

// Deferred-validation kernel: chained construction records failures here and
// build reports the first one.
fn record_failure(errors: &mut Vec<CompositionError>, result: Result<(), CompositionError>) {
    if let Err(error) = result {
        errors.push(error);
    }
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

/// Both keyed inputs of one join, named by side.
#[derive(Debug)]
pub struct JoinSides<'a, K, L, R> {
    /// Left keyed input.
    pub left: &'a BTreeMap<K, L>,
    /// Right keyed input.
    pub right: &'a BTreeMap<K, R>,
}

impl<K, L, R> Copy for JoinSides<'_, K, L, R> {}

impl<K, L, R> Clone for JoinSides<'_, K, L, R> {
    fn clone(&self) -> Self {
        *self
    }
}

/// Aligns keyed values without silently dropping missing-item coverage.
#[must_use]
pub fn join_keyed<K, L, R>(
    policy: KeyJoinPolicy,
    sides: JoinSides<'_, K, L, R>,
) -> JoinCoverage<K, L, R>
where
    K: Clone + Ord,
    L: Clone,
    R: Clone,
{
    let keys = sides
        .left
        .keys()
        .chain(sides.right.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut items = BTreeMap::new();
    let mut missing = Vec::new();
    for key in keys {
        let left_value = sides.left.get(&key).cloned();
        let right_value = sides.right.get(&key).cloned();
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheViolation {
    /// Requested key set exceeds configured cache bound.
    TooManyKeys,
}

/// One rejected per-key cache request: which rule it broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheFault {
    violation: CacheViolation,
}

impl CacheFault {
    /// Returns stable failure classification.
    #[must_use]
    pub const fn violation(self) -> CacheViolation {
        self.violation
    }
}

impl Fault for CacheFault {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::LimitExceeded)
    }

    fn context(&self) -> Vec<ErrorContext> {
        vec![ErrorContext::new("violation", fault_label(&self.violation))]
    }
}

/// Per-key cache failure.
pub type CacheError = Error<CacheFault>;

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
            return Err(CacheError::new(CacheFault {
                violation: CacheViolation::TooManyKeys,
            }));
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

    fn composition_id(id: &str) -> CompositionId {
        CompositionId::new(id).expect("valid composition fixture")
    }

    #[test]
    fn linear_and_multi_input_flows_compile_without_visible_downcast() {
        let mut builder = CompositionBuilder::new(composition_id("python-context"));
        let source = builder.source("project", &component::<(), Sources>("project"));
        let syntax = builder.then(source, "syntax", &component::<Sources, Syntax>("syntax"));
        let semantics = builder.scope("semantics").combine(
            source,
            syntax,
            "ty",
            &component::<(Sources, Syntax), Semantics>("semantics"),
        );
        let facts = builder.combine(
            syntax,
            semantics,
            "merge",
            &component::<(Syntax, Semantics), Facts>("merge"),
        );
        let composition = builder.output(facts).build().expect("complete graph");

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
        let mut builder = CompositionBuilder::new(composition_id("history"));
        let revisions =
            builder.source_many::<u64, Sources>("revisions", &component::<(), Sources>("git"));
        let syntax = builder.map_each(revisions, "syntax", &component::<Sources, Syntax>("syntax"));
        let metadata = builder.map_each(
            revisions,
            "metadata",
            &component::<Sources, Metadata>("metadata"),
        );
        let joined = builder.join_by_key(syntax, metadata, "join", KeyJoinPolicy::Exact);
        let aggregate = builder.aggregate(
            joined,
            "aggregate",
            &component::<Vec<JoinItem<Syntax, Metadata>>, Facts>("aggregate"),
        );
        let composition = builder.output(aggregate).build().expect("complete graph");

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
    fn cloned_keyed_flow_handle_stays_connectable() {
        let mut builder = CompositionBuilder::new(composition_id("keyed-clone"));
        let revisions =
            builder.source_many::<u64, Sources>("revisions", &component::<(), Sources>("git"));
        #[expect(clippy::clone_on_copy, reason = "exercises the manual Clone impl")]
        let cloned = revisions.clone();
        let syntax = builder.map_each(cloned, "syntax", &component::<Sources, Syntax>("syntax"));
        let aggregate = builder.aggregate(
            syntax,
            "aggregate",
            &component::<Vec<Syntax>, Facts>("aggregate"),
        );
        let composition = builder.output(aggregate).build().expect("complete graph");
        assert_eq!(
            composition
                .stage("keyed-clone.syntax")
                .expect("stage")
                .inputs()[0]
                .as_str(),
            "keyed-clone.revisions"
        );
    }

    #[test]
    fn join_with_control_character_name_fails_at_build() {
        let mut builder = CompositionBuilder::new(composition_id("history"));
        let revisions =
            builder.source_many::<u64, Sources>("revisions", &component::<(), Sources>("git"));
        let syntax = builder.map_each(revisions, "syntax", &component::<Sources, Syntax>("syntax"));
        let metadata = builder.map_each(
            revisions,
            "metadata",
            &component::<Sources, Metadata>("metadata"),
        );
        let joined = builder.join_by_key(syntax, metadata, "join\u{0}", KeyJoinPolicy::Inner);
        let aggregate = builder.aggregate(
            joined,
            "aggregate",
            &component::<Vec<JoinItem<Syntax, Metadata>>, Facts>("aggregate"),
        );
        let error = builder
            .output(aggregate)
            .build()
            .expect_err("control character in join name must fail");
        assert_eq!(error.fault(), &CompositionFault::InvalidName);
    }

    #[test]
    fn join_sides_copy_and_clone_agree() {
        let left = BTreeMap::from([(1, "left-1")]);
        let right = BTreeMap::from([(1, "right-1")]);
        let sides = JoinSides {
            left: &left,
            right: &right,
        };
        #[expect(clippy::clone_on_copy, reason = "exercises the manual Clone impl")]
        let cloned = sides.clone();
        assert_eq!(
            join_keyed(KeyJoinPolicy::Exact, sides).items,
            join_keyed(KeyJoinPolicy::Exact, cloned).items
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
            let joined = join_keyed(
                policy,
                JoinSides {
                    left: &left,
                    right: &right,
                },
            );
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
        assert_eq!(error.fault().violation(), CacheViolation::TooManyKeys);
        assert_eq!(
            error.context(),
            vec![ErrorContext::new("violation", "too_many_keys")]
        );
        assert_eq!(
            error.to_string(),
            "the request exceeded a declared resource limit: violation too_many_keys; \
             resize the request below the named limit, or raise that limit \
             in the workspace configuration"
        );
        assert_eq!(cache.get(&2).map(String::as_str), Some("2"));
    }

    #[test]
    fn editing_replaces_compatible_stage_and_rejects_dangling_remove() {
        let mut builder = CompositionBuilder::new(composition_id("core-syntax"));
        let source = builder.source("project", &component::<(), Sources>("project"));
        let _spare = builder.source("spare", &component::<(), Sources>("spare"));
        let syntax = builder.then(source, "syntax", &component::<Sources, Syntax>("syntax"));
        let composition = builder.output(syntax).build().expect("complete graph");

        let mismatch = composition
            .edit()
            .replace(
                "core-syntax.syntax",
                &component::<Metadata, Syntax>("wrong-input"),
            )
            .build()
            .expect_err("replacement input type must match");
        assert!(matches!(
            mismatch.fault(),
            CompositionFault::TypeMismatch(_)
        ));

        let error = composition
            .edit()
            .remove("core-syntax.project")
            .build()
            .expect_err("used source cannot be removed");
        assert!(matches!(error.fault(), CompositionFault::DanglingInput(_)));
        assert_eq!(
            error.to_string(),
            "the workspace configuration failed validation: \
             rule dangling_input, stage core-syntax.project; \
             correct the reported configuration field, then retry"
        );

        let edited = composition
            .edit()
            .insert_after(
                "core-syntax.syntax",
                "normalize-docs",
                &component::<Syntax, Syntax>("normalize-docs"),
            )
            .replace(
                "core-syntax.syntax",
                &component::<Sources, Syntax>("custom-syntax"),
            )
            .remove("core-syntax.spare")
            .build()
            .expect("chained edits");
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
        let mut builder = CompositionBuilder::new(composition_id("tailwind"));
        let authored = builder.source("css", &component::<(), Sources>("css-source"));
        let templates = builder.source("templates", &component::<(), Templates>("templates"));
        let generated = builder.scope("external").then(
            templates,
            "tailwind",
            &component::<Templates, GeneratedSource>("tailwind-command"),
        );
        let generated_syntax = builder.then(
            generated,
            "generated-syntax",
            &component::<GeneratedSource, Syntax>("css-syntax"),
        );
        let authored_syntax = builder.then(
            authored,
            "authored-syntax",
            &component::<Sources, Syntax>("css-syntax"),
        );
        let merged = builder.combine(
            authored_syntax,
            generated_syntax,
            "merge",
            &component::<(Syntax, Syntax), Facts>("css-merge"),
        );
        let composition = builder.output(merged).build().expect("complete route");

        assert_eq!(
            composition
                .stage("tailwind.external.tailwind")
                .expect("external stage")
                .output_type(),
            type_name::<GeneratedSource>()
        );
    }

    #[test]
    fn build_reports_first_of_duplicate_foreign_and_missing_output() {
        let mut first = CompositionBuilder::new(composition_id("first"));
        let flow = first.source("source", &component::<(), Sources>("source"));
        let _duplicate = first.source("source", &component::<(), Sources>("other"));
        let duplicate = first
            .output(flow)
            .build()
            .expect_err("duplicate scoped path must fail");
        assert!(matches!(
            duplicate.fault(),
            CompositionFault::DuplicateStage(_)
        ));

        let mut second = CompositionBuilder::new(composition_id("second"));
        let _foreign = second.then(flow, "syntax", &component::<Sources, Syntax>("syntax"));
        let foreign = second
            .build()
            .expect_err("foreign handle must fail before missing output");
        assert_eq!(foreign.fault(), &CompositionFault::ForeignFlow);

        let missing = CompositionBuilder::new(composition_id("third"))
            .build()
            .expect_err("output is required");
        assert_eq!(missing.fault(), &CompositionFault::MissingOutput);
    }

    #[test]
    fn stage_names_are_bounded_canonical_ascii() {
        let invalid = ["", "Uppercase", "two.words"];
        for name in invalid {
            let mut builder = CompositionBuilder::new(composition_id("names"));
            let flow = builder.source(name, &component::<(), Sources>("source"));
            let error = builder
                .output(flow)
                .build()
                .expect_err("non-canonical stage name must fail");
            assert_eq!(error.fault(), &CompositionFault::InvalidName);
        }

        let mut builder = CompositionBuilder::new(composition_id("names"));
        let over_limit = "x".repeat(STAGE_NAME_BYTES_MAX + 1);
        let flow = builder.source(&over_limit, &component::<(), Sources>("source"));
        let error = builder
            .output(flow)
            .build()
            .expect_err("oversized stage name must fail");
        assert_eq!(error.fault(), &CompositionFault::InvalidName);
    }

    #[test]
    fn descriptors_expose_identity_paths_and_recorded_types() {
        let mut builder = CompositionBuilder::new(composition_id("catalog"));
        let source = builder.source("project", &component::<(), Sources>("project"));
        let syntax = builder.then(source, "syntax", &component::<Sources, Syntax>("syntax"));
        let composition = builder.output(syntax).build().expect("complete graph");

        assert_eq!(composition.id().as_str(), "catalog");
        let stage = composition.stage("catalog.syntax").expect("syntax stage");
        assert_eq!(stage.path().to_string(), "catalog.syntax");
        assert_eq!(stage.component_input_type(), type_name::<Sources>());
        assert_eq!(stage.cardinality(), FlowCardinality::Single);
    }

    #[test]
    fn error_context_names_offending_stage_when_present() {
        let mut builder = CompositionBuilder::new(composition_id("dup"));
        let flow = builder.source("stage", &component::<(), Sources>("stage"));
        let _second = builder.source("stage", &component::<(), Sources>("again"));
        let duplicate = builder
            .output(flow)
            .build()
            .expect_err("duplicate stage path");
        assert_eq!(
            duplicate.fault().stage().map(StagePath::as_str),
            Some("dup.stage")
        );

        let missing = CompositionBuilder::new(composition_id("empty"))
            .build()
            .expect_err("missing output");
        assert!(missing.fault().stage().is_none());
    }

    #[test]
    fn cloned_flow_handle_stays_connectable() {
        let mut builder = CompositionBuilder::new(composition_id("clone"));
        let flow = builder.source("project", &component::<(), Sources>("project"));
        #[expect(clippy::clone_on_copy, reason = "exercises the manual Clone impl")]
        let cloned = flow.clone();
        let syntax = builder.then(cloned, "syntax", &component::<Sources, Syntax>("syntax"));
        let composition = builder.output(syntax).build().expect("complete graph");
        assert_eq!(
            composition.stage("clone.syntax").expect("stage").inputs()[0].as_str(),
            "clone.project"
        );
    }

    #[test]
    fn non_canonical_scope_name_fails_at_build() {
        let mut builder = CompositionBuilder::new(composition_id("scoped"));
        let source = builder.source("project", &component::<(), Sources>("project"));
        let scoped = builder.scope("Uppercase").then(
            source,
            "syntax",
            &component::<Sources, Syntax>("syntax"),
        );
        let error = builder
            .output(scoped)
            .build()
            .expect_err("scope name must be canonical");
        assert_eq!(error.fault(), &CompositionFault::InvalidName);
    }

    #[test]
    fn output_handle_from_another_builder_is_foreign() {
        let mut first = CompositionBuilder::new(composition_id("one"));
        let flow = first.source("source", &component::<(), Sources>("source"));
        let mut second = CompositionBuilder::new(composition_id("two"));
        let _own = second.source("source", &component::<(), Sources>("source"));
        let error = second
            .output(flow)
            .build()
            .expect_err("foreign output handle");
        assert_eq!(error.fault(), &CompositionFault::ForeignFlow);
    }

    fn project_syntax_composition() -> ProviderComposition {
        let mut builder = CompositionBuilder::new(composition_id("core"));
        let source = builder.source("project", &component::<(), Sources>("project"));
        let syntax = builder.then(source, "syntax", &component::<Sources, Syntax>("syntax"));
        builder.output(syntax).build().expect("complete graph")
    }

    #[test]
    fn editor_rejects_missing_stage_incompatible_and_duplicate_inserts() {
        let composition = project_syntax_composition();

        let not_found = composition
            .edit()
            .replace("core.absent", &component::<Sources, Syntax>("other"))
            .build()
            .expect_err("unknown stage path");
        assert!(matches!(
            not_found.fault(),
            CompositionFault::StageNotFound(_)
        ));
        assert_eq!(
            not_found.fault().stage().map(StagePath::as_str),
            Some("core.absent")
        );

        let mismatch = composition
            .edit()
            .insert_after(
                "core.project",
                "cleanup",
                &component::<Syntax, Syntax>("cleanup"),
            )
            .build()
            .expect_err("transform type must match stage output");
        assert!(matches!(
            mismatch.fault(),
            CompositionFault::TypeMismatch(_)
        ));
        assert_eq!(
            mismatch.fault().stage().map(StagePath::as_str),
            Some("core.project")
        );

        let duplicate = composition
            .edit()
            .insert_after(
                "core.syntax",
                "project",
                &component::<Syntax, Syntax>("shadow"),
            )
            .build()
            .expect_err("inserted stage path already exists");
        assert!(matches!(
            duplicate.fault(),
            CompositionFault::DuplicateStage(_)
        ));

        let invalid = composition
            .edit()
            .insert_after(
                "core.syntax",
                "Bad-Name",
                &component::<Syntax, Syntax>("bad"),
            )
            .build()
            .expect_err("inserted stage name must be canonical");
        assert_eq!(invalid.fault(), &CompositionFault::InvalidName);
    }

    #[test]
    fn editor_reports_first_failure_across_chained_edits() {
        let composition = project_syntax_composition();
        let error = composition
            .edit()
            .replace("core.syntax", &component::<Metadata, Syntax>("wrong-input"))
            .remove("core.absent")
            .build()
            .expect_err("first failure must win");
        assert!(matches!(error.fault(), CompositionFault::TypeMismatch(_)));
    }

    #[test]
    fn context_reports_rule_and_stage_for_every_composition_error_kind() {
        let mut builder = CompositionBuilder::new(composition_id("ctx"));
        let invalid = builder.source("Uppercase", &component::<(), Sources>("source"));
        let invalid_error = builder
            .output(invalid)
            .build()
            .expect_err("non-canonical name must fail");
        assert_eq!(
            invalid_error.context(),
            vec![ErrorContext::new("rule", "invalid_name")]
        );

        let mut builder = CompositionBuilder::new(composition_id("ctx"));
        let flow = builder.source("stage", &component::<(), Sources>("stage"));
        let _second = builder.source("stage", &component::<(), Sources>("again"));
        let duplicate_error = builder
            .output(flow)
            .build()
            .expect_err("duplicate scoped path must fail");
        assert_eq!(
            duplicate_error.context(),
            vec![
                ErrorContext::new("rule", "duplicate_stage"),
                ErrorContext::new("stage", "ctx.stage"),
            ]
        );

        let mut foreign_source = CompositionBuilder::new(composition_id("foreign"));
        let foreign_flow = foreign_source.source("source", &component::<(), Sources>("source"));
        let mut foreign_target = CompositionBuilder::new(composition_id("target"));
        let _own = foreign_target.source("source", &component::<(), Sources>("source"));
        let foreign_error = foreign_target
            .output(foreign_flow)
            .build()
            .expect_err("foreign output handle must fail");
        assert_eq!(
            foreign_error.context(),
            vec![ErrorContext::new("rule", "foreign_flow")]
        );

        let missing_error = CompositionBuilder::new(composition_id("empty"))
            .build()
            .expect_err("missing output must fail");
        assert_eq!(
            missing_error.context(),
            vec![ErrorContext::new("rule", "missing_output")]
        );

        let composition = project_syntax_composition();
        let not_found_error = composition
            .edit()
            .replace("core.absent", &component::<Sources, Syntax>("other"))
            .build()
            .expect_err("unknown stage path must fail");
        assert_eq!(
            not_found_error.context(),
            vec![
                ErrorContext::new("rule", "stage_not_found"),
                ErrorContext::new("stage", "core.absent"),
            ]
        );

        let mismatch_error = composition
            .edit()
            .replace("core.syntax", &component::<Metadata, Syntax>("wrong-input"))
            .build()
            .expect_err("incompatible replacement must fail");
        assert_eq!(
            mismatch_error.context(),
            vec![
                ErrorContext::new("rule", "type_mismatch"),
                ErrorContext::new("stage", "core.syntax"),
            ]
        );

        let dangling_error = composition
            .edit()
            .remove("core.project")
            .build()
            .expect_err("used source cannot be removed");
        assert_eq!(
            dangling_error.context(),
            vec![
                ErrorContext::new("rule", "dangling_input"),
                ErrorContext::new("stage", "core.project"),
            ]
        );
    }
}
