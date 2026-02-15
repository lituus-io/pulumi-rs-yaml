mod clients;
mod component_provider;
pub(crate) mod exec;
mod runner;
mod schema_loader;
mod server;
mod template_loader;

use std::net::SocketAddr;

use pulumi_rs_yaml_proto::pulumirpc;
use tonic::transport::Server;

use server::YamlLanguageHost;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    // Check for exec subcommand: pulumi-language-yaml exec -- <command> [args...]
    if args.len() > 1 && args[1] == "exec" {
        let dash_pos = args.iter().position(|a| a == "--");
        match dash_pos {
            Some(pos) if pos + 1 < args.len() => {
                let command_args: Vec<String> = args[pos + 1..].to_vec();
                std::process::exit(exec::run_exec(&command_args));
            }
            _ => {
                eprintln!("usage: pulumi-language-yaml exec -- <command> [args...]");
                std::process::exit(1);
            }
        }
    }

    // Parse arguments: the last non-flag argument is the engine address
    let mut engine_address = String::new();
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--tracing" || arg == "--root" {
            // Skip flag and its value
            i += 2;
            continue;
        }
        if arg.starts_with("--") {
            i += 1;
            continue;
        }
        // Non-flag argument: engine address
        engine_address = arg.clone();
        i += 1;
    }

    if engine_address.is_empty() {
        eprintln!("usage: pulumi-language-yaml [--tracing <endpoint>] <engine_address>");
        std::process::exit(1);
    }

    // Create the language host
    let host = YamlLanguageHost::new(engine_address);

    // Bind to a random port on localhost
    let addr: SocketAddr = "127.0.0.1:0".parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;

    // Print the port to stdout so the Pulumi engine can connect
    println!("{}", local_addr.port());

    // Serve the language runtime
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    Server::builder()
        .add_service(pulumirpc::language_runtime_server::LanguageRuntimeServer::new(host))
        .serve_with_incoming(incoming)
        .await?;

    Ok(())
}
