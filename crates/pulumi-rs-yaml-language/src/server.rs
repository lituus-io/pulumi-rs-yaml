//! The YAML language host gRPC server implementing `LanguageRuntime`.

use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use pulumi_rs_yaml_core::multi_file;
use pulumi_rs_yaml_core::packages;
use pulumi_rs_yaml_proto::pulumirpc;

use crate::runner;

/// The YAML language host implementation.
pub struct YamlLanguageHost {
    /// Address of the Pulumi engine gRPC server.
    pub engine_address: String,
}

impl YamlLanguageHost {
    pub fn new(engine_address: String) -> Self {
        Self { engine_address }
    }

    /// Loads all template files from a program directory and extracts referenced packages.
    ///
    /// Scans all `Pulumi.*.yaml` files for resource types to determine required packages.
    #[allow(clippy::result_large_err)]
    fn load_and_get_packages(
        &self,
        program_directory: &str,
    ) -> Result<Vec<packages::PackageDependency>, Status> {
        let dir = Path::new(program_directory);

        // Load and merge all project files (without Jinja preprocessing for package discovery)
        let (merged, load_diags) = multi_file::load_project(dir, None);
        if load_diags.has_errors() {
            // Swallow errors to allow project config to evaluate
            return Ok(Vec::new());
        }

        let template = merged.as_template_decl();
        let lock_packages = packages::search_package_decls(dir);
        Ok(packages::get_referenced_packages(&template, &lock_packages))
    }
}

