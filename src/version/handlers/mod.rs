pub mod v7_handler;
pub mod v8_handler;
pub mod v9_handler;

pub use v7_handler::Bazel7Handler;
pub use v8_handler::Bazel8Handler;
pub use v9_handler::Bazel9Handler;

use crate::version::BazelVersion;

pub use crate::proto::build::bazel::semver::SemVer;

pub fn to_proto_semver(_version: BazelVersion) -> SemVer {
    reapi_v2_0_semver()
}

pub fn reapi_v2_0_semver() -> SemVer {
    SemVer {
        major: 2,
        minor: 0,
        patch: 0,
        prerelease: String::new(),
    }
}

pub fn reapi_v2_3_semver() -> SemVer {
    SemVer {
        major: 2,
        minor: 3,
        patch: 0,
        prerelease: String::new(),
    }
}

pub fn reapi_v2_4_semver() -> SemVer {
    SemVer {
        major: 2,
        minor: 4,
        patch: 0,
        prerelease: String::new(),
    }
}
