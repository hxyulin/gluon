//! Structured DSL type information built from Rhai's metadata API.
//!
//! [`DslSchema`] replaces the flat signature strings from
//! [`super::dsl_signatures`] with typed data that the LSP and other
//! tooling can consume without parsing. The schema is built by calling
//! `gen_fn_metadata_to_json(false)` on a freshly-registered engine and
//! classifying each function entry as either a top-level constructor or
//! a builder method based on the first parameter type (`&mut Builder`
//! indicates a method).

use std::collections::BTreeMap;

/// Complete schema of the Gluon DSL surface.
#[derive(Debug, Clone)]
pub struct DslSchema {
    /// Top-level constructor functions (e.g. `project`, `target`, `group`).
    pub constructors: BTreeMap<String, Constructor>,
    /// Builder types and their chainable methods.
    pub builder_types: BTreeMap<String, BuilderType>,
    /// Global constants registered via `engine.register_global_module`
    /// (e.g. `LIB`, `BIN`, `PROC_MACRO`, `STATICLIB`).
    pub global_constants: BTreeMap<String, i64>,
}

/// A top-level DSL constructor function, potentially with multiple overloads.
#[derive(Debug, Clone)]
pub struct Constructor {
    pub name: String,
    pub overloads: Vec<FnSig>,
    pub returns: ReturnType,
}

/// A builder type with its chainable methods.
#[derive(Debug, Clone)]
pub struct BuilderType {
    pub name: String,
    pub methods: BTreeMap<String, MethodInfo>,
}

/// A method on a builder type.
#[derive(Debug, Clone)]
pub struct MethodInfo {
    pub name: String,
    pub overloads: Vec<FnSig>,
    pub returns: ReturnType,
}

/// A single function signature (one overload).
#[derive(Debug, Clone)]
pub struct FnSig {
    pub params: Vec<ParamInfo>,
    pub display: String,
}

/// A parameter in a function signature.
#[derive(Debug, Clone)]
pub struct ParamInfo {
    pub name: String,
    pub type_hint: String,
}

/// Classification of what a function/method returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnType {
    /// Method returns the same builder type it was called on (chainable).
    SelfType,
    /// Returns a different builder type (e.g. `group.add()` → `CrateBuilder`).
    Builder(String),
    /// Returns nothing meaningful (`()` or empty).
    Void,
}

/// Extract the last `::` segment from a fully-qualified Rust type name.
///
/// `"gluon_core::engine::builders::model::GroupBuilder"` → `"GroupBuilder"`.
/// A name without `::` is returned as-is.
pub fn short_type_name(full: &str) -> &str {
    full.rsplit("::").next().unwrap_or(full)
}

/// Classify a return type string from Rhai metadata.
///
/// - Empty or `"()"` → [`ReturnType::Void`]
/// - Matches `this_type` → [`ReturnType::SelfType`]
/// - Otherwise → [`ReturnType::Builder`] with the short name
fn classify_return(return_type: &str, this_type: Option<&str>) -> ReturnType {
    let trimmed = return_type.trim();
    if trimmed.is_empty() || trimmed == "()" {
        return ReturnType::Void;
    }
    if let Some(this) = this_type {
        if trimmed == this {
            return ReturnType::SelfType;
        }
    }
    ReturnType::Builder(short_type_name(trimmed).to_string())
}

