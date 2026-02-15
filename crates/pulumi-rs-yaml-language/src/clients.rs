//! Tonic gRPC client wrappers for the Pulumi engine and resource monitor.

use std::collections::{BTreeMap, HashMap};

use pulumi_rs_yaml_core::eval::callback::{InvokeResponse, RegisterResponse, ResourceCallback};
use pulumi_rs_yaml_core::eval::context::EngineError;
use pulumi_rs_yaml_core::eval::protobuf::{protobuf_to_value, value_to_protobuf};
use pulumi_rs_yaml_core::eval::resource::ResolvedResourceOptions;
use pulumi_rs_yaml_core::eval::value::Value;

use pulumi_rs_yaml_proto::pulumirpc;
use tokio::runtime::Handle;

/// Wraps a tonic `ResourceMonitorClient` with synchronous methods
/// suitable for use as a `ResourceCallback`.
pub struct GrpcCallback {
    monitor: pulumirpc::resource_monitor_client::ResourceMonitorClient<tonic::transport::Channel>,
    engine: pulumirpc::engine_client::EngineClient<tonic::transport::Channel>,
    handle: Handle,
}

impl GrpcCallback {
    /// Creates a new GrpcCallback by connecting to the given addresses.
    pub async fn connect(monitor_address: &str, engine_address: &str) -> Result<Self, EngineError> {
        let monitor_url = pulumi_rs_yaml_core::normalize_grpc_address(monitor_address);
        let engine_url = pulumi_rs_yaml_core::normalize_grpc_address(engine_address);

        let monitor =
            pulumirpc::resource_monitor_client::ResourceMonitorClient::connect(monitor_url)
                .await
                .map_err(|e| EngineError::Grpc(format!("failed to connect to monitor: {}", e)))?;

        let engine = pulumirpc::engine_client::EngineClient::connect(engine_url)
            .await
            .map_err(|e| EngineError::Grpc(format!("failed to connect to engine: {}", e)))?;

        Ok(Self {
            monitor,
            engine,
            handle: Handle::current(),
        })
    }

