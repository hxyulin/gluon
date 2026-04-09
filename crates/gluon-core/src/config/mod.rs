//! Config resolution: turns a validated [`gluon_model::BuildModel`] into a
//! flattened [`gluon_model::ResolvedConfig`] for a chosen profile.
//!
//! Resolution is the last "parse + validate + resolve" step before the
//! scheduler/compile path takes over. It walks the profile inheritance
//! chain, layers preset and external overrides on top of option defaults,
//! validates ranges/choices, iterates a selects/depends fixed point, and
//! interpolates `${OPTION}` references in string-typed values.

mod interpolate;
mod resolve;

pub use resolve::resolve;
