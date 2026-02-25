

use tonic::{Request, Response, Status, body::BoxBody};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::{Layer, Service};
use tracing::{info, warn};
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct LoggingLayer;

impl<S> Layer<S> for LoggingLayer {
    type Service = LoggingMiddleware<S>;

    fn layer(&self, service: S) -> Self::Service {
        LoggingMiddleware { inner: service }
    }
}

#[derive(Clone, Debug)]
pub struct LoggingMiddleware<S> {
    inner: S,
}

impl<S> Service<http::Request<BoxBody>> for LoggingMiddleware<S>
where
    S: Service<http::Request<BoxBody>, Response = http::Response<BoxBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<BoxBody>) -> Self::Future {
        let start = Instant::now();
        let method = req.uri().path().to_string();
        let inner = self.inner.clone();

        info!("📥 gRPC Request: {}", method);

        let future = async move {
            let mut inner = inner;
            let result = inner.call(req).await;
            let elapsed = start.elapsed();

            match &result {
                Ok(_) => info!("📤 gRPC Response: {} - {:?}", method, elapsed),
                Err(_) => warn!("❌ gRPC Error: {} - {:?}", method, elapsed),
            }

            result
        };

        Box::pin(future)
    }
}

pub fn with_logging<S>(service: S) -> LoggingMiddleware<S> {
    LoggingMiddleware { inner: service }
}