    /// Registers a package with the engine and returns a package reference UUID.
    pub fn register_package(
        &mut self,
        name: &str,
        version: &str,
        download_url: &str,
        parameterization: Option<(String, String, Vec<u8>)>,
    ) -> Result<String, EngineError> {
        let param = parameterization.map(|(name, version, value)| pulumirpc::Parameterization {
            name,
            version,
            value,
        });

        let req = pulumirpc::RegisterPackageRequest {
            name: name.to_string(),
            version: version.to_string(),
            download_url: download_url.to_string(),
            checksums: HashMap::new(),
            parameterization: param,
        };

        tokio::task::block_in_place(|| {
            self.handle.block_on(async {
                let resp = self
                    .monitor
                    .register_package(req)
                    .await
                    .map_err(|e| {
                        EngineError::Grpc(format!(
                            "register package {}@{} failed: {}",
                            name, version, e
                        ))
                    })?
                    .into_inner();
                Ok(resp.r#ref)
            })
        })
    }

    /// Logs a message to the engine.
    pub fn log_to_engine(
        &mut self,
        severity: i32,
        message: &str,
        urn: &str,
        stream_id: i32,
        ephemeral: bool,
    ) -> Result<(), EngineError> {
        let req = pulumirpc::LogRequest {
            severity,
            message: message.to_string(),
            urn: urn.to_string(),
            stream_id,
            ephemeral,
        };
        tokio::task::block_in_place(|| {
            self.handle.block_on(async {
                self.engine
                    .log(req)
                    .await
                    .map_err(|e| EngineError::Grpc(format!("log failed: {}", e)))?;
                Ok(())
            })
        })
    }
}

impl ResourceCallback for GrpcCallback {
    fn register_resource(
        &mut self,
        type_token: &str,
        name: &str,
        custom: bool,
        remote: bool,
        inputs: HashMap<String, Value<'static>>,
        options: ResolvedResourceOptions,
    ) -> Result<RegisterResponse, EngineError> {
        // Convert inputs to protobuf struct
        let object = values_to_struct(&inputs);

        // Convert property dependencies
        let property_dependencies: HashMap<
            String,
            pulumirpc::register_resource_request::PropertyDependencies,
        > = options
            .property_dependencies
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    pulumirpc::register_resource_request::PropertyDependencies { urns: v.clone() },
                )
            })
            .collect();

        // Build custom timeouts
        let custom_timeouts = options.custom_timeouts.as_ref().map(|(c, u, d)| {
            pulumirpc::register_resource_request::CustomTimeouts {
                create: c.clone(),
                update: u.clone(),
                delete: d.clone(),
            }
        });

        let req = pulumirpc::RegisterResourceRequest {
            r#type: type_token.to_string(),
            name: name.to_string(),
            parent: options.parent_urn.unwrap_or_default(),
            custom,
            object: Some(object),
            protect: Some(options.protect),
            dependencies: options.depends_on.clone(),
            provider: options.provider_ref.unwrap_or_default(),
            property_dependencies,
            delete_before_replace: options.delete_before_replace,
            version: options.version.clone(),
            ignore_changes: options.ignore_changes.clone(),
            accept_secrets: true,
            additional_secret_outputs: options.additional_secret_outputs.clone(),
            alias_ur_ns: Vec::new(),
            import_id: options.import_id.clone(),
            custom_timeouts,
            delete_before_replace_defined: options.delete_before_replace,
            supports_partial_values: true,
            remote,
            accept_resources: true,
            providers: options.providers.clone(),
            replace_on_changes: options.replace_on_changes.clone(),
            plugin_download_url: options.plugin_download_url.clone(),
            plugin_checksums: HashMap::new(),
            retain_on_delete: Some(options.retain_on_delete),
            aliases: options
                .aliases
                .iter()
                .map(|a| match a {
                    pulumi_rs_yaml_core::eval::resource::ResolvedAlias::Urn(urn) => {
                        pulumirpc::Alias {
                            alias: Some(pulumirpc::alias::Alias::Urn(urn.clone())),
                        }
                    }
                    pulumi_rs_yaml_core::eval::resource::ResolvedAlias::Spec {
                        name,
                        r#type,
                        stack,
                        project,
                        parent_urn,
                        no_parent,
                    } => {
                        let parent = if *no_parent {
                            Some(pulumirpc::alias::spec::Parent::NoParent(true))
                        } else if !parent_urn.is_empty() {
                            Some(pulumirpc::alias::spec::Parent::ParentUrn(
                                parent_urn.clone(),
                            ))
                        } else {
                            None
                        };
                        pulumirpc::Alias {
                            alias: Some(pulumirpc::alias::Alias::Spec(pulumirpc::alias::Spec {
                                name: name.clone(),
                                r#type: r#type.clone(),
                                stack: stack.clone(),
                                project: project.clone(),
                                parent,
                            })),
                        }
                    }
                })
                .collect(),
            deleted_with: options.deleted_with.clone(),
            alias_specs: true,
            source_position: None,
            supports_result_reporting: true,
            package_ref: options.package_ref.clone(),
            replace_with: options.replace_with.clone(),
            replacement_trigger: None,
            stack_trace: None,
            parent_stack_trace_handle: String::new(),
            transforms: Vec::new(),
            hooks: None,
            hide_diffs: options.hide_diffs.clone(),
        };

        tokio::task::block_in_place(|| {
            self.handle.block_on(async {
                let resp = self
                    .monitor
                    .register_resource(req)
                    .await
                    .map_err(|e| {
                        EngineError::Registration(format!("register {} failed: {}", name, e))
                    })?
                    .into_inner();

                let outputs = struct_to_values(resp.object.as_ref());

                Ok(RegisterResponse {
                    urn: resp.urn,
                    id: resp.id,
                    outputs,
                    stables: resp.stables,
                })
            })
        })
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
        let properties = values_to_struct(&inputs);

        let req = pulumirpc::ReadResourceRequest {
            r#type: type_token.to_string(),
            name: name.to_string(),
            id: id.to_string(),
            parent: parent_urn.to_string(),
            properties: Some(properties),
            dependencies: Vec::new(),
            provider: provider_ref.to_string(),
            version: version.to_string(),
            accept_secrets: true,
            additional_secret_outputs: Vec::new(),
            accept_resources: true,
            plugin_download_url: String::new(),
            plugin_checksums: HashMap::new(),
            source_position: None,
            stack_trace: None,
            parent_stack_trace_handle: String::new(),
            package_ref: String::new(),
        };

        tokio::task::block_in_place(|| {
            self.handle.block_on(async {
                let resp = self
                    .monitor
                    .read_resource(req)
                    .await
                    .map_err(|e| EngineError::Grpc(format!("read resource failed: {}", e)))?
                    .into_inner();

                let outputs = struct_to_values(resp.properties.as_ref());

                Ok(RegisterResponse {
                    urn: resp.urn,
                    id: id.to_string(),
                    outputs,
                    stables: Vec::new(),
                })
            })
        })
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
        let args_struct = values_to_struct(&args);

        let req = pulumirpc::ResourceInvokeRequest {
            tok: token.to_string(),
            args: Some(args_struct),
            provider: provider.to_string(),
            version: version.to_string(),
            accept_resources: true,
            plugin_download_url: String::new(),
            plugin_checksums: HashMap::new(),
            source_position: None,
            package_ref: String::new(),
            stack_trace: None,
            parent_stack_trace_handle: String::new(),
        };

        tokio::task::block_in_place(|| {
            self.handle.block_on(async {
                let resp = self
                    .monitor
                    .invoke(req)
                    .await
                    .map_err(|e| EngineError::Invoke(format!("invoke {} failed: {}", token, e)))?
                    .into_inner();

                let return_values = struct_to_values(resp.r#return.as_ref());
                let failures = resp
                    .failures
                    .iter()
                    .map(|f| (f.property.clone(), f.reason.clone()))
                    .collect();

                Ok(InvokeResponse {
                    return_values,
                    failures,
                })
            })
        })
    }

    fn register_outputs(
        &mut self,
        urn: &str,
        outputs: HashMap<String, Value<'static>>,
    ) -> Result<(), EngineError> {
        let outputs_struct = values_to_struct(&outputs);

        let req = pulumirpc::RegisterResourceOutputsRequest {
            urn: urn.to_string(),
            outputs: Some(outputs_struct),
        };

        tokio::task::block_in_place(|| {
            self.handle.block_on(async {
                self.monitor
                    .register_resource_outputs(req)
                    .await
                    .map_err(|e| EngineError::Grpc(format!("register outputs failed: {}", e)))?;
                Ok(())
            })
        })
    }

    fn log(&mut self, severity: i32, message: &str) {
        let _ = self.log_to_engine(severity, message, "", 0, false);
    }
}

/// Converts a HashMap of Values to a protobuf Struct.
fn values_to_struct(values: &HashMap<String, Value<'static>>) -> prost_types::Struct {
    let fields: BTreeMap<String, prost_types::Value> = values
        .iter()
        .map(|(k, v)| (k.clone(), value_to_protobuf(v)))
        .collect();
    prost_types::Struct { fields }
}

/// Converts a protobuf Struct to a HashMap of Values.
fn struct_to_values(s: Option<&prost_types::Struct>) -> HashMap<String, Value<'static>> {
    match s {
        Some(obj) => obj
            .fields
            .iter()
            .map(|(k, v)| (k.clone(), protobuf_to_value(v)))
            .collect(),
        None => HashMap::new(),
    }
}
