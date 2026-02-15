//! Component provider: a gRPC ResourceProvider that handles Construct
//! calls for YAML-defined component resources.
//!
//! When a YAML template declares `components:`, the language host spawns
//! this provider via `RunPlugin`. The Pulumi engine then calls `Construct`
//! on this provider for each component instantiation.

use std::collections::HashMap;

use tonic::{Request, Response, Status};

use pulumi_rs_yaml_core::ast::template::TemplateDecl;
use pulumi_rs_yaml_core::eval::callback::ResourceCallback;
use pulumi_rs_yaml_core::eval::evaluator::Evaluator;
use pulumi_rs_yaml_core::eval::protobuf::protobuf_to_value;
use pulumi_rs_yaml_core::eval::value::Value;
use pulumi_rs_yaml_proto::pulumirpc;

use crate::clients::GrpcCallback;

/// A gRPC ResourceProvider that handles component construction.
pub struct ComponentProvider {
    /// The engine address for creating inner gRPC callbacks.
    pub engine_address: String,
    /// The monitor address for creating inner gRPC callbacks.
    pub monitor_address: String,
    /// The template containing component declarations (leaked to 'static).
    pub template: &'static TemplateDecl<'static>,
    /// The JSON-encoded schema for this package.
    pub schema_json: String,
    /// Project name for evaluator context.
    pub project: String,
    /// Stack name for evaluator context.
    pub stack: String,
    /// Whether we're in preview mode.
    pub dry_run: bool,
}

#[tonic::async_trait]
impl pulumirpc::resource_provider_server::ResourceProvider for ComponentProvider {
    async fn handshake(
        &self,
        _request: Request<pulumirpc::ProviderHandshakeRequest>,
    ) -> Result<Response<pulumirpc::ProviderHandshakeResponse>, Status> {
        Ok(Response::new(pulumirpc::ProviderHandshakeResponse {
            ..Default::default()
        }))
    }

    async fn parameterize(
        &self,
        _request: Request<pulumirpc::ParameterizeRequest>,
    ) -> Result<Response<pulumirpc::ParameterizeResponse>, Status> {
        Err(Status::unimplemented("Parameterize"))
    }

    async fn get_schema(
        &self,
        _request: Request<pulumirpc::GetSchemaRequest>,
    ) -> Result<Response<pulumirpc::GetSchemaResponse>, Status> {
        Ok(Response::new(pulumirpc::GetSchemaResponse {
            schema: self.schema_json.clone(),
        }))
    }

    async fn configure(
        &self,
        _request: Request<pulumirpc::ConfigureRequest>,
    ) -> Result<Response<pulumirpc::ConfigureResponse>, Status> {
        Ok(Response::new(pulumirpc::ConfigureResponse {
            accept_secrets: true,
            supports_preview: true,
            accept_resources: true,
            accept_outputs: true,
            supports_autonaming_configuration: false,
        }))
    }

