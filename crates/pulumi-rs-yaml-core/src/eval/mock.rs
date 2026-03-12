use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::eval::callback::{InvokeResponse, RegisterResponse, ResourceCallback};
use crate::eval::context::EngineError;
use crate::eval::resource::ResolvedResourceOptions;
use crate::eval::value::Value;

/// A captured resource registration for test assertions.
#[derive(Debug, Clone)]
pub struct CapturedRegistration {
    pub type_token: String,
    pub name: String,
    pub custom: bool,
    pub remote: bool,
    pub inputs: HashMap<String, Value<'static>>,
    pub options: ResolvedResourceOptions,
}

/// A captured invoke call for test assertions.
#[derive(Debug, Clone)]
pub struct CapturedInvoke {
    pub token: String,
    pub args: HashMap<String, Value<'static>>,
    pub provider: String,
    pub version: String,
}

/// A captured output registration for test assertions.
#[derive(Debug, Clone)]
pub struct CapturedOutputs {
    pub urn: String,
    pub outputs: HashMap<String, Value<'static>>,
}

/// A captured read_resource call for test assertions.
#[derive(Debug, Clone)]
pub struct CapturedRead {
    pub type_token: String,
    pub name: String,
    pub id: String,
    pub parent_urn: String,
    pub inputs: HashMap<String, Value<'static>>,
    pub provider_ref: String,
    pub version: String,
}

/// Mock resource callback that records calls and returns pre-configured responses.
///
/// Uses `Arc<Mutex>` internally for thread-safety, enabling use in parallel
/// evaluation. All clones share the same underlying state.
#[derive(Clone)]
pub struct MockCallback {
    /// Pre-configured register responses, consumed in order.
    pub register_responses: Arc<Mutex<VecDeque<RegisterResponse>>>,
    /// Pre-configured invoke responses, consumed in order.
    pub invoke_responses: Arc<Mutex<VecDeque<InvokeResponse>>>,
    /// Captured registration calls.
    pub registrations: Arc<Mutex<Vec<CapturedRegistration>>>,
    /// Captured invoke calls.
    pub invocations: Arc<Mutex<Vec<CapturedInvoke>>>,
    /// Captured output registrations.
    pub output_registrations: Arc<Mutex<Vec<CapturedOutputs>>>,
    /// Captured log messages.
    pub logs: Arc<Mutex<Vec<(i32, String)>>>,
    /// Captured read_resource calls.
    pub reads: Arc<Mutex<Vec<CapturedRead>>>,
    /// Pre-configured read responses, consumed in order.
    pub read_responses: Arc<Mutex<VecDeque<RegisterResponse>>>,
    /// Default URN prefix for auto-generated responses.
    pub urn_prefix: String,
    /// Counter for auto-generating URNs.
    counter: Arc<AtomicU32>,
}

