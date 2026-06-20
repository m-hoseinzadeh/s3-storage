//! Minimal S3-compatible file server.
//!
//! Composes the on-disk storage [`backend`] (the `s3s::S3` implementation) with
//! the deployment-facing layers: SigV4 auth, public/private [`access`] control,
//! and path-style + custom-domain [`host`] routing. `s3s` itself handles the S3
//! wire protocol (SigV4, streaming uploads, XML, multipart dispatch).

mod access;
mod backend;
mod config;
mod host;

use std::future::Future;
use std::io;

use s3s::auth::SimpleAuth;
use s3s::service::{S3Service, S3ServiceBuilder};
use tokio::net::TcpListener;
use tracing::{info, warn};

use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use hyper_util::server::graceful::GracefulShutdown;

pub use crate::access::AccessControl;
pub use crate::backend::FileSystem;
pub use crate::config::Config;
pub use crate::host::CustomHost;

/// Build the configured [`S3Service`], wiring the storage backend, auth, access
/// control, and host routing.
pub fn build_service(config: &Config) -> io::Result<S3Service> {
    let fs = FileSystem::new(&config.root).map_err(|e| io::Error::other(format!("{e:?}")))?;

    let mut builder = S3ServiceBuilder::new(fs);

    // Host routing: always installed so path-style works and custom domains /
    // base-domain virtual-hosting resolve when configured.
    builder.set_host(CustomHost::new(config.domains.clone(), config.parsed_domain_map()));

    match config.credentials() {
        Some((access_key, secret_key)) => {
            builder.set_auth(SimpleAuth::from_single(access_key, secret_key));
            builder.set_access(AccessControl::new(config.public_bucket_set()));
            info!("authentication enabled; access control active");
        }
        None => {
            warn!(
                "no credentials configured (S3_ACCESS_KEY/S3_SECRET_KEY) - the server is fully \
                 open and unauthenticated; intended for local development only"
            );
        }
    }

    Ok(builder.build())
}

/// Accept connections and serve `service` until `shutdown` resolves, then drain
/// in-flight connections (up to 10s).
pub async fn serve(
    service: S3Service,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send,
) -> io::Result<()> {
    let http = ConnBuilder::new(TokioExecutor::new());
    let graceful = GracefulShutdown::new();
    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        let (socket, _) = tokio::select! {
            res = listener.accept() => match res {
                Ok(conn) => conn,
                Err(err) => {
                    tracing::error!("error accepting connection: {err}");
                    continue;
                }
            },
            () = shutdown.as_mut() => break,
        };

        let conn = http.serve_connection(TokioIo::new(socket), service.clone());
        let conn = graceful.watch(conn.into_owned());
        tokio::spawn(async move {
            let _ = conn.await;
        });
    }

    tokio::select! {
        () = graceful.shutdown() => tracing::debug!("graceful shutdown complete"),
        () = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
            tracing::debug!("graceful shutdown timed out after 10s");
        }
    }

    Ok(())
}

/// Run the server from a [`Config`], shutting down on Ctrl-C.
pub async fn run(config: Config) -> io::Result<()> {
    let service = build_service(&config)?;

    let listener = TcpListener::bind((config.host.as_str(), config.port)).await?;
    let local_addr = listener.local_addr()?;
    info!("s3-storage listening on http://{local_addr}, data root: {}", config.root.display());

    serve(service, listener, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}
