//! pi-evals — eval harness for Pi.
//!
//! Port of `@earendil-works/pi-evals`: the harness surface
//! (`pi-harness.ts`), the eval-table/summary/reporter/artifacts machinery
//! (`vitest-evals/*`), and the smoke/extension eval scenario definitions.
//!
//! The Rust port runs eval definitions against the real `pi` binary (the
//! `pi-coding-agent` CLI) rather than embedding an in-process
//! `AgentSession`, so the scenario assertions exercise the shipped surface.

pub mod artifacts;
pub mod error;
pub mod harness;
pub mod harness_table;
pub mod reporter;
pub mod session_usage;
pub mod summary;

pub mod evals;
