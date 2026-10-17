//! Type-erased values, dependency edges and independent evaluation metadata.
#![allow(dead_code)]

use std::{
    any::{Any, TypeId},
    cell::RefCell,
    collections::HashSet,
    rc::Rc,
};

use crate::{Runtime, RuntimeError, arena::SlotId};

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    Clean = 0,
    Check = 1,
    Dirty = 2,
}

/// Evaluation phase is independent of cached value validity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EvaluationPhase {
    #[default]
    Idle,
    Checking,
    Cleaning,
    Executing,
}

/// Logical queue membership, not evidence that a value is clean or dirty.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QueueMembership {
    #[default]
    None,
    Pending,
    Current(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NodeKind {
    Signal,
    Memo,
    Effect,
}

pub(crate) type Value = Rc<dyn Any>;
pub(crate) type MemoCompute =
    Box<dyn FnMut(&Runtime, Option<&Value>) -> Result<Value, RuntimeError>>;
pub(crate) type EffectCallback = Box<dyn FnMut(&Runtime) -> Result<(), RuntimeError>>;
pub(crate) type Comparator = Rc<dyn Fn(&dyn Any, &dyn Any) -> Result<bool, RuntimeError>>;

/// Immutable per-T cell descriptor shared by all instances of that type.
/// Typed extraction ends the cell borrow before guarded resource release.
pub(crate) struct ValueType {
    pub(crate) id: TypeId,
    pub(crate) storage_id: TypeId,
    pub(crate) clear: fn(&Runtime, &Value) -> Option<crate::runtime::PanicPayload>,
    pub(crate) initialized: fn(&Value) -> Result<bool, RuntimeError>,
}
impl ValueType {
    pub(crate) const fn of<T: 'static>() -> Self {
        Self {
            id: TypeId::of::<T>(),
            storage_id: TypeId::of::<RefCell<Option<T>>>(),
            clear: clear_value::<T>,
            initialized: initialized_value::<T>,
        }
    }
}

pub(crate) struct SignalData {
    pub(crate) value: Value,
    pub(crate) value_type: &'static ValueType,
    pub(crate) equals: Comparator,
}

/// First-read revisions are provisional until the reader reconciles its edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReadRecord {
    pub(crate) source: SlotId,
    pub(crate) freshness_revision: u64,
    pub(crate) value_revision: u64,
}

// One isolated experiment: edge buffers and algorithms remain unchanged.
#[cfg(feature = "experimental-inline-reads")]
pub(crate) type ReadBuffer = smallvec::SmallVec<[ReadRecord; 2]>;
#[cfg(not(feature = "experimental-inline-reads"))]
pub(crate) type ReadBuffer = Vec<ReadRecord>;

