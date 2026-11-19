//! Read-only test-log polling. Both adapters use the same error envelope,
//! including when there is no broker to supply one.
use logmon_broker_protocol::{TestLogQueryError, TestLogQueryErrorResult, TEST_LOG_SCHEMA_VERSION};
use logmon_broker_sdk::{Broker, BrokerError};
use serde_json::Value;
use std::time::Duration;

pub const QUERY_TIMEOUT: Duration = Duration::from_secs(3);

pub fn is_query(method: &str) -> bool {
    matches!(method, "test_log.status" | "test_log.list")
}

pub fn error(params: &Value, code: &str, message: impl ToString) -> Value {
    serde_json::to_value(TestLogQueryErrorResult {
        schema_version: TEST_LOG_SCHEMA_VERSION,
        error: TestLogQueryError {
            code: code.to_owned(),
            message: message.to_string(),
            domain: params["domain"].as_str().map(str::to_owned),
            run_id: params["run_id"].as_str().map(str::to_owned),
        },
    })
    .expect("query error contains only strings and an integer")
}

pub async fn query(broker: &Broker, method: &str, params: Value) -> Value {
    match tokio::time::timeout(QUERY_TIMEOUT, broker.call(method, params.clone())).await {
        Ok(Ok(value))
            if value["schema_version"].as_u64() == Some(TEST_LOG_SCHEMA_VERSION.into()) =>
        {
            value
        }
        Ok(Ok(_)) => error(
            &params,
            "unsupported_schema",
            "Unsupported test-log query schema version.",
        ),
        Ok(Err(e @ BrokerError::Method { .. } | e @ BrokerError::Protocol(_))) => {
            error(&params, "broker_error", e)
        }
        Ok(Err(e)) => error(&params, "broker_unreachable", e),
        Err(_) => error(&params, "broker_unreachable", "Broker query timed out."),
    }
}
