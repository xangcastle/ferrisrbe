//! BES (Build Event Service) configuration.

use std::path::PathBuf;

/// Configuration for the BES backend.
#[derive(Debug, Clone)]
pub struct BesConfig {
    /// gRPC port for the PublishBuildEvent service.
    pub grpc_port: u16,
    /// HTTP port for the BES UI/API.
    pub ui_port: u16,
    /// Address to bind both gRPC and HTTP servers to.
    pub bind_address: String,
    /// Directory where invocation data is persisted.
    pub data_dir: PathBuf,
    /// Maximum retention time for build data, in days.
    pub max_retention_days: Option<u32>,
    /// Directory from which static UI files are served.
    pub static_dir: PathBuf,
}

impl BesConfig {
    /// Load configuration from environment variables.
    pub fn from_env() -> Self {
        let grpc_port = std::env::var("RBE_BES_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(9095);
        let ui_port = std::env::var("RBE_BES_UI_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(9096);
        let bind_address =
            std::env::var("RBE_BES_BIND_ADDRESS").unwrap_or_else(|_| "0.0.0.0".to_string());
        let data_dir = std::env::var("RBE_BES_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/data/bes"));
        let max_retention_days = std::env::var("RBE_BES_MAX_RETENTION_DAYS")
            .ok()
            .and_then(|s| s.parse().ok());
        let static_dir = std::env::var("RBE_BES_STATIC_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("ui/dist"));

        Self {
            grpc_port,
            ui_port,
            bind_address,
            data_dir,
            max_retention_days,
            static_dir,
        }
    }

    /// Directory for a specific invocation.
    pub fn invocation_dir(&self, invocation_id: &str) -> PathBuf {
        self.data_dir.join(invocation_id)
    }

    /// Path to the events JSONL file for an invocation.
    pub fn events_path(&self, invocation_id: &str) -> PathBuf {
        self.invocation_dir(invocation_id).join("events.jsonl")
    }

    /// Path to the summary JSON file for an invocation.
    pub fn summary_path(&self, invocation_id: &str) -> PathBuf {
        self.invocation_dir(invocation_id).join("summary.json")
    }

    /// Path to the target execution history JSONL file.
    pub fn targets_path(&self) -> PathBuf {
        self.data_dir.join("targets.jsonl")
    }

    /// Path to the test execution history JSONL file.
    pub fn tests_path(&self) -> PathBuf {
        self.data_dir.join("tests.jsonl")
    }
}

impl Default for BesConfig {
    fn default() -> Self {
        Self {
            grpc_port: 9095,
            ui_port: 9096,
            bind_address: "0.0.0.0".to_string(),
            data_dir: PathBuf::from("/data/bes"),
            max_retention_days: None,
            static_dir: PathBuf::from("ui/dist"),
        }
    }
}