impl DslSchema {
    /// Build a [`DslSchema`] from a Rhai engine that has all builders
    /// registered.
    ///
    /// This parses the JSON produced by `gen_fn_metadata_to_json(false)`
    /// (excluding standard-library functions) and classifies each entry.
    ///
    /// Rhai's `thisType` metadata field is only populated for
    /// script-defined functions, not for native `register_fn` calls.
    /// Instead, we detect methods by checking whether the first
    /// parameter type starts with `&mut ` — the `builder_method!` macro
    /// always registers `|builder: &mut SomeBuilder, ...| -> SomeBuilder`.
    pub fn from_engine(engine: &rhai::Engine) -> Self {
        let json_str = engine
            .gen_fn_metadata_to_json(false)
            .expect("Rhai metadata serialization should not fail");

        let root: serde_json::Value =
            serde_json::from_str(&json_str).expect("Rhai metadata JSON should be valid");

        let empty = Vec::new();
        let functions = root["functions"].as_array().unwrap_or(&empty);

        let mut constructors: BTreeMap<String, Constructor> = BTreeMap::new();
        let mut builder_types: BTreeMap<String, BuilderType> = BTreeMap::new();

        for func in functions {
            let name = func["name"].as_str().unwrap_or_default().to_string();
            let return_type = func["returnType"].as_str().unwrap_or_default();
            let signature = func["signature"].as_str().unwrap_or_default().to_string();

            let raw_params = func["params"].as_array();

            // Detect methods: the first parameter type is `&mut <FullPath>`
            // for builder methods registered via `register_fn`.
            let receiver_type = raw_params
                .and_then(|arr| arr.first())
                .and_then(|p| p["type"].as_str())
                .and_then(|t| t.strip_prefix("&mut "));

            // Build the user-visible param list, skipping the `&mut self`
            // receiver for methods.
            let param_iter = raw_params.map(|arr| arr.as_slice()).unwrap_or_default();
            let skip = if receiver_type.is_some() { 1 } else { 0 };
            let params: Vec<ParamInfo> = param_iter
                .iter()
                .skip(skip)
                .map(|p| ParamInfo {
                    name: p["name"].as_str().unwrap_or("_").to_string(),
                    type_hint: p["type"].as_str().unwrap_or("").to_string(),
                })
                .collect();

            let sig = FnSig {
                params,
                display: signature,
            };

            if let Some(receiver) = receiver_type {
                // This is a method on a builder type.
                let short = short_type_name(receiver).to_string();
                let returns = classify_return(return_type, Some(receiver));

                let builder = builder_types
                    .entry(short.clone())
                    .or_insert_with(|| BuilderType {
                        name: short,
                        methods: BTreeMap::new(),
                    });

                builder
                    .methods
                    .entry(name.clone())
                    .and_modify(|m| m.overloads.push(sig.clone()))
                    .or_insert_with(|| MethodInfo {
                        name,
                        overloads: vec![sig],
                        returns,
                    });
            } else {
                // Top-level constructor (no receiver).
                let returns = classify_return(return_type, None);

                constructors
                    .entry(name.clone())
                    .and_modify(|c| c.overloads.push(sig.clone()))
                    .or_insert_with(|| Constructor {
                        name,
                        overloads: vec![sig],
                        returns,
                    });
            }
        }

        // Hardcoded global constants — these are registered via a Rhai
        // module (builders/mod.rs:86-91) and don't appear in function
        // metadata.
        let mut global_constants = BTreeMap::new();
        global_constants.insert("LIB".to_string(), 0);
        global_constants.insert("BIN".to_string(), 1);
        global_constants.insert("PROC_MACRO".to_string(), 2);
        global_constants.insert("STATICLIB".to_string(), 3);

        DslSchema {
            constructors,
            builder_types,
            global_constants,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Create a fresh engine with all gluon builders registered.
    fn test_schema() -> DslSchema {
        let state = crate::engine::EngineState::new(PathBuf::from("<test>"));
        let mut engine = rhai::Engine::new();
        crate::engine::builders::register_all(&mut engine, &state);
        DslSchema::from_engine(&engine)
    }

    #[test]
    fn schema_includes_known_constructors() {
        let schema = test_schema();
        for name in &["project", "target", "group", "profile", "pipeline", "qemu"] {
            assert!(
                schema.constructors.contains_key(*name),
                "missing constructor: {name}"
            );
        }
    }

    #[test]
    fn schema_includes_known_builder_types() {
        let schema = test_schema();
        for name in &[
            "GroupBuilder",
            "QemuBuilder",
            "ProfileBuilder",
            "CrateBuilder",
        ] {
            assert!(
                schema.builder_types.contains_key(*name),
                "missing builder type: {name}"
            );
        }
    }

    #[test]
    fn group_builder_has_add_method() {
        let schema = test_schema();
        let group = &schema.builder_types["GroupBuilder"];
        let add = group
            .methods
            .get("add")
            .expect("GroupBuilder should have an `add` method");
        assert_eq!(add.returns, ReturnType::Builder("CrateBuilder".to_string()));
    }

    #[test]
    fn qemu_builder_methods_return_self() {
        let schema = test_schema();
        let qemu = &schema.builder_types["QemuBuilder"];
        let machine = qemu
            .methods
            .get("machine")
            .expect("QemuBuilder should have a `machine` method");
        assert_eq!(machine.returns, ReturnType::SelfType);
    }

    #[test]
    fn void_constructors_detected() {
        let schema = test_schema();
        let target = &schema.constructors["target"];
        assert_eq!(target.returns, ReturnType::Void);
    }

    #[test]
    fn global_constants_present() {
        let schema = test_schema();
        assert_eq!(schema.global_constants["LIB"], 0);
        assert_eq!(schema.global_constants["BIN"], 1);
        assert_eq!(schema.global_constants["PROC_MACRO"], 2);
        assert_eq!(schema.global_constants["STATICLIB"], 3);
    }

    #[test]
    fn short_type_name_extracts_last_segment() {
        assert_eq!(
            short_type_name("gluon_core::engine::builders::model::GroupBuilder"),
            "GroupBuilder"
        );
        assert_eq!(short_type_name("GroupBuilder"), "GroupBuilder");
        assert_eq!(short_type_name(""), "");
    }
}
