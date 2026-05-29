//! Cedar primitives exposed to Shen.
//!
//! API surface mirrors the public `cedar-policy` crate, but with names
//! prefixed `cedar.` so they fit Shen conventions. Each primitive maps to
//! exactly one Cedar SDK call; complex flows like `Authorizer::is_authorized`
//! get a single Shen entry point that returns a structured Shen value.
//!
//! Cedar values flow through Shen as `Value::Foreign(Rc<dyn Any>)`. The
//! `types` module gives named downcasters with clear error messages.

use std::rc::Rc;
use std::str::FromStr;

use cedar_policy::{
    Authorizer, Context, Entities, EntityId, EntityTypeName, EntityUid, Policy, PolicyId,
    PolicySet, Request, Schema, ValidationMode, Validator,
};

use crate::cedar::types::{as_entities, as_policy, as_policy_set, as_request, as_schema, wrap};
use crate::error::{ShenError, ShenResult};
use crate::interp::eval::Interp;
use crate::value::Value;

/// Names + arities of every `cedar.*` primitive. Used both for
/// registration and for publishing `shen.lambda-form` entries onto the
/// kernel's `*property-vector*` post-boot.
pub const CEDAR_PRIMITIVES: &[(&str, usize)] = &[
    ("cedar.parse-policy", 1),
    ("cedar.parse-policy-set", 1),
    ("cedar.parse-schema", 1),
    ("cedar.parse-entities", 1),
    ("cedar.make-entity-uid", 2),
    ("cedar.entity-uid->string", 1),
    ("cedar.make-request", 4),
    ("cedar.is-authorized", 3),
    ("cedar.is-authorized-detailed", 3),
    ("cedar.validate", 2),
    ("cedar.policy->string", 1),
    ("cedar.policy-set->string", 1),
    ("cedar.empty-entities", 0),
    ("cedar.empty-policy-set", 0),
    ("cedar.policy-set-add", 2),
];

