//! Muesli core: domain model, pure logic (detection, session FSM, transcript, prompts),
//! storage, config, IPC protocol and the OpenRouter client.

pub mod config;
pub mod detect;
pub mod hooks;
pub mod import;
pub mod ipc;
pub mod model;
pub mod openrouter;
pub mod paths;
pub mod prompts;
pub mod session;
pub mod store;
pub mod transcript;
