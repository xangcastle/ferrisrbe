//! Read-only PromQL queries against the cluster's Prometheus, which keeps the
//! history of every `rbe_*` series across server restarts; the server's own
//! `/metrics` only covers the current process.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Base URL of the Prometheus HTTP API, e.g.
/// `http://prometheus.default.svc.cluster.local:9090`. Unset disables the tool.
pub const ENV_PROMETHEUS_URL: &str = "RBE_PROMETHEUS_URL";

/// Series kept in one answer; the rest are counted in `truncated_series`.
pub const MAX_SERIES: usize = 50;

/// Points per series a range query may ask for.
pub const MAX_POINTS: u64 = 500;

/// Points per series a range query gets when no step is given.
const DEFAULT_POINTS: u64 = 120;

/// Smallest step: the scrape interval, below which points repeat.
const MIN_STEP: Duration = Duration::from_secs(15);

const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// A client of one Prometheus HTTP API.
#[derive(Clone)]
pub struct PrometheusClient {
    base: String,
    http: reqwest::Client,
}

impl PrometheusClient {
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(QUERY_TIMEOUT)
                .build()
                .unwrap_or_default(),
        }
    }

    /// The client for [`ENV_PROMETHEUS_URL`], if it is set.
    pub fn from_env() -> Option<Self> {
        std::env::var(ENV_PROMETHEUS_URL)
            .ok()
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty())
            .map(|url| Self::new(&url))
    }

    /// Evaluates `expr` now, or over the `range` ending now every `step`, and
    /// returns Prometheus' `data` with at most [`MAX_SERIES`] series.
    ///
    /// # Errors
    ///
    /// The reason Prometheus refused the query, or why it could not be asked.
    pub async fn query(
        &self,
        expr: &str,
        range: Option<Duration>,
        step: Option<Duration>,
    ) -> Result<Value, String> {
        let request = match range {
            None => self
                .http
                .get(format!("{}/api/v1/query", self.base))
                .query(&[("query", expr)]),
            Some(range) => {
                let step = range_step(range, step)?;
                let end = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|e| e.to_string())?
                    .as_secs();
                let start = end.saturating_sub(range.as_secs());
                self.http
                    .get(format!("{}/api/v1/query_range", self.base))
                    .query(&[
                        ("query", expr.to_string()),
                        ("start", start.to_string()),
                        ("end", end.to_string()),
                        ("step", step.as_secs().to_string()),
                    ])
            }
        };
        let body = request
            .send()
            .await
            .map_err(|e| format!("Prometheus is unreachable: {}", with_causes(&e)))?
            .text()
            .await
            .map_err(|e| format!("Prometheus answer could not be read: {e}"))?;
        let answer: Value = serde_json::from_str(&body)
            .map_err(|_| format!("Prometheus answered something that is not JSON: {body:.200}"))?;
        if answer["status"] != "success" {
            return Err(format!(
                "Prometheus refused the query ({}): {}",
                answer["errorType"].as_str().unwrap_or("error"),
                answer["error"].as_str().unwrap_or("no reason given")
            ));
        }
        Ok(truncated(answer["data"].clone()))
    }
}

/// `error` followed by each error that caused it, which reqwest's own
/// message leaves out (a refused connection, a DNS failure).
fn with_causes(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut cause = error.source();
    while let Some(inner) = cause {
        message.push_str(": ");
        message.push_str(&inner.to_string());
        cause = inner.source();
    }
    message
}

/// The step of a range query: `step` when given, else `range` over
/// [`DEFAULT_POINTS`]; never under [`MIN_STEP`].
///
/// # Errors
///
/// When the step would give more than [`MAX_POINTS`] points.
fn range_step(range: Duration, step: Option<Duration>) -> Result<Duration, String> {
    let step = step.unwrap_or(range / DEFAULT_POINTS as u32).max(MIN_STEP);
    let points = range.as_secs() / step.as_secs().max(1);
    if points > MAX_POINTS {
        return Err(format!(
            "{points} points per series is too many; use a step of at least {}s",
            range.as_secs().div_ceil(MAX_POINTS)
        ));
    }
    Ok(step)
}

