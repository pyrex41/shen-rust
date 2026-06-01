//! Error type that flows through `eval`. Maps to Shen's `simple-error` /
//! `trap-error` semantics: an error carries a string message and is
//! re-presented as a `Value::Error` to user handlers.

use std::fmt;
use std::rc::Rc;

/// Distinguishes an ordinary Shen-level error (catchable by `trap-error`)
/// from a runtime *cancellation* — the latter is raised when the evaluator
/// exhausts an `Interp::set_budget` instruction-count budget (or wall-clock
/// deadline) and must propagate past `trap-error` rather than be swallowed
/// by user-level error handlers. See `interp::eval::Interp::set_budget`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// Normal Shen error — `trap-error` catches it and runs the handler.
    Normal,
    /// Evaluation was cancelled (budget/deadline). Bypasses `trap-error`.
    Cancelled,
}

/// Error raised by the Shen runtime. Carries an interned message string so
/// it can be cheaply cloned during handler dispatch.
#[derive(Clone, Debug)]
pub struct ShenError {
    pub message: Rc<str>,
    pub kind: ErrorKind,
}

impl ShenError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self {
            message: Rc::from(msg.into()),
            kind: ErrorKind::Normal,
        }
    }

    /// Construct a cancellation error. Raised by the evaluator when a step
    /// budget or deadline set via `Interp::set_budget` is exhausted; it
    /// propagates past `trap-error` so a scheduler can tell a budget abort
    /// from a genuine Shen error.
    pub fn cancelled(msg: impl Into<String>) -> Self {
        Self {
            message: Rc::from(msg.into()),
            kind: ErrorKind::Cancelled,
        }
    }

    /// True if this error is a cancellation (budget/deadline exhausted).
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        matches!(self.kind, ErrorKind::Cancelled)
    }
}

impl fmt::Display for ShenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ShenError {}

/// Convenience alias used throughout the runtime.
pub type ShenResult<T> = Result<T, ShenError>;

/// Quick constructor: `bail!("...")` returns `Err(ShenError::new(...))`.
#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::error::ShenError::new(format!($($arg)*)))
    };
}
