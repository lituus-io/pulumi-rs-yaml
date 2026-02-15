use std::path::Path;

use pulumi_rs_yaml_proto::pulumirpc;
use pulumi_rs_yaml_proto::pulumirpc::codegen as proto_codegen;

use crate::schema_loader::SchemaLoader;
use crate::{yaml_to_pcl, yaml_to_pcl_with_schema};

/// gRPC service implementation for the YAML converter.
pub struct YamlConverter;

#[tonic::async_trait]
impl pulumirpc::converter_server::Converter for YamlConverter {
    async fn convert_state(
        &self,
        _request: tonic::Request<pulumirpc::ConvertStateRequest>,
    ) -> Result<tonic::Response<pulumirpc::ConvertStateResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(
            "ConvertState is not supported for YAML",
        ))
    }

    async fn convert_program(
        &self,
        request: tonic::Request<pulumirpc::ConvertProgramRequest>,
    ) -> Result<tonic::Response<pulumirpc::ConvertProgramResponse>, tonic::Status> {
        let req = request.into_inner();
        let source_dir = Path::new(&req.source_directory);
        let target_dir = Path::new(&req.target_directory);

        // Find and read the Pulumi.yaml file
        let yaml_path = find_yaml_file(source_dir).ok_or_else(|| {
            tonic::Status::invalid_argument(format!(
                "no Pulumi.yaml or Pulumi.yml found in {}",
                source_dir.display()
            ))
        })?;

        let yaml_source = std::fs::read_to_string(&yaml_path).map_err(|e| {
            tonic::Status::internal(format!("failed to read {}: {}", yaml_path.display(), e))
        })?;

        // Optionally load schemas if loader_target is available
        let result = if !req.loader_target.is_empty() {
            // Try to load schemas for schema-based token resolution
            match SchemaLoader::connect(&req.loader_target).await {
                Ok(mut loader) => {
                    // Parse template to discover packages
                    let (template, _) =
                        pulumi_rs_yaml_core::ast::parse::parse_template(&yaml_source, None);
                    let lock_packages =
                        pulumi_rs_yaml_core::packages::search_package_decls(source_dir);
                    let pkgs = pulumi_rs_yaml_core::packages::get_referenced_packages(
                        &template,
                        &lock_packages,
                    );
                    let store = loader.fetch_and_build_store(&pkgs).await;
                    yaml_to_pcl_with_schema(&yaml_source, store)
                }
                Err(e) => {
                    eprintln!("warning: schema loader: {}", e);
                    yaml_to_pcl(&yaml_source)
                }
            }
        } else {
            yaml_to_pcl(&yaml_source)
        };

        // Write PCL to target directory
        std::fs::create_dir_all(target_dir).map_err(|e| {
            tonic::Status::internal(format!(
                "failed to create target directory {}: {}",
                target_dir.display(),
                e
            ))
        })?;

        let pcl_path = target_dir.join("main.pp");
        std::fs::write(&pcl_path, &result.pcl_text).map_err(|e| {
            tonic::Status::internal(format!("failed to write {}: {}", pcl_path.display(), e))
        })?;

        // Copy Pulumi.yaml project file to target
        let project_target = target_dir.join("Pulumi.yaml");
        if let Err(e) = std::fs::copy(&yaml_path, &project_target) {
            // Non-fatal — just warn
            eprintln!(
                "warning: failed to copy {} to {}: {}",
                yaml_path.display(),
                project_target.display(),
                e
            );
        }

        // Convert diagnostics
        let diagnostics = result
            .diagnostics
            .into_vec()
            .into_iter()
            .map(|d| proto_codegen::Diagnostic {
                severity: if d.is_error() {
                    proto_codegen::DiagnosticSeverity::DiagError as i32
                } else {
                    proto_codegen::DiagnosticSeverity::DiagWarning as i32
                },
                summary: d.summary,
                detail: d.detail,
                ..Default::default()
            })
            .collect();

        Ok(tonic::Response::new(pulumirpc::ConvertProgramResponse {
            diagnostics,
        }))
    }
}

/// Finds Pulumi.yaml or Pulumi.yml in a directory.
fn find_yaml_file(dir: &Path) -> Option<std::path::PathBuf> {
    let yaml = dir.join("Pulumi.yaml");
    if yaml.exists() {
        return Some(yaml);
    }
    let yml = dir.join("Pulumi.yml");
    if yml.exists() {
        return Some(yml);
    }
    None
}