/// Register every `cedar.*` primitive. Called from `boot.rs` after
/// kernel boot so the `cedar.*` names are available at the REPL.
pub fn register_all(interp: &mut Interp) {
    interp.register_native("cedar.parse-policy", 1, |_, args| match &args[0] {
        Value::Str(s) => {
            let p = Policy::from_str(s)
                .map_err(|e| ShenError::new(format!("cedar.parse-policy: {e}")))?;
            Ok(wrap(p))
        }
        other => Err(ShenError::new(format!(
            "cedar.parse-policy: expected string, got {other:?}"
        ))),
    });

    interp.register_native("cedar.parse-policy-set", 1, |_, args| match &args[0] {
        Value::Str(s) => {
            let ps = PolicySet::from_str(s)
                .map_err(|e| ShenError::new(format!("cedar.parse-policy-set: {e}")))?;
            Ok(wrap(ps))
        }
        other => Err(ShenError::new(format!(
            "cedar.parse-policy-set: expected string, got {other:?}"
        ))),
    });

    interp.register_native("cedar.parse-schema", 1, |_, args| match &args[0] {
        Value::Str(s) => {
            // Cedar 4.x exposes both human-readable (cedar schema) and
            // JSON schema parsers. Default to human-readable since
            // that's the canonical authoring format.
            let (schema, _warnings) = Schema::from_cedarschema_str(s)
                .map_err(|e| ShenError::new(format!("cedar.parse-schema: {e}")))?;
            Ok(wrap(schema))
        }
        other => Err(ShenError::new(format!(
            "cedar.parse-schema: expected string, got {other:?}"
        ))),
    });

    interp.register_native("cedar.parse-entities", 1, |_, args| match &args[0] {
        Value::Str(s) => {
            let ents = Entities::from_json_str(s, None)
                .map_err(|e| ShenError::new(format!("cedar.parse-entities: {e}")))?;
            Ok(wrap(ents))
        }
        other => Err(ShenError::new(format!(
            "cedar.parse-entities: expected string, got {other:?}"
        ))),
    });

    interp.register_native("cedar.make-entity-uid", 2, |_, args| {
        match (&args[0], &args[1]) {
            (Value::Str(type_name), Value::Str(id)) => {
                let etype = EntityTypeName::from_str(type_name)
                    .map_err(|e| ShenError::new(format!("cedar.make-entity-uid: type {e}")))?;
                let eid = EntityId::from_str(id)
                    .map_err(|e| ShenError::new(format!("cedar.make-entity-uid: id {e}")))?;
                Ok(wrap(EntityUid::from_type_name_and_id(etype, eid)))
            }
            (a, b) => Err(ShenError::new(format!(
                "cedar.make-entity-uid: expected two strings, got {a:?}, {b:?}"
            ))),
        }
    });

    interp.register_native("cedar.entity-uid->string", 1, |_, args| match &args[0] {
        Value::Foreign(_) => {
            let uid = crate::cedar::types::downcast::<EntityUid>(&args[0], "cedar.entity-uid")?;
            Ok(Value::str(Rc::from(uid.to_string())))
        }
        other => Err(ShenError::new(format!(
            "cedar.entity-uid->string: not a cedar.entity-uid: {other:?}"
        ))),
    });

    interp.register_native("cedar.make-request", 4, |_, args| {
        let principal = coerce_entity_uid(&args[0], "cedar.make-request principal")?;
        let action = coerce_entity_uid(&args[1], "cedar.make-request action")?;
        let resource = coerce_entity_uid(&args[2], "cedar.make-request resource")?;
        let context = match &args[3] {
            Value::Str(s) if s.trim().is_empty() => Context::empty(),
            Value::Str(s) => Context::from_json_str(s, None)
                .map_err(|e| ShenError::new(format!("cedar.make-request: context: {e}")))?,
            Value::Nil => Context::empty(),
            other => {
                return Err(ShenError::new(format!(
                    "cedar.make-request: context must be a JSON string or (), got {other:?}"
                )))
            }
        };
        let req = Request::new(principal, action, resource, context, None)
            .map_err(|e| ShenError::new(format!("cedar.make-request: {e}")))?;
        Ok(wrap(req))
    });

    interp.register_native("cedar.is-authorized", 3, |interp, args| {
        let policy_set = as_policy_set(&args[0])?;
        let entities = as_entities(&args[1])?;
        let request = as_request(&args[2])?;
        let authorizer = Authorizer::new();
        let response = authorizer.is_authorized(&request, &policy_set, &entities);
        let decision = match response.decision() {
            cedar_policy::Decision::Allow => "Allow",
            cedar_policy::Decision::Deny => "Deny",
        };
        let decision_sym = interp.intern(decision);
        Ok(Value::sym(decision_sym))
    });

    interp.register_native("cedar.is-authorized-detailed", 3, |interp, args| {
        let policy_set = as_policy_set(&args[0])?;
        let entities = as_entities(&args[1])?;
        let request = as_request(&args[2])?;
        let authorizer = Authorizer::new();
        let response = authorizer.is_authorized(&request, &policy_set, &entities);

        let decision = match response.decision() {
            cedar_policy::Decision::Allow => "Allow",
            cedar_policy::Decision::Deny => "Deny",
        };
        let decision_sym = interp.intern(decision);

        let diagnostics = response.diagnostics();
        let reasons: Vec<Value> = diagnostics
            .reason()
            .map(|pid| Value::str(Rc::from(pid.to_string())))
            .collect();
        let errors: Vec<Value> = diagnostics
            .errors()
            .map(|err| Value::str(Rc::from(err.to_string())))
            .collect();

        // Return as a 3-element list: (DECISION REASONS ERRORS).
        Ok(Value::list([
            Value::sym(decision_sym),
            Value::list(reasons),
            Value::list(errors),
        ]))
    });

    interp.register_native("cedar.validate", 2, |_, args| {
        let policy_set = as_policy_set(&args[0])?;
        let schema = as_schema(&args[1])?;
        let validator = Validator::new((*schema).clone());
        let result = validator.validate(&policy_set, ValidationMode::default());
        let errors: Vec<Value> = result
            .validation_errors()
            .map(|err| Value::str(Rc::from(err.to_string())))
            .collect();
        Ok(Value::list(errors))
    });

    interp.register_native("cedar.policy->string", 1, |_, args| {
        let p = as_policy(&args[0])?;
        Ok(Value::str(Rc::from(p.to_string())))
    });

    interp.register_native("cedar.policy-set->string", 1, |_, args| {
        let ps = as_policy_set(&args[0])?;
        Ok(Value::str(Rc::from(ps.to_string())))
    });

    interp.register_native("cedar.empty-entities", 0, |_, _| {
        Ok(wrap(Entities::empty()))
    });

    interp.register_native("cedar.empty-policy-set", 0, |_, _| {
        Ok(wrap(PolicySet::new()))
    });

    interp.register_native("cedar.policy-set-add", 2, |_, args| {
        let mut ps: PolicySet = (*as_policy_set(&args[0])?).clone();
        let policy = as_policy(&args[1])?;
        // Cedar requires policies in a set to have unique IDs. If the
        // input policy has the default id `policy0`, re-id it with a
        // count-based suffix so callers can chain `add` without thinking
        // about ids.
        let next = format!("policy{}", ps.policies().count());
        let pid = PolicyId::new(next);
        let renamed = policy.new_id(pid);
        ps.add(renamed)
            .map_err(|e| ShenError::new(format!("cedar.policy-set-add: {e}")))?;
        Ok(wrap(ps))
    });
}

/// Accept either an already-constructed `cedar.entity-uid` foreign value
/// or a raw Cedar entity-uid string like `User::"alice"`.
fn coerce_entity_uid(v: &Value, what: &str) -> ShenResult<EntityUid> {
    match v {
        Value::Str(s) => EntityUid::from_str(s).map_err(|e| ShenError::new(format!("{what}: {e}"))),
        Value::Foreign(_) => {
            let uid = crate::cedar::types::downcast::<EntityUid>(v, what)?;
            Ok((*uid).clone())
        }
        other => Err(ShenError::new(format!(
            "{what}: expected entity-uid handle or string, got {other:?}"
        ))),
    }
}
