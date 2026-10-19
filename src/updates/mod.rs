mod disk;
pub mod github;
mod policy;
mod supervisor;
pub mod version;

use crate::error::{Fault, Result};
use serde_json::{Value, json};
use std::{net::SocketAddr, path::Path};
pub use supervisor::run;

pub const PROTOCOL: u32 = 1;
pub const DATA_FORMAT: &str = "sqlite-v1/Claud2Api-vault-v1";
pub const RUNTIME_ABI: &str = "linux-amd64-glibc-2.36";
pub fn describe() -> Value {
    json!({"protocol":PROTOCOL,"version":crate::VERSION,"runtime_abi":RUNTIME_ABI,"data_format":DATA_FORMAT})
}
pub fn managed() -> bool {
    std::env::args().any(|v| v == "--worker")
        && std::env::var("CLAUD2API_WORKER_TOKEN")
            .is_ok_and(|v| v.len() == 64 && v.bytes().all(|c| c.is_ascii_hexdigit()))
}
pub fn ready(public: SocketAddr, control: SocketAddr) -> Result<()> {
    if !managed() {
        return Ok(());
    }
    let path = std::env::var("CLAUD2API_WORKER_READY").map_err(|_| Fault::internal())?;
    let token = std::env::var("CLAUD2API_WORKER_TOKEN").map_err(|_| Fault::internal())?;
    disk::atomic_json(
        Path::new(&path),
        &json!({"public":public,"control":control,"token":token,"build":describe()}),
    )
}
pub fn bounded_server(
    listener: tokio::net::TcpListener,
) -> Result<axum_server::Server<SocketAddr>> {
    let failed = |_| Fault::new(503, "api_error", "listener initialization failed");
    let mut server = axum_server::from_tcp(listener.into_std().map_err(failed)?).map_err(failed)?;
    let builder = server.http_builder();
    *builder = builder.clone().http1_only();
    server
        .http_builder()
        .http1()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(std::time::Duration::from_secs(10));
    Ok(server)
}
