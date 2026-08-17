/*
tools/mod.rs - Tool system module

This module contains the tool system infrastructure:
- trait_def: Core abstractions (Tool trait, ToolContext, PermissionCheck)
- (Future modules will contain individual tool implementations)
*/

pub mod trait_def;

pub use trait_def::{PermissionCheck, Tool, ToolContext};