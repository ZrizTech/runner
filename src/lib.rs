//! zriz-runner: runs inside the customer's environment, executes ops the
//! zriz cloud sends, holds every secret. Never imports from the cloud.

pub mod argshape;
pub mod config;
pub mod config_cli;
pub mod contract;
pub mod evidence;
pub mod exchange;
pub mod logfmt;
pub mod ops;
pub mod origin;
pub mod placeholder;
pub mod project;
pub mod readonly;
pub mod scrub;
