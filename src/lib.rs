//! ods — OneDrive two-way code sync (Rust port of the PowerShell tool).
//!
//! The engine orchestrates `rclone bisync` + `git` per project. This library
//! holds the reusable pieces; `main.rs` is the CLI and (later) the tray/GUI.

pub mod actions;
pub mod config;
pub mod conflicts;
pub mod discovery;
pub mod engine;
pub mod events;
pub mod filter;
pub mod git;
pub mod gui;
pub mod icon;
pub mod jsonio;
pub mod paths;
pub mod prune;
pub mod run;
pub mod state;
