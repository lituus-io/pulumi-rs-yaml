/// Errors from engine/monitor communication.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("gRPC error: {0}")]
    Grpc(String),
    #[error("resource registration failed: {0}")]
    Registration(String),
    #[error("invoke failed: {0}")]
    Invoke(String),
    #[error("feature not supported: {0}")]
    FeatureNotSupported(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_engine_error_display() {
        let err = EngineError::Grpc("connection refused".to_string());
        assert_eq!(err.to_string(), "gRPC error: connection refused");

        let err = EngineError::Registration("missing type".to_string());
        assert_eq!(
            err.to_string(),
            "resource registration failed: missing type"
        );
    }
}
