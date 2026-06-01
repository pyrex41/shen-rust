//! Type-tag helpers for embedding Cedar handles in `Value::Foreign`.
//!
//! `Value::Foreign(Rc<dyn Any>)` already supports arbitrary host types;
//! these helpers just give us named constructors/accessors and clean
//! error messages.

use std::rc::Rc;

use cedar_policy::{Authorizer, Entities, Policy, PolicySet, Request, Schema};

use crate::error::{ShenError, ShenResult};
use crate::value::Value;

/// Wrap a concrete Cedar type as a Shen `Foreign` value.
pub fn wrap<T: 'static>(x: T) -> Value {
    Value::foreign(Rc::new(x))
}

/// Downcast a `Foreign` value to a specific Cedar type. Returns a clear error
/// naming the expected and actual types if the cast fails.
pub fn downcast<T: 'static>(v: &Value, expected: &str) -> ShenResult<Rc<T>> {
    match v.as_foreign() {
        Some(any) => any.downcast::<T>().map_err(|_| {
            ShenError::new(format!("expected {expected}, got Foreign of another type"))
        }),
        None => Err(ShenError::new(format!("expected {expected}, got {v:?}"))),
    }
}

// Named accessors — easier to read at the call site than `downcast::<Policy>(...)`.

pub fn as_policy(v: &Value) -> ShenResult<Rc<Policy>> {
    downcast::<Policy>(v, "cedar.policy")
}

pub fn as_policy_set(v: &Value) -> ShenResult<Rc<PolicySet>> {
    downcast::<PolicySet>(v, "cedar.policy-set")
}

pub fn as_schema(v: &Value) -> ShenResult<Rc<Schema>> {
    downcast::<Schema>(v, "cedar.schema")
}

pub fn as_entities(v: &Value) -> ShenResult<Rc<Entities>> {
    downcast::<Entities>(v, "cedar.entities")
}

pub fn as_request(v: &Value) -> ShenResult<Rc<Request>> {
    downcast::<Request>(v, "cedar.request")
}

#[allow(dead_code)]
pub fn as_authorizer(v: &Value) -> ShenResult<Rc<Authorizer>> {
    downcast::<Authorizer>(v, "cedar.authorizer")
}