impl MockCallback {
    /// Creates a new mock with no pre-configured responses.
    /// When no responses are queued, auto-generates placeholder responses.
    pub fn new() -> Self {
        Self {
            register_responses: Arc::new(Mutex::new(VecDeque::new())),
            invoke_responses: Arc::new(Mutex::new(VecDeque::new())),
            registrations: Arc::new(Mutex::new(Vec::new())),
            invocations: Arc::new(Mutex::new(Vec::new())),
            output_registrations: Arc::new(Mutex::new(Vec::new())),
            logs: Arc::new(Mutex::new(Vec::new())),
            reads: Arc::new(Mutex::new(Vec::new())),
            read_responses: Arc::new(Mutex::new(VecDeque::new())),
            urn_prefix: "urn:pulumi:test::test".to_string(),
            counter: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Creates a mock with pre-configured register responses.
    pub fn with_register_responses(responses: Vec<RegisterResponse>) -> Self {
        let mock = Self::new();
        *mock.register_responses.lock().unwrap() = responses.into();
        mock
    }

    /// Creates a mock with pre-configured invoke responses.
    pub fn with_invoke_responses(responses: Vec<InvokeResponse>) -> Self {
        let mock = Self::new();
        *mock.invoke_responses.lock().unwrap() = responses.into();
        mock
    }

    /// Creates a mock with pre-configured read responses.
    pub fn with_read_responses(responses: Vec<RegisterResponse>) -> Self {
        let mock = Self::new();
        *mock.read_responses.lock().unwrap() = responses.into();
        mock
    }

    /// Returns captured registrations.
    pub fn registrations(&self) -> Vec<CapturedRegistration> {
        self.registrations.lock().unwrap().clone()
    }

    /// Returns captured invocations.
    pub fn invocations(&self) -> Vec<CapturedInvoke> {
        self.invocations.lock().unwrap().clone()
    }

    /// Returns captured output registrations.
    pub fn output_registrations(&self) -> Vec<CapturedOutputs> {
        self.output_registrations.lock().unwrap().clone()
    }

    /// Returns captured log messages.
    pub fn logs(&self) -> Vec<(i32, String)> {
        self.logs.lock().unwrap().clone()
    }

    /// Returns captured read_resource calls.
    pub fn reads(&self) -> Vec<CapturedRead> {
        self.reads.lock().unwrap().clone()
    }

    /// Generates an auto-URN for the given type and name.
    fn auto_urn(&self, type_token: &str, name: &str) -> String {
        format!("{}::{}::{}", self.urn_prefix, type_token, name)
    }

    /// Generates a sequential auto-ID.
    fn auto_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        format!("id-{:04x}", n)
    }
}

impl Default for MockCallback {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceCallback for MockCallback {
    fn register_resource(
        &mut self,
        type_token: &str,
        name: &str,
        custom: bool,
        remote: bool,
        inputs: HashMap<String, Value<'static>>,
        options: ResolvedResourceOptions,
    ) -> Result<RegisterResponse, EngineError> {
        // Capture the call
        self.registrations
            .lock()
            .unwrap()
            .push(CapturedRegistration {
                type_token: type_token.to_string(),
                name: name.to_string(),
                custom,
                remote,
                inputs: inputs.clone(),
                options,
            });

        // Return pre-configured response or auto-generate one
        if let Some(resp) = self.register_responses.lock().unwrap().pop_front() {
            Ok(resp)
        } else {
            Ok(RegisterResponse {
                urn: self.auto_urn(type_token, name),
                id: self.auto_id(),
                outputs: inputs,
                stables: Vec::new(),
            })
        }
    }

    fn read_resource(
        &mut self,
        type_token: &str,
        name: &str,
        id: &str,
        parent_urn: &str,
        inputs: HashMap<String, Value<'static>>,
        provider_ref: &str,
        version: &str,
    ) -> Result<RegisterResponse, EngineError> {
        // Capture the call
        self.reads.lock().unwrap().push(CapturedRead {
            type_token: type_token.to_string(),
            name: name.to_string(),
            id: id.to_string(),
            parent_urn: parent_urn.to_string(),
            inputs: inputs.clone(),
            provider_ref: provider_ref.to_string(),
            version: version.to_string(),
        });

        // Return pre-configured response or auto-generate one
        if let Some(resp) = self.read_responses.lock().unwrap().pop_front() {
            Ok(resp)
        } else {
            Ok(RegisterResponse {
                urn: self.auto_urn(type_token, name),
                id: id.to_string(),
                outputs: inputs,
                stables: Vec::new(),
            })
        }
    }

    fn invoke(
        &mut self,
        token: &str,
        args: HashMap<String, Value<'static>>,
        provider: &str,
        version: &str,
        _parent: &str,
        _depends_on: &[String],
    ) -> Result<InvokeResponse, EngineError> {
        // Capture the call
        self.invocations.lock().unwrap().push(CapturedInvoke {
            token: token.to_string(),
            args: args.clone(),
            provider: provider.to_string(),
            version: version.to_string(),
        });

        // Return pre-configured response or empty
        if let Some(resp) = self.invoke_responses.lock().unwrap().pop_front() {
            Ok(resp)
        } else {
            Ok(InvokeResponse {
                return_values: HashMap::new(),
                failures: Vec::new(),
            })
        }
    }

    fn register_outputs(
        &mut self,
        urn: &str,
        outputs: HashMap<String, Value<'static>>,
    ) -> Result<(), EngineError> {
        self.output_registrations
            .lock()
            .unwrap()
            .push(CapturedOutputs {
                urn: urn.to_string(),
                outputs,
            });
        Ok(())
    }

