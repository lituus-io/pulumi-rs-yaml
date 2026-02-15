use std::net::SocketAddr;

use pulumi_rs_yaml_proto::pulumirpc;
use tonic::transport::Server;

use pulumi_rs_yaml_converter::server::YamlConverter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Bind to a random port on localhost
    let addr: SocketAddr = "127.0.0.1:0".parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;

    // Print the port to stdout so the Pulumi engine can connect
    println!("{}", local_addr.port());

    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    Server::builder()
        .add_service(pulumirpc::converter_server::ConverterServer::new(
            YamlConverter,
        ))
        .serve_with_incoming(incoming)
        .await?;

    Ok(())
}
