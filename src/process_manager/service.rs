//! Module for data representation of the service
//!
//! Singly handles (de)serialization of the service data to/from the nix type

use serde::{Deserialize, Serialize};

mod config_data;
mod process;

pub use config_data::{ConfigData, ConfigDataMap};
pub use process::{ArgV, Process};

/// Service type, similar to systemd's Type=.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceType {
    /// Service runs continuously, expected to stay running (default).
    #[default]
    Simple,
    /// Service runs once and exits. Considered "started" on successful exit.
    Oneshot,
    /// Service sends READY=1 via sd_notify when ready (not implemented, yet).
    Notify,
}

/// Service Data Struct
///
/// Rust based mirror of the services as defined in the [NixOS Modular Services
/// Modules](https://github.com/NixOS/nixpkgs/blob/a338deb8a1d11ead60c3d20b03f466b745514c38/lib/services/service.nix).
#[derive(Debug, Serialize, Deserialize)]
pub struct Service {
    /// Configuration files for the service
    #[serde(rename = "configData")]
    pub config_data: ConfigDataMap,

    /// Process configuration
    pub process: Process,

    /// Optional binary to run before each start of this service
    #[serde(rename = "preStart")]
    pub pre_start: Option<String>,

    /// Optional binary to run after service starts. Runs once after spawn.
    /// If it fails, the service fails. If it succeeds, the service is marked ready.
    #[serde(rename = "postStart")]
    pub post_start: Option<String>,

    /// Service type (defaults to Simple)
    #[serde(default, rename = "type")]
    pub service_type: ServiceType,
}
