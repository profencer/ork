//! Id validation for [`crate::OrkAppBuilder`](super::OrkAppBuilder) (ADR [`0049`](../../docs/adrs/0049-orkapp-central-registry.md)).

use ork_common::error::OrkError;

fn cfg_err(msg: impl Into<String>) -> OrkError {
    OrkError::Configuration {
        message: msg.into(),
    }
}

/// Validates `id` matches `^[a-z0-9][a-z0-9-]{0,62}$` (Kong-safe path segment).
#[must_use]
pub fn is_valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    let len = id.len();
    if len > 63 {
        return false;
    }
    for c in chars {
        if c != '-' && !c.is_ascii_lowercase() && !c.is_ascii_digit() {
            return false;
        }
    }
    true
}

/// Returns `Ok(())` when [`is_valid_id`] holds, else [`OrkError::Configuration`].
pub fn validate_id(id: &str) -> Result<(), OrkError> {
    if is_valid_id(id) {
        Ok(())
    } else {
        Err(cfg_err(format!(
            "invalid component id `{id}`: expected ^[a-z0-9][a-z0-9-]{{0,62}}$",
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_rejected() {
        assert!(validate_id("").is_err());
    }

    #[test]
    fn leading_dash_rejected() {
        assert!(validate_id("-ab").is_err());
    }

    #[test]
    fn uppercase_rejected() {
        assert!(validate_id("Ab").is_err());
        assert!(validate_id("aB").is_err());
    }

    #[test]
    fn underscore_rejected() {
        assert!(validate_id("a_b").is_err());
    }

    #[test]
    fn max_len_boundary() {
        let ok62 = format!("{}{}", "a", "b".repeat(62));
        assert_eq!(ok62.len(), 63);
        assert!(validate_id(&ok62).is_ok());
        let bad64 = format!("{}{}", "a", "b".repeat(63));
        assert!(validate_id(&bad64).is_err());
    }

    #[test]
    fn single_char_ok() {
        assert!(validate_id("x").is_ok());
    }

    #[test]
    fn hyphens_inside_ok() {
        assert!(validate_id("weather-bot").is_ok());
    }
}
