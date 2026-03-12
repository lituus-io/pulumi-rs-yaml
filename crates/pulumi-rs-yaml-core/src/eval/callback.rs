use std::collections::HashMap;

use crate::eval::context::EngineError;
use crate::eval::resource::ResolvedResourceOptions;
use crate::eval::value::Value;

/// Response from registering a resource via callback.
#[derive(Debug, Clone)]
pub struct RegisterResponse {
    pub urn: String,
    pub id: String,
    pub outputs: HashMap<String, Value<'static>>,
    pub stables: Vec<String>,
}

/// Response from invoking a function via callback.
#[derive(Debug, Clone)]
pub struct InvokeResponse {
    pub return_values: HashMap<String, Value<'static>>,
    pub failures: Vec<(String, String)>,
}

/// Trait for resource operations during evaluation.
///
/// The evaluator calls these methods when it encounters resource declarations
/// and invoke expressions. Implementations can be:
/// - `NoopCallback` for unit tests (no actual registration)
/// - `MockCallback` for integration tests (record & replay)
/// - `GrpcCallback` for real deployment (wraps tonic gRPC clients)
#[allow(clippy::too_many_arguments)]
pub trait ResourceCallback {
    /// Register a resource with the engine.
    fn register_resource(
        &mut self,
        type_token: &str,
        name: &str,
        custom: bool,
        remote: bool,
        inputs: HashMap<String, Value<'static>>,
        options: ResolvedResourceOptions,
    ) -> Result<RegisterResponse, EngineError>;

    /// Read an existing resource from the engine.
    fn read_resource(
        &mut self,
        type_token: &str,
        name: &str,
        id: &str,
        parent_urn: &str,
        inputs: HashMap<String, Value<'static>>,
        provider_ref: &str,
        version: &str,
    ) -> Result<RegisterResponse, EngineError>;

    /// Invoke a provider function.
    fn invoke(
        &mut self,
        token: &str,
        args: HashMap<String, Value<'static>>,
        provider: &str,
        version: &str,
        parent: &str,
        depends_on: &[String],
    ) -> Result<InvokeResponse, EngineError>;

    /// Register outputs for a resource (typically the stack).
    fn register_outputs(
        &mut self,
        urn: &str,
        outputs: HashMap<String, Value<'static>>,
    ) -> Result<(), EngineError>;

    /// Log a message to the engine.
    fn log(&mut self, severity: i32, message: &str);
}

/// No-op callback that returns placeholder values.
///
/// Used by unit tests that don't need actual resource registration.
/// Resources get empty URN/ID and their inputs echoed back as outputs.
#[derive(Clone)]
pub struct NoopCallback;

impl ResourceCallback for NoopCallback {
    fn register_resource(
        &mut self,
        _type_token: &str,
        _name: &str,
        _custom: bool,
        _remote: bool,
        inputs: HashMap<String, Value<'static>>,
        _options: ResolvedResourceOptions,
    ) -> Result<RegisterResponse, EngineError> {
        Ok(RegisterResponse {
            urn: String::new(),
            id: String::new(),
            outputs: inputs,
            stables: Vec::new(),
        })
    }

    fn read_resource(
        &mut self,
        _type_token: &str,
        _name: &str,
        _id: &str,
        _parent_urn: &str,
        inputs: HashMap<String, Value<'static>>,
        _provider_ref: &str,
        _version: &str,
    ) -> Result<RegisterResponse, EngineError> {
        Ok(RegisterResponse {
            urn: String::new(),
            id: String::new(),
            outputs: inputs,
            stables: Vec::new(),
        })
    }

    fn invoke(
        &mut self,
        _token: &str,
        _args: HashMap<String, Value<'static>>,
        _provider: &str,
        _version: &str,
        _parent: &str,
        _depends_on: &[String],
    ) -> Result<InvokeResponse, EngineError> {
        Ok(InvokeResponse {
            return_values: HashMap::new(),
            failures: Vec::new(),
        })
    }

    fn register_outputs(
        &mut self,
        _urn: &str,
        _outputs: HashMap<String, Value<'static>>,
    ) -> Result<(), EngineError> {
        Ok(())
    }

    fn log(&mut self, _severity: i32, _message: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    #[test]
    fn test_noop_register_echoes_inputs() {
        let mut noop = NoopCallback;
        let mut inputs = HashMap::new();
        inputs.insert(
            "key".to_string(),
            Value::String(Cow::Owned("value".to_string())),
        );
        let resp = noop
            .register_resource(
                "test:Type",
                "name",
                true,
                false,
                inputs.clone(),
                Default::default(),
            )
            .unwrap();
        assert_eq!(resp.urn, "");
        assert_eq!(resp.id, "");
        assert_eq!(
            resp.outputs.get("key").and_then(|v| v.as_str()),
            Some("value")
        );
    }

    #[test]
    fn test_noop_read_echoes_inputs() {
        let mut noop = NoopCallback;
        let mut inputs = HashMap::new();
        inputs.insert("prop".to_string(), Value::Bool(true));
        let resp = noop
            .read_resource("test:Type", "name", "id-1", "", inputs, "", "")
            .unwrap();
        assert_eq!(
            resp.outputs.get("prop").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn test_noop_invoke_returns_empty() {
        let mut noop = NoopCallback;
        let resp = noop
            .invoke("test:func", HashMap::new(), "", "", "", &[])
            .unwrap();
        assert!(resp.return_values.is_empty());
        assert!(resp.failures.is_empty());
    }

    #[test]
    fn test_noop_register_outputs_ok() {
        let mut noop = NoopCallback;
        assert!(noop.register_outputs("urn:test", HashMap::new()).is_ok());
    }

    #[test]
    fn test_noop_log_does_nothing() {
        let mut noop = NoopCallback;
        noop.log(3, "error message"); // should not panic
    }
}
