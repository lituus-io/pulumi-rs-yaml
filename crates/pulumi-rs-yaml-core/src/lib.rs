pub mod ast;
pub mod completion;
pub mod config_types;
pub mod diag;
pub mod eval;
pub mod jinja;
pub mod multi_file;
pub mod packages;
pub mod pcl_gen;
pub mod schema;
pub mod source;
pub mod syntax;
pub mod type_check;

/// Normalizes a gRPC address string for tonic connection.
/// Ensures the address has an `http://` scheme unless it already has one
/// or uses a Unix socket.
///
/// Note: Pulumi always provides TCP addresses (`127.0.0.1:<port>`) for
/// engine/monitor communication, even on Windows. Named pipes are not used.
pub fn normalize_grpc_address(addr: &str) -> String {
    if addr.starts_with("unix:") || addr.starts_with("http://") || addr.starts_with("https://") {
        addr.to_string()
    } else {
        format!("http://{}", addr)
    }
}
