/*
mod.rs - Task System module

Exports Task, TaskStatus, and TaskStore for use by other modules.
*/

pub mod task;
pub mod store;
#[cfg(test)]
mod store_tests;

pub use task::{Task, TaskStatus};
pub use store::TaskStoreError;