//! `${OPTION_NAME}` expansion for resolved string/choice config values.
//!
//! Interpolation is performed once after the selects/depends fixed point
//! has converged, so the option map is final by the time we walk it. We
//! recurse on demand: if a referenced option's own value contains another
//! `${...}`, we resolve that one first. A `visiting` set guards against
//! cycles.

use gluon_model::ResolvedValue;
use std::collections::{BTreeMap, HashSet};

/// Walk `value` and replace every `${NAME}` reference with the resolved
/// value of the option named `NAME`. Returns the expanded string or
/// `Err(message)` describing the failure (unknown option, cycle, type
/// mismatch). Caller is responsible for translating the error string into
/// a [`crate::error::Diagnostic`].
pub(crate) fn interpolate(
    value: &str,
    options: &BTreeMap<String, ResolvedValue>,
    visiting: &mut HashSet<String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            // Find the matching '}'.
            let start = i + 2;
            let Some(rel_end) = value[start..].find('}') else {
                return Err(format!(
                    "unterminated '${{' in interpolated string {value:?}"
                ));
            };
            let end = start + rel_end;
            let name = &value[start..end];
            if name.is_empty() {
                return Err(format!("empty interpolation '${{}}' in {value:?}"));
            }
            if visiting.contains(name) {
                return Err(format!(
                    "interpolation cycle detected involving option '{name}'"
                ));
            }
            let Some(resolved) = options.get(name) else {
                return Err(format!("interpolation references unknown option '{name}'"));
            };
            let raw = match resolved {
                ResolvedValue::String(s) => s.clone(),
                ResolvedValue::Choice(s) => s.clone(),
                ResolvedValue::Bool(b) => b.to_string(),
                ResolvedValue::U32(n) => n.to_string(),
                ResolvedValue::U64(n) => n.to_string(),
                ResolvedValue::Tristate(t) => match t {
                    gluon_model::TristateVal::Yes => "y".to_string(),
                    gluon_model::TristateVal::Module => "m".to_string(),
                    gluon_model::TristateVal::No => "n".to_string(),
                },
                ResolvedValue::List(_) => {
                    return Err(format!(
                        "cannot interpolate list-typed option '{name}' into a string"
                    ));
                }
            };
            // Recurse only if the substituted text might itself contain `${...}`.
            let expanded = if raw.contains("${") {
                visiting.insert(name.to_string());
                let r = interpolate(&raw, options, visiting);
                visiting.remove(name);
                r?
            } else {
                raw
            };
            out.push_str(&expanded);
            i = end + 1;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> BTreeMap<String, ResolvedValue> {
        let mut m = BTreeMap::new();
        m.insert("NAME".into(), ResolvedValue::String("foo".into()));
        m.insert(
            "GREETING".into(),
            ResolvedValue::String("hello ${NAME}".into()),
        );
        m.insert("CYCLE_A".into(), ResolvedValue::String("${CYCLE_B}".into()));
        m.insert("CYCLE_B".into(), ResolvedValue::String("${CYCLE_A}".into()));
        m.insert("NUM".into(), ResolvedValue::U32(42));
        m
    }

    #[test]
    fn plain_passthrough() {
        let mut v = HashSet::new();
        assert_eq!(
            interpolate("hello world", &opts(), &mut v).unwrap(),
            "hello world"
        );
    }

    #[test]
    fn single_substitution() {
        let mut v = HashSet::new();
        assert_eq!(
            interpolate("name=${NAME}", &opts(), &mut v).unwrap(),
            "name=foo"
        );
    }

    #[test]
    fn nested_substitution() {
        let mut v = HashSet::new();
        assert_eq!(
            interpolate("${GREETING}!", &opts(), &mut v).unwrap(),
            "hello foo!"
        );
    }

    #[test]
    fn numeric_value_stringified() {
        let mut v = HashSet::new();
        assert_eq!(interpolate("${NUM}", &opts(), &mut v).unwrap(), "42");
    }

    #[test]
    fn unknown_option_errors() {
        let mut v = HashSet::new();
        let e = interpolate("${MISSING}", &opts(), &mut v).unwrap_err();
        assert!(e.contains("unknown option 'MISSING'"));
    }

    #[test]
    fn cycle_detected() {
        let mut v = HashSet::new();
        let e = interpolate("${CYCLE_A}", &opts(), &mut v).unwrap_err();
        assert!(e.contains("cycle"));
    }

    #[test]
    fn unterminated_braces_error() {
        let mut v = HashSet::new();
        let e = interpolate("${NAME", &opts(), &mut v).unwrap_err();
        assert!(e.contains("unterminated"));
    }
}
