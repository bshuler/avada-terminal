//! A pane's operating-model role: `manager`, `orchestrator` or `builder`.
//!
//! The role rides in the pane's string-to-string `meta` map under [`META_KEY`] (so `/state` and
//! `ctl info` can report it) and is exported to the pane's process as [`ENV_VAR`] (`AVADA_ROLE`),
//! which is what the operating-model rule injected into AGENTS.md keys off. A pane with no role
//! has NO `AVADA_ROLE` at all, including when Avada itself was launched from a role pane
//! (`session::spawn::build_env` scrubs the inherited value).

use serde_json::{json, Value};

/// Key in a pane's `meta` map.
pub const META_KEY: &str = "role";
/// Environment variable the pane's process sees.
pub const ENV_VAR: &str = "AVADA_ROLE";
/// The accepted values, exactly as typed (lowercase).
pub const ROLES: &[&str] = &["manager", "orchestrator", "builder"];

/// `Ok(role)` for a valid role, else a message naming the accepted values.
pub fn validate(role: &str) -> Result<&str, String> {
    if ROLES.contains(&role) {
        Ok(role)
    } else {
        Err(format!(
            "invalid role {role:?}: must be one of {}",
            ROLES.join("|")
        ))
    }
}

/// For a `newPane` spec: validate `meta.role` (when present and non-empty) and mirror it into
/// `env.AVADA_ROLE`. An empty `meta.role` is dropped (unset = absent). A caller-supplied
/// `env.AVADA_ROLE` with no `meta.role` is also rejected: the role must be stored with the pane,
/// never exist only in the process env. A non-string `meta.role` is an error.
pub fn apply_to_spec(spec: &mut Value) -> Result<(), String> {
    let Some(obj) = spec.as_object_mut() else {
        return Ok(());
    };
    let role = match obj.get("meta").and_then(|m| m.get(META_KEY)) {
        None => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::String(s)) => Some(validate(s)?.to_string()),
        Some(_) => return Err("role must be a string".to_string()),
    };
    let env_has_role = obj
        .get("env")
        .and_then(|e| e.get(ENV_VAR))
        .is_some();
    let Some(role) = role else {
        if let Some(meta) = obj.get_mut("meta").and_then(Value::as_object_mut) {
            meta.remove(META_KEY);
        }
        if env_has_role {
            return Err(format!(
                "env.{ENV_VAR} is set by the host from the pane's role; pass the role (manager|orchestrator|builder), not the variable"
            ));
        }
        return Ok(());
    };
    let env = obj.entry("env").or_insert_with(|| json!({}));
    let Some(env) = env.as_object_mut() else {
        return Err("env must be an object".to_string());
    };
    env.insert(ENV_VAR.to_string(), Value::String(role));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_exactly_the_three_roles() {
        for r in ["manager", "orchestrator", "builder"] {
            assert_eq!(validate(r), Ok(r));
        }
        for bad in ["Manager", "agent", "", " builder", "builder "] {
            assert!(validate(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn a_role_is_mirrored_into_the_spawn_env_and_kept_in_meta() {
        let mut spec = json!({ "command": "claude", "meta": { "role": "builder", "origin.why": "x" } });
        apply_to_spec(&mut spec).unwrap();
        assert_eq!(spec["env"]["AVADA_ROLE"], "builder");
        assert_eq!(spec["meta"]["role"], "builder");
        assert_eq!(spec["meta"]["origin.why"], "x");
    }

    #[test]
    fn existing_env_entries_are_kept() {
        let mut spec = json!({ "env": { "FOO": "1" }, "meta": { "role": "manager" } });
        apply_to_spec(&mut spec).unwrap();
        assert_eq!(spec["env"]["FOO"], "1");
        assert_eq!(spec["env"]["AVADA_ROLE"], "manager");
    }

    #[test]
    fn no_role_means_no_variable() {
        let mut spec = json!({ "command": "claude", "meta": { "origin.why": "x" } });
        apply_to_spec(&mut spec).unwrap();
        assert!(spec.get("env").is_none());
        let mut empty = json!({ "meta": { "role": "" } });
        apply_to_spec(&mut empty).unwrap();
        assert!(empty.get("env").is_none());
        assert!(empty["meta"].get("role").is_none());
        let mut bare = json!({});
        apply_to_spec(&mut bare).unwrap();
        assert_eq!(bare, json!({}));
    }

    #[test]
    fn a_bad_role_or_a_hand_set_variable_is_refused() {
        let mut bad = json!({ "meta": { "role": "boss" } });
        let e = apply_to_spec(&mut bad).unwrap_err();
        assert!(e.contains("manager|orchestrator|builder"), "{e}");
        let mut num = json!({ "meta": { "role": 3 } });
        assert!(apply_to_spec(&mut num).is_err());
        let mut raw = json!({ "env": { "AVADA_ROLE": "builder" } });
        assert!(apply_to_spec(&mut raw).is_err());
    }
}
