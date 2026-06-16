//! gRPC client for the Pulumi schema loader (`codegen.Loader` service).
//!
//! Fetches provider schemas via the `GetSchema` RPC and builds a `SchemaStore`
//! containing resource metadata (output properties, secrets, aliases, types).

use tokio::runtime::Handle;

use pulumi_rs_yaml_core::packages::PackageDependency;
use pulumi_rs_yaml_core::schema::{self, SchemaStore};
use pulumi_rs_yaml_proto::codegen;

/// Wraps a `codegen.Loader` gRPC client for fetching provider schemas.
pub struct SchemaLoader {
    client: codegen::loader_client::LoaderClient<tonic::transport::Channel>,
    handle: Handle,
}

impl SchemaLoader {
    /// Connect to the loader gRPC service at the given address.
    pub async fn connect(loader_target: &str) -> Result<Self, String> {
        let url = pulumi_rs_yaml_core::normalize_grpc_address(loader_target);
        // Provider schemas (e.g. gcp classic, ~56 MB) exceed tonic's default
        // 4 MiB decode cap — raise it so GetSchema succeeds and type checking /
        // preview fidelity work. See core::MAX_GRPC_MESSAGE_BYTES.
        let client = codegen::loader_client::LoaderClient::connect(url)
            .await
            .map_err(|e| format!("failed to connect to schema loader: {}", e))?
            .max_decoding_message_size(pulumi_rs_yaml_core::MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(pulumi_rs_yaml_core::MAX_GRPC_MESSAGE_BYTES);
        Ok(Self {
            client,
            handle: Handle::current(),
        })
    }

    /// Fetch schemas for all referenced packages and build a `SchemaStore`.
    ///
    /// Uses `block_in_place` to run async calls synchronously, matching
    /// the pattern in `clients.rs` for `GrpcCallback`.
    pub fn fetch_and_build_store(mut self, packages: &[PackageDependency]) -> SchemaStore {
        let mut store = SchemaStore::new();

        for pkg in packages {
            let request = schema::build_schema_request(pkg);

            let result = tokio::task::block_in_place(|| {
                self.handle.block_on(self.client.get_schema(request))
            });

            match result {
                Ok(resp) => {
                    let schema_bytes = resp.into_inner().schema;
                    if let Err(e) =
                        schema::process_schema_response(&mut store, &pkg.name, &schema_bytes)
                    {
                        eprintln!("warning: {}", e);
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
