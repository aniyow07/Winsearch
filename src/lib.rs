//! WinSearch engine: persistent file index, scanners, change tracking,
//! search and previews. The GUI lives in `main.rs` and only talks to this
//! library through the handles exposed here.

pub mod config;
pub mod db;
pub mod fileops;
pub mod indexer;
pub mod model;
pub mod platform;
pub mod preview;
pub mod scanner;
pub mod search;
pub mod util;
