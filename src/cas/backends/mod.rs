//! [`CasBackend`](crate::cas::CasBackend) implementations: local disk, a
//! remote REAPI CAS over gRPC, an object store (S3), and disk in front of a
//! durable remote tier.

pub mod disk;
pub mod grpc;
pub mod object;
pub mod tiered;

pub use disk::DiskBackend;
pub use grpc::GrpcCasBackend;
pub use object::ObjectStoreBackend;
pub use tiered::TieredBackend;
