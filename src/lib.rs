//! FerrisRBE - Remote Build Execution Server Library
//!
//! This library provides the core components for the FerrisRBE system:
//! - CAS (Content Addressable Storage) backends
//! - Worker management and materialization
//! - Shared types and utilities

pub mod bes;
pub mod cache;
pub mod cas;
pub mod execution;
pub mod server;
pub mod types;
pub mod version;
pub mod worker;

#[allow(clippy::doc_lazy_continuation)]
pub mod proto {
    /// Build-specific protobuf definitions
    pub mod build {
        pub mod bazel {
            pub mod build_event_stream {
                include!(concat!(env!("OUT_DIR"), "/build_event_stream.rs"));
            }

            pub mod semver {
                include!(concat!(env!("OUT_DIR"), "/build.bazel.semver.rs"));
            }

            pub mod remote {
                pub mod execution {
                    pub mod v2 {
                        include!(concat!(
                            env!("OUT_DIR"),
                            "/build.bazel.remote.execution.v2.rs"
                        ));
                    }
                }
            }
        }
    }

    /// Google API protobuf definitions
    pub mod google {
        pub mod bytestream {
            include!(concat!(env!("OUT_DIR"), "/google.bytestream.rs"));
        }
        pub mod devtools {
            pub mod build {
                pub mod v1 {
                    include!(concat!(env!("OUT_DIR"), "/google.devtools.build.v1.rs"));
                }
            }
        }
        pub mod longrunning {
            include!(concat!(env!("OUT_DIR"), "/google.longrunning.rs"));
        }
        pub mod rpc {
            include!(concat!(env!("OUT_DIR"), "/google.rpc.rs"));
        }
    }

    pub mod ferris {
        pub mod rbe {
            pub mod worker {
                tonic::include_proto!("ferris.rbe.worker");
            }
        }
    }

    pub mod tools {
        pub mod protos {
            include!(concat!(env!("OUT_DIR"), "/tools.protos.rs"));
        }
    }
}