/// `data` with its `result` cut to [`MAX_SERIES`] series, and the number of
/// series left out in `truncated_series` when any were.
fn truncated(mut data: Value) -> Value {
    if let Some(result) = data["result"].as_array_mut() {
        if result.len() > MAX_SERIES {
            let dropped = result.len() - MAX_SERIES;
            result.truncate(MAX_SERIES);
            data["truncated_series"] = dropped.into();
        }
    }
    data
}

/// A Prometheus-style duration: a number and one of `s`, `m`, `h`, `d`, `w`.
///
/// # Errors
///
/// For anything else.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let unit_at = text
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("{text:?} has no unit (s, m, h, d or w)"))?;
    let (amount, unit) = text.split_at(unit_at);
    let amount: u64 = amount
        .parse()
        .map_err(|_| format!("{text:?} does not start with a number"))?;
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return Err(format!("{text:?} has an unknown unit; use s, m, h, d or w")),
    };
    Ok(Duration::from_secs(amount * seconds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Query, routing::get, Json, Router};
    use std::collections::HashMap;

    #[test]
    fn durations_read_like_prometheus_ones() {
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("6h").unwrap(), Duration::from_secs(21_600));
        assert_eq!(
            parse_duration(" 7d ").unwrap(),
            Duration::from_secs(604_800)
        );
        assert_eq!(
            parse_duration("2w").unwrap(),
            Duration::from_secs(1_209_600)
        );
        for bad in ["", "7", "d", "7y", "1.5h", "-1h"] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_default_step_gives_about_120_points_and_too_fine_a_step_is_refused() {
        let week = Duration::from_secs(604_800);
        assert_eq!(range_step(week, None).unwrap(), Duration::from_secs(5040));
        assert_eq!(
            range_step(Duration::from_secs(600), None).unwrap(),
            MIN_STEP,
            "never below the scrape interval"
        );
        let error = range_step(week, Some(Duration::from_secs(60))).unwrap_err();
        assert!(error.contains("at least 1210s"), "{error}");
    }

    /// A Prometheus that answers instant queries with `series` copies of one
    /// series, refuses `bad(`, and echoes the range parameters it got.
    async fn fake_prometheus(series: usize) -> String {
        let instant = move |Query(params): Query<HashMap<String, String>>| async move {
            if params["query"] == "bad(" {
                return Json(serde_json::json!({
                    "status": "error", "errorType": "bad_data", "error": "parse error"
                }));
            }
            let one = serde_json::json!({"metric": {"query": params["query"]}, "value": [0, "1"]});
            Json(serde_json::json!({
                "status": "success",
                "data": {"resultType": "vector", "result": vec![one; series]}
            }))
        };
        let range = |Query(params): Query<HashMap<String, String>>| async move {
            Json(serde_json::json!({
                "status": "success",
                "data": {"resultType": "matrix", "result": [{"metric": params, "values": []}]}
            }))
        };
        let app = Router::new()
            .route("/api/v1/query", get(instant))
            .route("/api/v1/query_range", get(range));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}/")
    }

    #[tokio::test]
    async fn an_instant_query_returns_its_series_up_to_the_limit() {
        let client = PrometheusClient::new(&fake_prometheus(MAX_SERIES + 7).await);

        let data = client.query("rbe_workers", None, None).await.unwrap();

        assert_eq!(data["result"].as_array().unwrap().len(), MAX_SERIES);
        assert_eq!(data["truncated_series"], 7);
        assert_eq!(data["result"][0]["metric"]["query"], "rbe_workers");
    }

    #[tokio::test]
    async fn a_range_query_asks_for_the_window_ending_now() {
        let client = PrometheusClient::new(&fake_prometheus(1).await);

        let data = client
            .query("up", Some(Duration::from_secs(3600)), None)
            .await
            .unwrap();

        let asked = &data["result"][0]["metric"];
        let start: u64 = asked["start"].as_str().unwrap().parse().unwrap();
        let end: u64 = asked["end"].as_str().unwrap().parse().unwrap();
        assert_eq!(end - start, 3600);
        assert_eq!(asked["step"], "30");
        assert!(data.get("truncated_series").is_none());
    }

    #[tokio::test]
    async fn a_refused_query_reports_why() {
        let client = PrometheusClient::new(&fake_prometheus(1).await);
        let error = client.query("bad(", None, None).await.unwrap_err();
        assert_eq!(
            error,
            "Prometheus refused the query (bad_data): parse error"
        );
    }
}
