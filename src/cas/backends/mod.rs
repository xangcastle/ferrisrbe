//! CAS Backend Implementations

pub mod disk;
pub mod grpc;
pub mod http_proxy;

pub use disk::DiskBackend;
pub use grpc::GrpcCasBackend;
#[allow(unused_imports)]
pub use http_proxy::HttpProxyBackend;
