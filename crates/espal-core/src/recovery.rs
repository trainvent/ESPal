use std::time::Duration;

pub const TRANSPORT_ERROR: &str = "TLS/WebSocket connection failed or disconnected";
pub const SERVER_ERROR: &str = "OpenAI server temporarily unavailable";

pub fn handshake_error(status: i32) -> &'static str {
    match status {
        401 => "OpenAI authentication failed (401); re-provision the API key",
        403 => "OpenAI access denied (403); check project/model access",
        400 => "OpenAI handshake rejected (400); check configured Realtime model",
        404 => "OpenAI model not found (404); check configured Realtime model",
        429 => "OpenAI rate or quota limit (429); no automatic retry",
        500..=599 => SERVER_ERROR,
        // A disconnected socket can retain the successful upgrade status.
        0 | 101 => TRANSPORT_ERROR,
        _ => "OpenAI handshake rejected; check configuration",
    }
}

pub fn server_error(error: &serde_json::Value) -> &'static str {
    let code = error["code"].as_str().unwrap_or("");
    let kind = error["type"].as_str().unwrap_or("");
    match (code, kind) {
        ("invalid_api_key", _) | (_, "authentication_error") => {
            "OpenAI authentication failed; re-provision the API key"
        }
        (
            "insufficient_quota"
            | "credit_balance_exhausted"
            | "organization_spend_limit_exceeded"
            | "project_spend_limit_exceeded"
            | "organization_usage_limit_exceeded",
            _,
        )
        | (_, "insufficient_quota") => "OpenAI quota exhausted; check billing and usage limits",
        ("rate_limit_exceeded", _) | (_, "rate_limit_error") => {
            "OpenAI rate limit reached; no automatic retry"
        }
        ("model_not_found", _) => "OpenAI model unavailable; check model access",
        ("server_error", _) | (_, "server_error") => SERVER_ERROR,
        _ => "OpenAI returned an error; check model access, key and limits",
    }
}

pub fn is_retryable(error: &str) -> bool {
    matches!(
        error,
        TRANSPORT_ERROR
            | SERVER_ERROR
            | "Wi-Fi state failed"
            | "Wi-Fi stop failed"
            | "Wi-Fi start failed"
            | "Wi-Fi connection failed or timed out"
            | "DHCP failed or timed out"
            | "SNTP initialization failed"
            | "SNTP timed out; TLS not attempted"
            | "WebSocket start failed"
            | "WebSocket connection timed out"
            | "WebSocket send failed"
            | "Realtime session deadline exceeded"
    )
}

#[derive(Default)]
pub struct Recovery {
    failures: usize,
}

pub struct Decision {
    pub error_sound: bool,
    pub retry_after: Option<Duration>,
}

impl Recovery {
    pub fn reset(&mut self) {
        self.failures = 0;
    }

    pub fn failed(&mut self, error: &str) -> Decision {
        let retry_after = if is_retryable(error) {
            [1, 2, 4]
                .get(self.failures)
                .copied()
                .map(Duration::from_secs)
        } else {
            None
        };
        let error_sound = retry_after.is_none();
        self.failures = self.failures.saturating_add(1);
        Decision {
            error_sound,
            retry_after,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn outage_only_signals_final_failure_after_three_retries() {
        let mut recovery = Recovery::default();
        for (index, delay) in [Some(1), Some(2), Some(4), None].into_iter().enumerate() {
            let decision = recovery.failed(TRANSPORT_ERROR);
            assert_eq!(decision.error_sound, index == 3);
            assert_eq!(decision.retry_after, delay.map(Duration::from_secs));
        }
        assert!(recovery.failed(TRANSPORT_ERROR).retry_after.is_none());
        recovery.reset();
        let next_outage = recovery.failed("WebSocket send failed");
        assert!(!next_outage.error_sound);
        assert_eq!(next_outage.retry_after, Some(Duration::from_secs(1)));
    }

    #[test]
    fn auth_quota_config_and_local_failures_never_retry() {
        for status in [400, 401, 403, 404, 429, 418] {
            assert!(Recovery::default()
                .failed(handshake_error(status))
                .retry_after
                .is_none());
        }
        for error in [
            json!({"code":"invalid_api_key"}),
            json!({"type":"insufficient_quota", "code":"billing_limit_reached"}),
            json!({"code":"credit_balance_exhausted"}),
            json!({"code":"project_spend_limit_exceeded", "type":"server_error"}),
            json!({"code":"rate_limit_exceeded"}),
            json!({"code":"model_not_found"}),
            json!({"type":"invalid_request_error"}),
        ] {
            assert!(!is_retryable(server_error(&error)));
        }
        assert!(!is_retryable("Microphone capture failed"));
        assert!(!is_retryable("Playback overflow; response cancelled"));
    }

    #[test]
    fn transport_and_service_failures_can_recover() {
        for error in [
            handshake_error(0),
            handshake_error(101),
            handshake_error(503),
            server_error(&json!({"type":"server_error"})),
            "Wi-Fi connection failed or timed out",
            "DHCP failed or timed out",
            "SNTP timed out; TLS not attempted",
            "WebSocket send failed",
        ] {
            assert!(is_retryable(error), "{error}");
        }
        let mut recovery = Recovery::default();
        recovery.failed(TRANSPORT_ERROR);
        let final_failure = recovery.failed(handshake_error(401));
        assert!(final_failure.retry_after.is_none());
        assert!(final_failure.error_sound);
    }
}
