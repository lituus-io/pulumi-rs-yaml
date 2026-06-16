pub mod ast;
pub mod classify;
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

/// Maximum gRPC message size (encode + decode) for Pulumi engine / monitor /
/// loader clients.
///
/// tonic defaults to a 4 MiB receive cap, but provider schemas exceed that —
/// the `gcp` classic provider schema is ~56 MB — which surfaces as
/// `OutOfRange: "decoded message length too large: found N bytes, the limit is
/// 4194304 bytes"` and silently disables schema-based type checking / preview
/// fidelity. Large resource registrations can hit the same cap. The Go Pulumi
/// engine raises this limit (`rpcutil`), so we match it: apply
/// `.max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)` (and the encoding
/// counterpart) to every tonic client we build.
pub const MAX_GRPC_MESSAGE_BYTES: usize = 512 * 1024 * 1024;
