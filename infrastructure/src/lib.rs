pub mod agents;
pub mod box_icons;
pub mod boxes;
pub mod budget;
pub mod client;
#[cfg(feature = "lua-config")]
pub mod config;
pub mod daemon;
pub mod plugins;

mod dag_adapter;
mod dag_store;
mod persistence;
pub mod shell;
pub mod skill_install;
pub mod skills;
mod talk_adapter;
mod terminal;
pub mod theme;
mod wire;
pub mod workspace;
