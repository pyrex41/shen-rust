//! Error type that flows through `eval`. Maps to Shen's `simple-error` /
//! `trap-error` semantics: an error carries a string message and is
//! re-presented as a `Value::Error` to user handlers.

use std::fmt;
use std::rc::Rc;

/// Error raised by the Shen runtime. Carries an interned message string so
/// it can be cheaply cloned during handler dispatch.
#[derive(Clone, Debug)]
pub struct ShenError {
    pub message: Rc<str>,
}

impl ShenError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self {
            message: Rc::from(msg.into()),
        }
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
