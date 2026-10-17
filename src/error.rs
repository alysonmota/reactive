use std::fmt;

/// Errors are explicit and never replace an invalid access with a default value.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeError {
    InvalidHandle,
    WrongRuntime,
    TypeMismatch,
    MissingOwner,
    OwnerDisposed,
    UninitializedValue,
    UpdateConflict,
    WriteForbidden,
    MutationForbidden,
    Reentrancy,
    Cycle,
    DepthLimit,
    CounterExhausted,
    CallbackFailed(&'static str),
    InvariantViolation(&'static str),
    Panic,
    Poisoned,
    RuntimeClosed,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidHandle => "invalid or disposed handle",
            Self::WrongRuntime => "handle belongs to another runtime",
            Self::TypeMismatch => "node kind or value type does not match the handle",
            Self::MissingOwner => "node creation requires an explicit owner",
            Self::OwnerDisposed => "current owner is disposed or disposing",
            Self::UninitializedValue => "derived value has not been initialized",
            Self::UpdateConflict => "signal changed during update",
            Self::WriteForbidden => "writes are forbidden in this callback",
            Self::MutationForbidden => "structural mutation is forbidden in this callback",
            Self::Reentrancy => "runtime reentry is forbidden in this context",
            Self::Cycle => "computation is already being evaluated",
            Self::DepthLimit => "evaluation or owner depth limit exceeded",
            Self::CounterExhausted => "monotonic counter exhausted",
            Self::CallbackFailed(message) => return write!(f, "callback failed: {message}"),
            Self::InvariantViolation(message) => {
                return write!(f, "runtime invariant violated: {message}");
            }
            Self::Panic => "user callback panicked",
            Self::Poisoned => "runtime is poisoned; only diagnostics and teardown are available",
            Self::RuntimeClosed => "runtime has been shut down",
        };
        f.write_str(message)
    }
}

impl std::error::Error for RuntimeError {}
