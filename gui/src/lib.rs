//! Headless proxy process and its local control protocol.
extern crate self as coport_gui;
pub mod daemon;
pub mod data_api;
pub mod data_client;
pub mod data_migration;
pub mod device_capabilities;
mod device_events;
pub mod devices;
pub mod logs;
pub mod proxy;
pub mod remote;
pub mod remote_forward;
pub mod settings;

pub mod tasks;
pub mod traffic;
pub mod traffic_identity;

#[cfg(test)]
mod test_support;