#[derive(Clone, Copy, Debug)]
pub(crate) struct SourceEdge {
    pub(crate) source: SlotId,
    pub(crate) observer_index: usize,
    pub(crate) value_revision: u64,
    pub(crate) live: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ObserverEdge {
    pub(crate) reader: SlotId,
    pub(crate) source_index: usize,
}

/// Each reader owns its reusable buffers; the runtime stacks active reader IDs.
#[derive(Default)]
pub(crate) struct TrackingContext {
    pub(crate) execution_epoch: u64,
    pub(crate) stable_prefix_cursor: usize,
    pub(crate) diverged: bool,
    pub(crate) hashing: bool,
    // When hashing, covers the first len() unique reads; fast-prefix reads may
    // leave a suffix unseeded until the next fallback. Never overwrites records.
    pub(crate) seen_sources: HashSet<SlotId>,
}

pub(crate) enum Payload {
    Signal(SignalData),
    Memo {
        value_type: &'static ValueType,
        value: Option<Value>,
        compute: Option<MemoCompute>,
        equals: Comparator,
    },
    Effect(Option<EffectCallback>),
}

impl Payload {
    pub(crate) fn effect(callback: EffectCallback) -> Self {
        Self::Effect(Some(callback))
    }
    pub(crate) fn comparator<T: 'static>(
        comparator: impl Fn(&T, &T) -> bool + 'static,
    ) -> Comparator {
        Rc::new(move |left, right| {
            if let Some(left) = left.downcast_ref::<RefCell<Option<T>>>() {
                let left = left.try_borrow().map_err(|_| RuntimeError::Reentrancy)?;
                let left = left.as_ref().ok_or(RuntimeError::UninitializedValue)?;
                if let Some(right) = right.downcast_ref::<RefCell<Option<T>>>() {
                    let right = right.try_borrow().map_err(|_| RuntimeError::Reentrancy)?;
                    return Ok(comparator(
                        left,
                        right.as_ref().ok_or(RuntimeError::UninitializedValue)?,
                    ));
                }
                return Ok(comparator(
                    left,
                    right
                        .downcast_ref::<T>()
                        .ok_or(RuntimeError::TypeMismatch)?,
                ));
            }
            Err(RuntimeError::TypeMismatch)
        })
    }
    pub(crate) fn signal<T: 'static>(
        value: T,
        comparator: impl Fn(&T, &T) -> bool + 'static,
    ) -> Self {
        Self::Signal(SignalData {
            value: Rc::new(RefCell::new(Some(value))),
            value_type: &const { ValueType::of::<T>() },
            equals: Self::comparator(comparator),
        })
    }

    pub(crate) fn kind(&self) -> NodeKind {
        match self {
            Self::Signal(_) => NodeKind::Signal,
            Self::Memo { .. } => NodeKind::Memo,
            Self::Effect(_) => NodeKind::Effect,
        }
    }

    pub(crate) fn value_type(&self) -> Option<TypeId> {
        match self {
            Self::Signal(data) => Some(data.value_type.id),
            Self::Memo { value_type, .. } => Some(value_type.id),
            Self::Effect(_) => None,
        }
    }
}

pub(crate) struct Metadata {
    pub(crate) state: State,
    pub(crate) phase: EvaluationPhase,
    pub(crate) queue: QueueMembership,
    pub(crate) initialized: bool,
    pub(crate) freshness_revision: u64,
    pub(crate) value_revision: u64,
    pub(crate) invalidation_revision: u64,
    pub(crate) last_processed_round: Option<u64>,
    pub(crate) last_executed_round: Option<u64>,
    pub(crate) failure: Option<Rc<RuntimeError>>,
    pub(crate) creation_sequence: u64,
}

pub(crate) struct Node {
    pub(crate) owner: SlotId,
    pub(crate) owner_index: usize,
    pub(crate) execution_owner: Option<SlotId>,
    pub(crate) metadata: Metadata,
    pub(crate) payload: Payload,
    pub(crate) observers: Vec<ObserverEdge>,
    pub(crate) sources: Vec<SourceEdge>,
    pub(crate) reads: ReadBuffer,
    pub(crate) tracking: TrackingContext,
    // Dirty readers may retain ID-only tombstones at stable physical indices;
    // successful reconciliation removes them. Live edges keep valid back-indices.
    pub(crate) source_holes: bool,
}