    async fn construct(
        &self,
        request: Request<pulumirpc::ConstructRequest>,
    ) -> Result<Response<pulumirpc::ConstructResponse>, Status> {
        let req = request.into_inner();

        // Parse component type: "pkg:index:ComponentName" → extract "ComponentName"
        let component_name = req
            .r#type
            .rsplit(':')
            .next()
            .ok_or_else(|| Status::invalid_argument("invalid component type"))?;

        // Find the matching component declaration
        let component = self
            .template
            .components
            .iter()
            .find(|c| c.key.as_ref() == component_name)
            .ok_or_else(|| {
                Status::not_found(format!(
                    "component '{}' not found in template",
                    component_name
                ))
            })?;

        // Connect gRPC clients for inner resource registration
        let mut callback = GrpcCallback::connect(&self.monitor_address, &self.engine_address)
            .await
            .map_err(|e| Status::internal(format!("failed to connect: {}", e)))?;

        // Register the component resource itself (custom=false, remote=false)
        let comp_resp = callback
            .register_resource(
                &req.r#type,
                &req.name,
                false,
                false,
                HashMap::new(),
                Default::default(),
            )
            .map_err(|e| Status::internal(format!("failed to register component: {}", e)))?;

        let component_urn = comp_resp.urn.clone();

        // Build a synthetic TemplateDecl from the component's body
        let synthetic = TemplateDecl {
            meta: pulumi_rs_yaml_core::syntax::ExprMeta::no_span(),
            name: self.template.name.clone(),
            namespace: self.template.namespace.clone(),
            description: None,
            pulumi: Default::default(),
            config: component.component.inputs.clone(),
            variables: component.component.variables.clone(),
            resources: component.component.resources.clone(),
            outputs: component.component.outputs.clone(),
            components: Vec::new(),
        };

        // Leak the synthetic template so it has 'static lifetime
        let synthetic: &'static _ = Box::leak(Box::new(synthetic));

        // Create evaluator for the component body
        let mut eval = Evaluator::with_callback(
            self.project.clone(),
            self.stack.clone(),
            std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            self.dry_run,
            callback,
        );

        // Set component parent so inner resources inherit this component as parent
        eval.component_parent_urn = Some(component_urn.clone());

        // Convert construct inputs to raw config strings for the evaluator
        let raw_config = convert_construct_inputs(&req);

        // Evaluate the component body
        eval.evaluate_template(synthetic, &raw_config, &[]);

        if eval.diags.has_errors() {
            let errors: Vec<String> = eval
                .diags
                .iter()
                .filter(|d| d.is_error())
                .map(|d| d.summary.clone())
                .collect();
            return Err(Status::internal(format!(
                "component evaluation failed: {}",
                errors.join("; ")
            )));
        }

        // Collect outputs
        let output_values: HashMap<String, Value<'static>> = eval
            .outputs
            .drain()
            .map(|(k, v)| (k, v.into_owned()))
            .collect();

        // Register outputs on the component
        eval.callback_mut()
            .register_outputs(&component_urn, output_values.clone())
            .map_err(|e| {
                Status::internal(format!("failed to register component outputs: {}", e))
            })?;

        // Convert outputs to protobuf
        let state_fields: std::collections::BTreeMap<String, prost_types::Value> = output_values
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    pulumi_rs_yaml_core::eval::protobuf::value_to_protobuf(v),
                )
            })
            .collect();

        Ok(Response::new(pulumirpc::ConstructResponse {
            urn: component_urn,
            state: Some(prost_types::Struct {
                fields: state_fields,
            }),
            state_dependencies: HashMap::new(),
        }))
    }

    // --- Stub implementations for unused RPCs ---

    async fn check_config(
        &self,
        _request: Request<pulumirpc::CheckRequest>,
    ) -> Result<Response<pulumirpc::CheckResponse>, Status> {
        Err(Status::unimplemented("CheckConfig"))
    }

    async fn diff_config(
        &self,
        _request: Request<pulumirpc::DiffRequest>,
    ) -> Result<Response<pulumirpc::DiffResponse>, Status> {
        Err(Status::unimplemented("DiffConfig"))
    }

    async fn invoke(
        &self,
        _request: Request<pulumirpc::InvokeRequest>,
    ) -> Result<Response<pulumirpc::InvokeResponse>, Status> {
        Err(Status::unimplemented("Invoke"))
    }

    async fn call(
        &self,
        _request: Request<pulumirpc::CallRequest>,
    ) -> Result<Response<pulumirpc::CallResponse>, Status> {
        Err(Status::unimplemented("Call"))
    }

    async fn check(
        &self,
        _request: Request<pulumirpc::CheckRequest>,
    ) -> Result<Response<pulumirpc::CheckResponse>, Status> {
        Err(Status::unimplemented("Check"))
    }

    async fn diff(
        &self,
        _request: Request<pulumirpc::DiffRequest>,
    ) -> Result<Response<pulumirpc::DiffResponse>, Status> {
        Err(Status::unimplemented("Diff"))
    }

    async fn create(
        &self,
        _request: Request<pulumirpc::CreateRequest>,
    ) -> Result<Response<pulumirpc::CreateResponse>, Status> {
        Err(Status::unimplemented("Create"))
    }

    async fn read(
        &self,
        _request: Request<pulumirpc::ReadRequest>,
    ) -> Result<Response<pulumirpc::ReadResponse>, Status> {
        Err(Status::unimplemented("Read"))
    }

    async fn update(
        &self,
        _request: Request<pulumirpc::UpdateRequest>,
    ) -> Result<Response<pulumirpc::UpdateResponse>, Status> {
        Err(Status::unimplemented("Update"))
    }

    async fn delete(
        &self,
        _request: Request<pulumirpc::DeleteRequest>,
    ) -> Result<Response<()>, Status> {
        Err(Status::unimplemented("Delete"))
    }

    async fn cancel(&self, _request: Request<()>) -> Result<Response<()>, Status> {
        Ok(Response::new(()))
    }

    async fn get_plugin_info(
        &self,
        _request: Request<()>,
    ) -> Result<Response<pulumirpc::PluginInfo>, Status> {
        Ok(Response::new(pulumirpc::PluginInfo {
            version: "0.0.1".to_string(),
        }))
    }

    async fn attach(
        &self,
        _request: Request<pulumirpc::PluginAttach>,
    ) -> Result<Response<()>, Status> {
        Ok(Response::new(()))
    }

    async fn get_mapping(
        &self,
        _request: Request<pulumirpc::GetMappingRequest>,
    ) -> Result<Response<pulumirpc::GetMappingResponse>, Status> {
        Ok(Response::new(pulumirpc::GetMappingResponse {
            provider: String::new(),
            data: Vec::new(),
        }))
    }

    async fn get_mappings(
        &self,
        _request: Request<pulumirpc::GetMappingsRequest>,
    ) -> Result<Response<pulumirpc::GetMappingsResponse>, Status> {
        Ok(Response::new(pulumirpc::GetMappingsResponse {
            providers: Vec::new(),
        }))
    }
}

/// Converts ConstructRequest inputs to raw config strings for the evaluator.
fn convert_construct_inputs(req: &pulumirpc::ConstructRequest) -> HashMap<String, String> {
    let mut config = HashMap::new();
    if let Some(ref inputs) = req.inputs {
        for (k, v) in &inputs.fields {
            let eval_val = protobuf_to_value(v);
            match &eval_val {
                Value::String(s) => {
                    config.insert(k.clone(), s.to_string());
                }
                Value::Number(n) => {
                    config.insert(k.clone(), n.to_string());
                }
                Value::Bool(b) => {
                    config.insert(k.clone(), b.to_string());
                }
                _ => {
                    let json = eval_val.to_json();
                    config.insert(k.clone(), json.to_string());
                }
            }
        }
    }
    config
}
