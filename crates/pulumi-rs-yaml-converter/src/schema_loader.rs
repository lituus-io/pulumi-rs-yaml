//! gRPC client for fetching provider schemas in the converter.
//!
//! Adapted from the language host's schema_loader.rs to avoid circular deps.

use pulumi_rs_yaml_core::packages::PackageDependency;
use pulumi_rs_yaml_core::schema::{self, SchemaStore};
use pulumi_rs_yaml_proto::codegen;

/// Wraps a `codegen.Loader` gRPC client for fetching provider schemas.
pub struct SchemaLoader {
    client: codegen::loader_client::LoaderClient<tonic::transport::Channel>,
}

impl SchemaLoader {
    /// Connect to the loader gRPC service at the given address.
    pub async fn connect(loader_target: &str) -> Result<Self, String> {
        let url = pulumi_rs_yaml_core::normalize_grpc_address(loader_target);
        let client = codegen::loader_client::LoaderClient::connect(url)
            .await
            .map_err(|e| format!("failed to connect to schema loader: {}", e))?;
        Ok(Self { client })
    }

    /// Fetch schemas for all referenced packages and build a `SchemaStore`.
    pub async fn fetch_and_build_store(&mut self, packages: &[PackageDependency]) -> SchemaStore {
        let mut store = SchemaStore::new();

        for pkg in packages {
            let parameterization = pkg.parameterization.as_ref().map(|p| {
                use base64::Engine;
                let value = base64::engine::general_purpose::STANDARD
                    .decode(&p.value)
                    .unwrap_or_default();
                codegen::Parameterization {
                    name: p.name.clone(),
                    version: p.version.clone(),
                    value,
                }
            });

            let request = codegen::GetSchemaRequest {
                package: pkg.name.clone(),
                version: pkg.version.clone(),
                download_url: pkg.download_url.clone(),
                parameterization,
            };

            match self.client.get_schema(request).await {
                Ok(resp) => {
                    let schema_bytes = resp.into_inner().schema;
                    match schema::parse_schema_json(&schema_bytes) {
                        Ok(pkg_schema) => {
                            store.insert(pkg_schema);
                        }
                        Err(e) => {
                            eprintln!("warning: failed to parse schema for {}: {}", pkg.name, e);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("warning: failed to fetch schema for {}: {}", pkg.name, e);
                }
            }
        }

        store
    }
}
