pub mod agents;
pub mod box_icons;
pub mod boxes;
pub mod budget;
mod cgroup;
pub mod client;
#[cfg(feature = "lua-config")]
pub mod config;
pub mod daemon;
pub mod plugins;

mod agent_socket;
mod dag_adapter;
mod dag_store;
mod ownership;
mod persistence;
mod process_tree;
pub mod reaper;
mod scheduler;
pub mod shell;
pub mod skill_install;
pub mod skills;
mod talk_adapter;
mod terminal;
pub mod theme;
mod wire;
pub mod workspace;
