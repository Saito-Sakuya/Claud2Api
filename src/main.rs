use claud2api::{
    config::Config,
    control,
    engine::Engine,
    updates::{self, bounded_server},
    web,
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn main() {
    if std::env::args().any(|a| a == "--describe-build") {
        println!("{}", updates::describe());
        return;
    }
    if std::env::args().any(|a| a == "--healthcheck") {
        use std::io::{BufRead, BufReader, Read, Write};
        let port = std::env::var("PORT")
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(8080);
        let result = (|| -> std::io::Result<bool> {
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            let mut socket = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
            socket.set_read_timeout(Some(Duration::from_secs(2)))?;
            socket.write_all(
                b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )?;
            let mut status = String::new();
            BufReader::new(socket.take(256)).read_line(&mut status)?;
            Ok(status.starts_with("HTTP/1.1 200 "))
        })();
        std::process::exit(if matches!(result, Ok(true)) { 0 } else { 1 });
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(
            std::thread::available_parallelism()
                .map(|n| n.get().clamp(2, 4))
                .unwrap_or(2),
        )
        .enable_all()
        .build()
        .expect("runtime initialization");
    if let Err(error) = runtime.block_on(run()) {
        eprintln!(
            "startup_failed status={} code={} message={}",
            error.status, error.kind, error.message
        );
        std::process::exit(1);
    }
}
async fn run() -> claud2api::error::Result<()> {
    let config = Config::load()?;
    if !updates::managed() && cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        let stop = CancellationToken::new();
        let signal_stop = stop.clone();
        let signals = tokio::spawn(async move {
            signal().await;
            signal_stop.cancel();
        });
        let result = updates::run(config, stop).await;
        signals.abort();
        return result;
    }
    run_worker(config).await
}
async fn run_worker(config: Config) -> claud2api::error::Result<()> {
    let engine = Engine::open(config.clone()).await?;
    if updates::managed() {
        let stop = engine.shutdown.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut byte = [0; 1];
            while matches!(std::io::stdin().read(&mut byte), Ok(1)) {}
            stop.cancel();
        });
    }
    let public = tokio::net::TcpListener::bind(config.public_bind)
        .await
        .map_err(|_| {
            claud2api::error::Fault::new(503, "api_error", "cannot bind public listener")
        })?;
    let private = tokio::net::TcpListener::bind(config.control_bind)
        .await
        .map_err(|_| {
            claud2api::error::Fault::new(503, "api_error", "cannot bind control listener")
        })?;
    updates::ready(
        public
            .local_addr()
            .map_err(|_| claud2api::error::Fault::internal())?,
        private
            .local_addr()
            .map_err(|_| claud2api::error::Fault::internal())?,
    )?;
    let worker_stop = engine.shutdown.clone();
    let shutdown = CancellationToken::new();
    let janitor_stop = shutdown.clone();
    let janitor_engine = engine.clone();
    let janitor = tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(15));
        loop {
            tokio::select! {_=janitor_stop.cancelled()=>break,_=timer.tick()=>{if janitor_engine.sweep().await.is_err(){eprintln!("background_failed code=state_or_cleanup");}}}
        }
    });
    eprintln!(
        "Claud2Api {} ready public={} control={}",
        claud2api::VERSION,
        config.public_bind,
        config.control_bind
    );
    let public_handle = axum_server::Handle::new();
    let private_handle = axum_server::Handle::new();
    let public = bounded_server(public)?.handle(public_handle.clone());
    let private = bounded_server(private)?.handle(private_handle.clone());
    let private_engine = engine.clone();
    let status_store = engine.store.clone();
    let mut public =
        tokio::spawn(async move { public.serve(web::public(engine).into_make_service()).await });
    let mut private = tokio::spawn(async move {
        private
            .serve(control::router(private_engine).into_make_service())
            .await
    });
    let completed = tokio::select! {_=signal()=>0,_=worker_stop.cancelled()=>0,_=&mut public=>1,_=&mut private=>2};
    shutdown.cancel();
    public_handle.graceful_shutdown(Some(Duration::from_secs(5)));
    private_handle.graceful_shutdown(Some(Duration::from_secs(5)));
    if completed != 1
        && tokio::time::timeout(Duration::from_secs(5), &mut public)
            .await
            .is_err()
    {
        public.abort();
    }
    if completed != 2
        && tokio::time::timeout(Duration::from_secs(5), &mut private)
            .await
            .is_err()
    {
        private.abort();
    }
    if tokio::time::timeout(Duration::from_secs(5), janitor)
        .await
        .is_err()
    {
        eprintln!("shutdown code=cleanup_deferred");
    }
    let _ = status_store.flush_status().await;
    Ok(())
}
async fn signal() {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
