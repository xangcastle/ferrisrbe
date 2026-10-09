//! Credentials a gRPC client presents to a FerrisRBE server.
//!
//! [`BearerAuth`] attaches `authorization: Bearer <token>` to every request of
//! the client it intercepts; without a token it passes requests through
//! unchanged, so authenticated and anonymous clients share one type.

use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::interceptor::InterceptedService;
use tonic::service::Interceptor;
use tonic::transport::Channel;
use tonic::{Request, Status};

/// A channel whose requests carry the credentials of a [`BearerAuth`].
pub type AuthChannel = InterceptedService<Channel, BearerAuth>;

/// Adds a bearer token to each request, when there is one.
#[derive(Clone, Debug, Default)]
pub struct BearerAuth {
    header: Option<MetadataValue<Ascii>>,
}

impl BearerAuth {
    /// Credentials for `token`; `None` or an empty token sends none.
    ///
    /// # Errors
    ///
    /// When the token contains characters not allowed in a header value.
    pub fn new(token: Option<&str>) -> Result<Self, String> {
        let header = token
            .filter(|token| !token.is_empty())
            .map(|token| {
                format!("Bearer {token}")
                    .parse()
                    .map_err(|_| "the token is not a valid header value".to_string())
            })
            .transpose()?;
        Ok(Self { header })
    }

    /// Credentials from the token in the environment variable `name`, if set.
    ///
    /// # Errors
    ///
    /// When the token is not a valid header value.
    pub fn from_env(name: &str) -> Result<Self, String> {
        Self::new(std::env::var(name).ok().as_deref())
    }

    /// Whether requests carry a token.
    pub fn is_authenticated(&self) -> bool {
        self.header.is_some()
    }
}

impl Interceptor for BearerAuth {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        if let Some(header) = &self.header {
            request
                .metadata_mut()
                .insert("authorization", header.clone());
        }
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authorization(auth: &mut BearerAuth) -> Option<String> {
        auth.call(Request::new(()))
            .unwrap()
            .metadata()
            .get("authorization")
            .map(|value| value.to_str().unwrap().to_string())
    }

    #[test]
    fn a_token_is_sent_as_a_bearer_credential() {
        let mut auth = BearerAuth::new(Some("secret")).unwrap();
        assert!(auth.is_authenticated());
        assert_eq!(authorization(&mut auth).as_deref(), Some("Bearer secret"));
    }

    #[test]
    fn without_a_token_requests_pass_unchanged() {
        for mut auth in [
            BearerAuth::default(),
            BearerAuth::new(None).unwrap(),
            BearerAuth::new(Some("")).unwrap(),
        ] {
            assert!(!auth.is_authenticated());
            assert_eq!(authorization(&mut auth), None);
        }
    }

    #[test]
    fn a_token_that_is_not_a_header_value_is_rejected() {
        assert!(BearerAuth::new(Some("line\nbreak")).is_err());
    }
}
