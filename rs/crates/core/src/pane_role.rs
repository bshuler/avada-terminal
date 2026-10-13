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

/// The canonical role in `meta` (a valid string), if any. A free-form `meta.role` that some
/// existing caller stores for its own purposes is NOT a role and yields `None`.
pub fn canonical(meta: Option<&std::collections::BTreeMap<String, String>>) -> Option<&str> {
    meta.and_then(|m| m.get(META_KEY))
        .map(String::as_str)
        .filter(|r| ROLES.contains(r))
}

/// `env` plus `AVADA_ROLE` when `meta` (or the origin bag, which carries the canonical role
/// across restarts, re-hosts and relaunches) names a canonical role. Used by every respawn path.
pub fn env_with_role(
    env: Option<crate::session::spawn::EnvMap>,
    meta: Option<&std::collections::BTreeMap<String, String>>,
) -> Option<crate::session::spawn::EnvMap> {
    match canonical(meta) {
        None => env,
        Some(r) => {
            let mut e = env.unwrap_or_default();
            e.insert(ENV_VAR.to_string(), r.to_string());
            Some(e)
        }
    }
}

/// For a `newPane` spec: when `meta.role` is one of [`ROLES`], mirror it into
/// `env.AVADA_ROLE`. Any other `meta.role` (free-form, empty, non-string) is left stored
/// exactly as sent, with no env and no error. A caller-supplied `env.AVADA_ROLE` without a
/// canonical `meta.role` is refused: the role must be stored with the pane, never exist only
/// in the process env.
pub fn apply_to_spec(spec: &mut Value) -> Result<(), String> {
    let Some(obj) = spec.as_object_mut() else {
        return Ok(());
    };
    let role = match obj.get("meta").and_then(|m| m.get(META_KEY)) {
        Some(Value::String(s)) if ROLES.contains(&s.as_str()) => Some(s.clone()),
        _ => None,
    };
    let Some(role) = role else {
        if obj.get("env").and_then(|e| e.get(ENV_VAR)).is_some() {
            return Err(format!(
                "env.{ENV_VAR} is set by the host from the pane's role; pass meta.role (manager|orchestrator|builder), not the variable"
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
    fn no_role_means_no_variable_and_free_form_roles_are_kept_without_env() {
        let mut spec = json!({ "command": "claude", "meta": { "origin.why": "x" } });
        apply_to_spec(&mut spec).unwrap();
        assert!(spec.get("env").is_none());
        for v in [json!("worker"), json!(""), json!("Builder"), json!(3)] {
            let mut s = json!({ "meta": { "role": v.clone() } });
            apply_to_spec(&mut s).unwrap();
            assert!(s.get("env").is_none(), "{v} must not export a role");
            assert_eq!(s["meta"]["role"], v, "{v} must stay stored");
        }
        let mut bare = json!({});
        apply_to_spec(&mut bare).unwrap();
        assert_eq!(bare, json!({}));
    }

    #[test]
    fn a_hand_set_variable_is_refused() {
        let mut raw = json!({ "env": { "AVADA_ROLE": "builder" } });
        assert!(apply_to_spec(&mut raw).is_err());
        let mut odd = json!({ "env": { "AVADA_ROLE": "x" }, "meta": { "role": "worker" } });
        assert!(apply_to_spec(&mut odd).is_err());
    }

    #[test]
    fn respawn_env_is_rederived_from_the_stored_role() {
        let m = |r: &str| std::collections::BTreeMap::from([("role".to_string(), r.to_string())]);
        let e = env_with_role(None, Some(&m("orchestrator"))).unwrap();
        assert_eq!(e.get("AVADA_ROLE").map(String::as_str), Some("orchestrator"));
        assert!(env_with_role(None, Some(&m("worker"))).is_none());
        assert!(env_with_role(None, None).is_none());
        let base = std::collections::HashMap::from([("A".to_string(), "1".to_string())]);
        let e = env_with_role(Some(base), Some(&m("builder"))).unwrap();
        assert_eq!(e.len(), 2);
    }

    #[test]
    fn canonical_reads_only_valid_roles() {
        let m = |r: &str| std::collections::BTreeMap::from([("role".to_string(), r.to_string())]);
        assert_eq!(canonical(Some(&m("builder"))), Some("builder"));
        assert_eq!(canonical(Some(&m("worker"))), None);
        assert_eq!(canonical(None), None);
    }
}
