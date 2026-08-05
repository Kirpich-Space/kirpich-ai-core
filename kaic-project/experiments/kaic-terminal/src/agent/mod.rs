//! Agent Layer: Planner → (gate) → Coder (not implemented) → Reviewer (not implemented).

mod planner;
mod run_pass;

#[cfg(test)]
mod smoke;

// Re-exported for upcoming REPL wiring; unused by main until then.
#[allow(unused_imports)]
pub use planner::plan;
#[allow(unused_imports)]
pub use run_pass::{run_pass, run_pass_from_parsed};
