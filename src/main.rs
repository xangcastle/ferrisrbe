

use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

mod cache;
mod cas;
mod execution;
mod server;
mod types;
mod version;
mod worker;

#[allow(clippy::doc_lazy_continuation)]
pub mod proto {
    pub mod google {
        pub mod bytestream {
            include!(concat!(env!("OUT_DIR"), "/google.bytestream.rs"));
        }
        pub mod longrunning {
            include!(concat!(env!("OUT_DIR"), "/google.longrunning.rs"));
        }
        pub mod rpc {
            include!(concat!(env!("OUT_DIR"), "/google.rpc.rs"));
        }
    }
    
    pub mod build {
        pub mod bazel {
            pub mod semver {
                include!(concat!(env!("OUT_DIR"), "/build.bazel.semver.rs"));
            }
            
            pub mod remote {
                pub mod execution {
                    pub mod v2 {
                        include!(concat!(env!("OUT_DIR"), "/build.bazel.remote.execution.v2.rs"));
                    }
                }
            }
        }
    }
    
    pub mod ferris {
        pub mod rbe {
            pub mod worker {
                tonic::include_proto!("ferris.rbe.worker");
            }
        }
    }
}

use server::{RbeServer, RbeServerConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    crate::types::init_global_base_instant();

    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("╔══════════════════════════════════════════════════════════════╗");
    info!("║              RBE Server - Remote Build Execution              ║");
    info!("╠══════════════════════════════════════════════════════════════╣");
    info!("║ ✓ ByteStream Service (CAS upload/download)                    ║");
    info!("║ ✓ ActionCache Service (L1: DashMap 64 shards)                 ║");
    info!("║ ✓ CAS Service (FindMissing, BatchRead, BatchUpdate)           ║");
    info!("║ ✓ Execution Service (MultiLevelScheduler + StateMachine)      ║");
    info!("║ ✓ Capabilities Service (REAPI v2.3)                           ║");
    info!("╚══════════════════════════════════════════════════════════════╝");

    let port = std::env::var("RBE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(9092);

    let bind_address = std::env::var("RBE_BIND_ADDRESS").unwrap_or_else(|_| "0.0.0.0".to_string());

    let config = RbeServerConfig {
        bind_address,
        port,
        ..Default::default()
    };

    let server = RbeServer::new(config).await?;
    server.run().await?;

    Ok(())
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use cache::L1ActionCache;
    use execution::{ExecutionStage, ExecutionStateMachine, MultiLevelScheduler};
    use types::DigestInfo;

    #[test]
    fn test_full_stack_compilation() {

        let _digest = DigestInfo::new("test", 1024);
        let _cache = L1ActionCache::default();
        let _scheduler = MultiLevelScheduler::new();

        let _ = ExecutionStage::CacheCheck;
        let _ = ExecutionStage::Queued;
        let _ = ExecutionStage::Assigned;
        let _ = ExecutionStage::Downloading;
        let _ = ExecutionStage::Executing;
        let _ = ExecutionStage::Uploading;
        let _ = ExecutionStage::Completed;
        let _ = ExecutionStage::Failed;

        info!("All modules compile correctly!");
    }

    #[tokio::test]
    async fn test_state_machine_full_flow() {
        let digest = DigestInfo::new("test_action", 2048);
        let op_id = execution::state_machine::OperationId::generate();
        let sm = ExecutionStateMachine::new(op_id, digest);

        sm.transition_to(ExecutionStage::Queued).await.unwrap();
        sm.transition_to(ExecutionStage::Assigned).await.unwrap();
        sm.transition_to(ExecutionStage::Downloading).await.unwrap();
        sm.transition_to(ExecutionStage::Executing).await.unwrap();
        sm.transition_to(ExecutionStage::Uploading).await.unwrap();
        sm.transition_to(ExecutionStage::Completed).await.unwrap();

        assert!(sm.is_terminal().await);
    }

    #[test]
    fn test_dashmap_sharding() {
        use dashmap::DashMap;

        let map: DashMap<DigestInfo, String, ahash::RandomState> =
            DashMap::with_capacity_and_hasher_and_shard_amount(1000, ahash::RandomState::new(), 64);

        map.insert(DigestInfo::new("test", 1024), "value".to_string());
        assert_eq!(map.len(), 1);
    }
}