type StreamResponse<T> =
    Pin<Box<dyn tokio_stream::Stream<Item = Result<T, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl pulumirpc::language_runtime_server::LanguageRuntime for YamlLanguageHost {
    type InstallDependenciesStream = StreamResponse<pulumirpc::InstallDependenciesResponse>;
    type RunPluginStream = StreamResponse<pulumirpc::RunPluginResponse>;

    async fn get_required_plugins(
        &self,
        _request: Request<pulumirpc::GetRequiredPluginsRequest>,
    ) -> Result<Response<pulumirpc::GetRequiredPluginsResponse>, Status> {
        // Deprecated in favor of GetRequiredPackages
        Ok(Response::new(pulumirpc::GetRequiredPluginsResponse {
            plugins: Vec::new(),
        }))
    }

    async fn get_required_packages(
        &self,
        request: Request<pulumirpc::GetRequiredPackagesRequest>,
    ) -> Result<Response<pulumirpc::GetRequiredPackagesResponse>, Status> {
        let req = request.into_inner();
        let program_dir = req
            .info
            .as_ref()
            .map(|i| i.program_directory.as_str())
            .unwrap_or("");

        let packages = self.load_and_get_packages(program_dir)?;

        let proto_packages: Vec<pulumirpc::PackageDependency> = packages
            .iter()
            .map(|pkg| {
                let parameterization =
                    pkg.parameterization
                        .as_ref()
                        .map(|p| pulumirpc::PackageParameterization {
                            name: p.name.clone(),
                            version: p.version.clone(),
                            value: base64_decode_or_empty(&p.value),
                        });

                pulumirpc::PackageDependency {
                    name: pkg.name.clone(),
                    kind: "resource".to_string(),
                    version: pkg.version.clone(),
                    server: pkg.download_url.clone(),
                    checksums: HashMap::new(),
                    parameterization,
                }
            })
            .collect();

        Ok(Response::new(pulumirpc::GetRequiredPackagesResponse {
            packages: proto_packages,
        }))
    }

    async fn run(
        &self,
        request: Request<pulumirpc::RunRequest>,
    ) -> Result<Response<pulumirpc::RunResponse>, Status> {
        let req = request.into_inner();

        let program_dir = req
            .info
            .as_ref()
            .map(|i| i.program_directory.as_str())
            .unwrap_or(&req.pwd);

        let loader_target = if req.loader_target.is_empty() {
            None
        } else {
            Some(req.loader_target.as_str())
        };

        let result = runner::run(
            &req.project,
            &req.stack,
            &req.pwd,
            &req.monitor_address,
            &self.engine_address,
            &req.config,
            &req.config_secret_keys,
            req.dry_run,
            program_dir,
            &req.organization,
            loader_target,
            req.parallel,
        )
        .await;

        Ok(Response::new(pulumirpc::RunResponse {
            error: result.error,
            bail: result.bail,
        }))
    }

    async fn get_plugin_info(
        &self,
        _request: Request<()>,
    ) -> Result<Response<pulumirpc::PluginInfo>, Status> {
        Ok(Response::new(pulumirpc::PluginInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
        }))
    }

    async fn install_dependencies(
        &self,
        _request: Request<pulumirpc::InstallDependenciesRequest>,
    ) -> Result<Response<Self::InstallDependenciesStream>, Status> {
        // YAML has no dependencies to install.
        // Send an empty stream that completes immediately.
        let (tx, rx) = mpsc::channel(1);
        drop(tx); // Close immediately
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn runtime_options_prompts(
        &self,
        _request: Request<pulumirpc::RuntimeOptionsRequest>,
    ) -> Result<Response<pulumirpc::RuntimeOptionsResponse>, Status> {
        Ok(Response::new(pulumirpc::RuntimeOptionsResponse {
            prompts: Vec::new(),
        }))
    }

    async fn about(
        &self,
        _request: Request<pulumirpc::AboutRequest>,
    ) -> Result<Response<pulumirpc::AboutResponse>, Status> {
        Ok(Response::new(pulumirpc::AboutResponse {
            executable: String::new(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            metadata: HashMap::new(),
        }))
    }

    async fn get_program_dependencies(
        &self,
        request: Request<pulumirpc::GetProgramDependenciesRequest>,
    ) -> Result<Response<pulumirpc::GetProgramDependenciesResponse>, Status> {
        let req = request.into_inner();
        let program_dir = req
            .info
            .as_ref()
            .map(|i| i.program_directory.as_str())
            .unwrap_or("");

        let packages = self.load_and_get_packages(program_dir)?;

        let deps: Vec<pulumirpc::DependencyInfo> = packages
            .iter()
            .map(|pkg| pulumirpc::DependencyInfo {
                name: pkg.name.clone(),
                version: pkg.version.clone(),
            })
            .collect();

        Ok(Response::new(pulumirpc::GetProgramDependenciesResponse {
            dependencies: deps,
        }))
    }

    async fn run_plugin(
        &self,
        request: Request<pulumirpc::RunPluginRequest>,
    ) -> Result<Response<Self::RunPluginStream>, Status> {
        let req = request.into_inner();

        let program_directory = req
            .info
            .as_ref()
            .map(|i| i.program_directory.clone())
            .unwrap_or_default();

        if program_directory.is_empty() {
            return Err(Status::invalid_argument(
                "RunPlugin requires a program directory",
            ));
        }

        // Load the template to get component declarations
        let dir = std::path::Path::new(&program_directory);
        let (merged, load_diags) = multi_file::load_project(dir, None);
        if load_diags.has_errors() {
            return Err(Status::internal("failed to load component template"));
        }

        let template = merged.as_template_decl();

        // Verify there are components
        if template.components.is_empty() {
            return Err(Status::invalid_argument(
                "no component declarations found in template",
            ));
        }

        // Generate schema JSON from component declarations
        let schema = pulumi_rs_yaml_core::schema::generate_component_schema(&template);
        let schema_json = serde_json::to_string(&schema)
            .map_err(|e| Status::internal(format!("schema serialization failed: {}", e)))?;

        // Leak the template for 'static lifetime (process-scoped)
        let template: &'static _ = Box::leak(Box::new(template));

        // Determine monitor address from env (set by engine before calling RunPlugin)
        let monitor_address = std::env::var("PULUMI_MONITOR_ADDRESS").unwrap_or_default();

        // Create the component provider
        let provider = crate::component_provider::ComponentProvider {
            engine_address: self.engine_address.clone(),
            monitor_address,
            template,
            schema_json,
            project: String::new(),
            stack: String::new(),
            dry_run: false,
        };

        // Spawn a gRPC server for the component provider on a random port
        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| Status::internal(format!("failed to bind: {}", e)))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| Status::internal(format!("failed to get local addr: {}", e)))?;
        let port = local_addr.port();

        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

        // Spawn the server in a background task
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(
                    pulumirpc::resource_provider_server::ResourceProviderServer::new(provider),
                )
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        // Create response stream: write port number to stdout, then wait
        let (tx, rx) = mpsc::channel(4);

        // Send the port number on stdout (protocol requirement)
        let port_msg = format!("{}\n", port);
        let _ = tx
            .send(Ok(pulumirpc::RunPluginResponse {
                output: Some(pulumirpc::run_plugin_response::Output::Stdout(
                    port_msg.into_bytes(),
                )),
            }))
            .await;

        // Keep the stream open — the server runs until the engine disconnects
        // When the stream is dropped, send shutdown signal
        tokio::spawn(async move {
            // The tx will be held by the stream; when the engine drops
            // the stream connection, tx gets dropped and this task ends
            tx.closed().await;
            let _ = shutdown_tx.send(());
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn generate_program(
        &self,
        request: Request<pulumirpc::GenerateProgramRequest>,
    ) -> Result<Response<pulumirpc::GenerateProgramResponse>, Status> {
        let req = request.into_inner();
        let result = pulumi_rs_yaml_core::pcl_gen::generate_program(&req.source);

        // Convert diagnostics
        let diagnostics: Vec<pulumirpc::codegen::Diagnostic> = result
            .diagnostics
            .into_vec()
            .into_iter()
            .map(|d| pulumirpc::codegen::Diagnostic {
                severity: if d.is_error() {
                    pulumirpc::codegen::DiagnosticSeverity::DiagError as i32
                } else {
                    pulumirpc::codegen::DiagnosticSeverity::DiagWarning as i32
                },
                summary: d.summary,
                detail: d.detail,
                ..Default::default()
            })
            .collect();

        Ok(Response::new(pulumirpc::GenerateProgramResponse {
            source: result.files,
            diagnostics,
        }))
    }

    async fn generate_project(
        &self,
        request: Request<pulumirpc::GenerateProjectRequest>,
    ) -> Result<Response<pulumirpc::GenerateProjectResponse>, Status> {
        let req = request.into_inner();

        // Read PCL source files from source_directory
        let source_dir = std::path::Path::new(&req.source_directory);
        let mut sources = std::collections::HashMap::new();
        if source_dir.is_dir() {
            let entries = std::fs::read_dir(source_dir)
                .map_err(|e| Status::internal(format!("failed to read source directory: {}", e)))?;
            for entry in entries {
                let entry = entry.map_err(|e| {
                    Status::internal(format!("failed to read directory entry: {}", e))
                })?;
                let path = entry.path();
                if path.is_file() {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        let content = std::fs::read_to_string(&path).map_err(|e| {
                            Status::internal(format!("failed to read {}: {}", path.display(), e))
                        })?;
                        sources.insert(name.to_string(), content);
                    }
                }
            }
        }

        let result = pulumi_rs_yaml_core::pcl_gen::generate_program(&sources);

        // Write generated files to the target directory
        let target_dir = std::path::Path::new(&req.target_directory);
        std::fs::create_dir_all(target_dir)
            .map_err(|e| Status::internal(format!("failed to create target directory: {}", e)))?;

        // Extract project name from the project JSON field
        let project_name = if req.project.is_empty() {
            "project".to_string()
        } else {
            // project field is JSON — try to extract "name"
            serde_json::from_str::<serde_json::Value>(&req.project)
                .ok()
                .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(String::from))
                .unwrap_or_else(|| "project".to_string())
        };

        // Write each generated file
        for (filename, content) in &result.files {
            let file_path = target_dir.join(filename);
            std::fs::write(&file_path, content).map_err(|e| {
                Status::internal(format!("failed to write {}: {}", file_path.display(), e))
            })?;
        }

        // Write Pulumi.yaml project file
        let project_yaml_path = target_dir.join("Pulumi.yaml");
        if let Some(yaml_content) = result.files.get("Pulumi.yaml") {
            // Prepend name/runtime header to generated Pulumi.yaml
            let yaml_str = String::from_utf8_lossy(yaml_content);
            let full_content = format!("name: {}\nruntime: yaml\n{}", project_name, yaml_str);
            std::fs::write(&project_yaml_path, full_content)
                .map_err(|e| Status::internal(format!("failed to write Pulumi.yaml: {}", e)))?;
        } else if !project_yaml_path.exists() {
            // No generated content, write minimal project file
            let content = format!("name: {}\nruntime: yaml\n", project_name);
            std::fs::write(&project_yaml_path, content)
                .map_err(|e| Status::internal(format!("failed to write Pulumi.yaml: {}", e)))?;
        }

        // Convert diagnostics
        let diagnostics: Vec<pulumirpc::codegen::Diagnostic> = result
            .diagnostics
            .into_vec()
            .into_iter()
            .map(|d| pulumirpc::codegen::Diagnostic {
                severity: if d.is_error() {
                    pulumirpc::codegen::DiagnosticSeverity::DiagError as i32
                } else {
                    pulumirpc::codegen::DiagnosticSeverity::DiagWarning as i32
                },
                summary: d.summary,
                detail: d.detail,
                ..Default::default()
            })
            .collect();

        Ok(Response::new(pulumirpc::GenerateProjectResponse {
            diagnostics,
        }))
    }

    async fn generate_package(
        &self,
        request: Request<pulumirpc::GeneratePackageRequest>,
    ) -> Result<Response<pulumirpc::GeneratePackageResponse>, Status> {
        let req = request.into_inner();

        if !req.extra_files.is_empty() {
            return Err(Status::invalid_argument(
                "overlays are not supported for YAML",
            ));
        }

        // Parse the schema to extract package name and version
        let spec: serde_json::Value = serde_json::from_str(&req.schema)
            .map_err(|e| Status::internal(format!("failed to parse schema: {}", e)))?;

        let pkg_name = spec
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let pkg_version = spec
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let download_url = spec
            .get("pluginDownloadURL")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Check for parameterization
        let parameterization = spec.get("parameterization");

        // Build the lock file
        let mut lock = packages::PackageDecl {
            package_declaration_version: 1,
            name: String::new(),
            version: String::new(),
            download_url: String::new(),
            parameterization: None,
        };

        if let Some(param) = parameterization {
            // Parameterized package: base provider info goes in the lock root
            let base_name = param
                .get("baseProvider")
                .and_then(|bp| bp.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let base_version = param
                .get("baseProvider")
                .and_then(|bp| bp.get("version"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            lock.name = base_name;
            lock.version = base_version;
            lock.download_url = download_url;

            if pkg_version.is_empty() {
                return Err(Status::invalid_argument(
                    "parameterized package must have a version",
                ));
            }

            let param_value = param
                .get("parameter")
                .map(|v| {
                    use base64::Engine;
                    // Parameter is stored as base64-encoded bytes
                    if let Some(s) = v.as_str() {
                        s.to_string()
                    } else {
                        base64::engine::general_purpose::STANDARD.encode(v.to_string().as_bytes())
                    }
                })
                .unwrap_or_default();

            lock.parameterization = Some(packages::ParameterizationDecl {
                name: pkg_name.clone(),
                version: pkg_version.clone(),
                value: param_value,
            });
        } else {
            lock.name = pkg_name.clone();
            lock.version = pkg_version.clone();
            lock.download_url = download_url;
        }

        // Write the YAML lock file
        let version_suffix = if pkg_version.is_empty() {
            String::new()
        } else {
            format!("-{}", pkg_version)
        };
        let dest = Path::new(&req.directory).join(format!("{}{}.yaml", pkg_name, version_suffix));

        // Create directory
        std::fs::create_dir_all(&req.directory).map_err(|e| {
            Status::internal(format!(
                "could not create output directory {}: {}",
                req.directory, e
            ))
        })?;

        // Serialize and write
        let yaml_content = serde_yaml::to_string(&lock)
            .map_err(|e| Status::internal(format!("failed to serialize lock file: {}", e)))?;

        std::fs::write(&dest, yaml_content).map_err(|e| {
            Status::internal(format!(
                "could not write output file {}: {}",
                dest.display(),
                e
            ))
        })?;

        Ok(Response::new(pulumirpc::GeneratePackageResponse {
            diagnostics: Vec::new(),
        }))
    }

    async fn pack(
        &self,
        request: Request<pulumirpc::PackRequest>,
    ) -> Result<Response<pulumirpc::PackResponse>, Status> {
        let req = request.into_inner();

        // Create destination directory
        std::fs::create_dir_all(&req.destination_directory).map_err(|e| {
            Status::internal(format!("failed to create destination directory: {}", e))
        })?;

        // Read package directory
        let entries: Vec<_> = std::fs::read_dir(&req.package_directory)
            .map_err(|e| Status::internal(format!("reading package directory: {}", e)))?
            .filter_map(|e| e.ok())
            .collect();

        // Expect exactly one file
        if entries.len() != 1 {
            if entries.is_empty() {
                return Err(Status::internal(format!(
                    "no files in package directory {}",
                    req.package_directory
                )));
            }
            return Err(Status::internal(format!(
                "multiple files in package directory {}: {} and {}",
                req.package_directory,
                entries[0].file_name().to_string_lossy(),
                entries[1].file_name().to_string_lossy()
            )));
        }

        let file_name = entries[0].file_name();
        let src = Path::new(&req.package_directory).join(&file_name);
        let dst = Path::new(&req.destination_directory).join(&file_name);

        std::fs::copy(&src, &dst).map_err(|e| {
            Status::internal(format!(
                "copying {} to {}: {}",
                src.display(),
                dst.display(),
                e
            ))
        })?;

        Ok(Response::new(pulumirpc::PackResponse {
            artifact_path: dst.to_string_lossy().to_string(),
        }))
    }

    async fn template(
        &self,
        _request: Request<pulumirpc::TemplateRequest>,
    ) -> Result<Response<pulumirpc::TemplateResponse>, Status> {
        // Not implemented in Go either
        Err(Status::unimplemented("Template not implemented"))
    }

    async fn handshake(
        &self,
        _request: Request<pulumirpc::LanguageHandshakeRequest>,
    ) -> Result<Response<pulumirpc::LanguageHandshakeResponse>, Status> {
        Ok(Response::new(pulumirpc::LanguageHandshakeResponse {}))
    }

    async fn link(
        &self,
        _request: Request<pulumirpc::LinkRequest>,
    ) -> Result<Response<pulumirpc::LinkResponse>, Status> {
        // YAML doesn't need to do anything to link in a package.
        // We still implement Link so the engine knows that we have done all we need to do.
        Ok(Response::new(pulumirpc::LinkResponse {
            import_instructions: String::new(),
        }))
    }

    async fn cancel(&self, _request: Request<()>) -> Result<Response<()>, Status> {
        Ok(Response::new(()))
    }
}

/// Decodes a base64 string to bytes, returning empty on failure.
fn base64_decode_or_empty(s: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .unwrap_or_default()
}