impl Node {
    #[cfg(test)]
    pub(crate) fn snapshot<T: Clone + 'static>(
        &self,
        kind: NodeKind,
    ) -> Result<Rc<T>, RuntimeError> {
        let cell = self.value_cell::<T>(kind)?;
        let value = cell.borrow();
        Ok(Rc::new(
            value
                .as_ref()
                .ok_or(RuntimeError::UninitializedValue)?
                .clone(),
        ))
    }
    pub(crate) fn new(owner: SlotId, payload: Payload, sequence: u64) -> Self {
        let signal = payload.kind() == NodeKind::Signal;
        let initialized = matches!(
            &payload,
            Payload::Signal(_) | Payload::Memo { value: Some(_), .. }
        );
        Self {
            owner,
            owner_index: 0,
            execution_owner: None,
            metadata: Metadata {
                state: if signal { State::Clean } else { State::Dirty },
                phase: EvaluationPhase::Idle,
                // Public effect insertion enrolls this node separately.
                queue: QueueMembership::None,
                initialized,
                freshness_revision: 0,
                value_revision: 0,
                invalidation_revision: 0,
                last_processed_round: None,
                last_executed_round: None,
                failure: None,
                creation_sequence: sequence,
            },
            payload,
            observers: Vec::new(),
            sources: Vec::new(),
            reads: ReadBuffer::default(),
            tracking: TrackingContext::default(),
            source_holes: false,
        }
    }

    pub(crate) fn validate_type<T: 'static>(&self, kind: NodeKind) -> Result<(), RuntimeError> {
        if self.payload.kind() != kind || self.payload.value_type() != Some(TypeId::of::<T>()) {
            return Err(RuntimeError::TypeMismatch);
        }
        // Validate the actual cell type, not merely the declared value TypeId.
        match &self.payload {
            Payload::Signal(data) if !data.value.is::<RefCell<Option<T>>>() => {
                return Err(RuntimeError::TypeMismatch);
            }
            Payload::Memo {
                value: Some(value), ..
            } if !value.is::<RefCell<Option<T>>>() => return Err(RuntimeError::TypeMismatch),
            _ => {}
        }
        Ok(())
    }

    /// Internal cell access only. Public APIs never return this cell or its Ref.
    pub(crate) fn value_cell<T: 'static>(
        &self,
        kind: NodeKind,
    ) -> Result<Rc<RefCell<Option<T>>>, RuntimeError> {
        let value = self.erased_value::<T>(kind)?;
        Rc::clone(value)
            .downcast::<RefCell<Option<T>>>()
            .map_err(|_| RuntimeError::TypeMismatch)
    }

    /// No user code: copy under the graph borrow without a temporary Rc.
    pub(crate) fn copy_value<T: Copy + 'static>(&self, kind: NodeKind) -> Result<T, RuntimeError> {
        let value = self.erased_value::<T>(kind)?;
        let cell = value
            .downcast_ref::<RefCell<Option<T>>>()
            .ok_or(RuntimeError::TypeMismatch)?;
        let value = cell.try_borrow().map_err(|_| RuntimeError::Reentrancy)?;
        value
            .as_ref()
            .copied()
            .ok_or(RuntimeError::UninitializedValue)
    }

    fn erased_value<T: 'static>(&self, kind: NodeKind) -> Result<&Value, RuntimeError> {
        if self.payload.kind() != kind {
            return Err(RuntimeError::TypeMismatch);
        }
        let value = match &self.payload {
            Payload::Signal(data) => {
                if data.value_type.id != TypeId::of::<T>() {
                    return Err(RuntimeError::TypeMismatch);
                }
                &data.value
            }
            Payload::Memo {
                value_type, value, ..
            } => {
                if value_type.id != TypeId::of::<T>() {
                    return Err(RuntimeError::TypeMismatch);
                }
                value.as_ref().ok_or(RuntimeError::UninitializedValue)?
            }
            Payload::Effect(_) => return Err(RuntimeError::TypeMismatch),
        };
        Ok(value)
    }
}

fn clear_value<T: 'static>(
    runtime: &Runtime,
    value: &Value,
) -> Option<crate::runtime::PanicPayload> {
    let cell = value.downcast_ref::<RefCell<Option<T>>>()?;
    let retired = cell.borrow_mut().take();
    runtime.release_option(retired)
}

fn initialized_value<T: 'static>(value: &Value) -> Result<bool, RuntimeError> {
    let cell = value
        .downcast_ref::<RefCell<Option<T>>>()
        .ok_or(RuntimeError::TypeMismatch)?;
    Ok(cell
        .try_borrow()
        .map_err(|_| RuntimeError::Reentrancy)?
        .is_some())
}
