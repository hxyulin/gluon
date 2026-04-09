//! Builder registration for the Rhai engine.
//!
//! This module defines the `builder_method!` macro and the
//! [`register_all`] entry point used by [`super::evaluate_script`].

use super::EngineState;
use rhai::Engine;

pub(super) mod config;
pub(super) mod kconfig;
pub(super) mod model;
pub(super) mod pipeline;

/// Register a chainable builder method on the Rhai engine.
///
/// The macro wraps the repeated `state.model.borrow_mut()` + `builder.clone()`
/// boilerplate. The body sees `model: &mut BuildModel`, `state: &EngineState`,
/// and `pos: rhai::Position` (the **builder creation** position — where
/// `target("name")`, `group("name")`, etc. was originally called).
///
/// # Variants
///
/// - Zero extra args: `builder_method!(engine, "name", Ty, |state, model, pos| { ... })`
/// - One+ extra args: `builder_method!(engine, "name", Ty, |state, model, pos, arg: Type, ...| { ... })`
///
/// Every variant returns `builder.clone()` so the call is chainable.
macro_rules! builder_method {
    ($engine:expr, $name:expr, $builder_ty:ty,
     |$state:ident, $model:ident, $item_name:ident, $pos:ident| $body:block) => {
        $engine.register_fn(
            $name,
            |builder: &mut $builder_ty| -> $builder_ty {
                // Short-circuit: if the builder was returned from a
                // duplicate definition, chained methods must not mutate
                // the first definition's state.
                if builder.is_duplicate {
                    return builder.clone();
                }
                let $state = &builder.state;
                let $item_name: String = builder.name.clone();
                #[allow(unused_variables)]
                let $pos = builder.pos;
                {
                    #[allow(unused_mut)]
                    let mut $model = $state.model.borrow_mut();
                    let $item_name: &str = &$item_name;
                    $body
                }
                builder.clone()
            },
        );
    };
    ($engine:expr, $name:expr, $builder_ty:ty,
     |$state:ident, $model:ident, $item_name:ident, $pos:ident, $($arg:ident : $arg_ty:ty),+| $body:block) => {
        $engine.register_fn(
            $name,
            |builder: &mut $builder_ty, $($arg: $arg_ty),+| -> $builder_ty {
                // Short-circuit: if the builder was returned from a
                // duplicate definition, chained methods must not mutate
                // the first definition's state.
                if builder.is_duplicate {
                    return builder.clone();
                }
                let $state = &builder.state;
                let $item_name: String = builder.name.clone();
                #[allow(unused_variables)]
                let $pos = builder.pos;
                {
                    #[allow(unused_mut)]
                    let mut $model = $state.model.borrow_mut();
                    let $item_name: &str = &$item_name;
                    $body
                }
                builder.clone()
            },
        );
    };
}

pub(super) use builder_method;

/// Register every builder function exposed to gluon scripts.
pub(super) fn register_all(engine: &mut Engine, state: &EngineState) {
    // Expose crate-type constants as globals. Values are arbitrary i64
    // tags matched by [`model::crate_type_from_i64`].
    let mut module = rhai::Module::new();
    module.set_var("LIB", 0_i64);
    module.set_var("BIN", 1_i64);
    module.set_var("PROC_MACRO", 2_i64);
    module.set_var("STATICLIB", 3_i64);
    engine.register_global_module(rhai::Shared::new(module));

    model::register(engine, state);
    config::register(engine, state);
    kconfig::register(engine, state);
    pipeline::register(engine, state);
}