    fn log(&mut self, severity: i32, message: &str) {
        self.logs
            .lock()
            .unwrap()
            .push((severity, message.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    #[test]
    fn test_mock_auto_generates_responses() {
        let mut mock = MockCallback::new();
        let result = mock
            .register_resource(
                "aws:s3:Bucket",
                "myBucket",
                true,
                false,
                HashMap::new(),
                ResolvedResourceOptions::default(),
            )
            .unwrap();

        assert!(result.urn.contains("aws:s3:Bucket"));
        assert!(result.urn.contains("myBucket"));
        assert!(!result.id.is_empty());
    }

    #[test]
    fn test_mock_uses_queued_responses() {
        let resp = RegisterResponse {
            urn: "custom-urn".to_string(),
            id: "custom-id".to_string(),
            outputs: HashMap::new(),
            stables: vec!["id".to_string()],
        };
        let mut mock = MockCallback::with_register_responses(vec![resp]);

        let result = mock
            .register_resource(
                "aws:s3:Bucket",
                "myBucket",
                true,
                false,
                HashMap::new(),
                ResolvedResourceOptions::default(),
            )
            .unwrap();

        assert_eq!(result.urn, "custom-urn");
        assert_eq!(result.id, "custom-id");
    }

    #[test]
    fn test_mock_captures_registrations() {
        let mut mock = MockCallback::new();
        let mut inputs = HashMap::new();
        inputs.insert(
            "bucketName".to_string(),
            Value::String(Cow::Owned("test-bucket".to_string())),
        );

        mock.register_resource(
            "aws:s3:Bucket",
            "myBucket",
            true,
            false,
            inputs,
            ResolvedResourceOptions::default(),
        )
        .unwrap();

        let regs = mock.registrations();
        assert_eq!(regs.len(), 1);
        assert_eq!(regs[0].type_token, "aws:s3:Bucket");
        assert_eq!(regs[0].name, "myBucket");
        assert!(regs[0].custom);
        assert_eq!(
            regs[0].inputs.get("bucketName").and_then(|v| v.as_str()),
            Some("test-bucket")
        );
    }

    #[test]
    fn test_mock_captures_invocations() {
        let mut mock = MockCallback::new();
        let mut args = HashMap::new();
        args.insert(
            "name".to_string(),
            Value::String(Cow::Owned("my-vm".to_string())),
        );

        mock.invoke("aws:ec2:getAmi", args, "", "", "", &[])
            .unwrap();

        let invocations = mock.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].token, "aws:ec2:getAmi");
    }

    #[test]
    fn test_mock_invoke_with_queued_response() {
        let mut return_values = HashMap::new();
        return_values.insert(
            "id".to_string(),
            Value::String(Cow::Owned("ami-12345".to_string())),
        );

        let resp = InvokeResponse {
            return_values,
            failures: Vec::new(),
        };
        let mut mock = MockCallback::with_invoke_responses(vec![resp]);

        let result = mock
            .invoke("aws:ec2:getAmi", HashMap::new(), "", "", "", &[])
            .unwrap();
        assert_eq!(
            result.return_values.get("id").and_then(|v| v.as_str()),
            Some("ami-12345")
        );
    }

    #[test]
    fn test_mock_captures_logs() {
        let mut mock = MockCallback::new();
        mock.log(1, "test message");
        mock.log(3, "error message");

        let logs = mock.logs();
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0], (1, "test message".to_string()));
        assert_eq!(logs[1], (3, "error message".to_string()));
    }

    #[test]
    fn test_mock_captures_outputs() {
        let mut mock = MockCallback::new();
        let mut outputs = HashMap::new();
        outputs.insert(
            "result".to_string(),
            Value::String(Cow::Owned("value".to_string())),
        );
        mock.register_outputs("urn:stack", outputs).unwrap();

        let regs = mock.output_registrations();
        assert_eq!(regs.len(), 1);
        assert_eq!(regs[0].urn, "urn:stack");
    }

    #[test]
    fn test_mock_clone_shares_state() {
        let mut mock1 = MockCallback::new();
        let mut mock2 = mock1.clone();

        mock1
            .register_resource(
                "test:A",
                "a",
                true,
                false,
                HashMap::new(),
                ResolvedResourceOptions::default(),
            )
            .unwrap();

        mock2
            .register_resource(
                "test:B",
                "b",
                true,
                false,
                HashMap::new(),
                ResolvedResourceOptions::default(),
            )
            .unwrap();

        // Both registrations visible from either mock
        assert_eq!(mock1.registrations().len(), 2);
        assert_eq!(mock2.registrations().len(), 2);
    }
}
