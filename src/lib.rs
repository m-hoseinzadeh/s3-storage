//! Minimal S3-compatible file server.
//!
//! Composes the on-disk storage [`backend`] (the `s3s::S3` implementation) with
//! the deployment-facing layers: SigV4 auth, public/private [`access`] control,
//! and path-style + custom-domain [`host`] routing. `s3s` itself handles the S3
//! wire protocol (SigV4, streaming uploads, XML, multipart dispatch).

mod access;
mod admin;
mod backend;
mod config;
mod cors;
mod host;
mod settings;

use std::collections::HashSet;
use std::future::Future;
use std::io;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::service::Service;
use hyper::Request;
use s3s::auth::SimpleAuth;
use s3s::service::{S3Service, S3ServiceBuilder};
use s3s::{HttpError, HttpResponse};
use tokio::net::TcpListener;
use tracing::{info, warn};
use uuid::Uuid;

use crate::admin::{AdminRoute, AdminState};
use crate::backend::SharedFileSystem;

use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use hyper_util::server::graceful::GracefulShutdown;

pub use crate::access::{AccessControl, PublicReadAccess};
pub use crate::backend::FileSystem;
pub use crate::config::Config;
pub use crate::cors::CorsService;
pub use crate::host::CustomHost;
pub use crate::settings::{RuntimeSettings, SettingsStore, SettingsUpdate, SharedSettings};

/// Open the on-disk storage backend. A single instance is shared by all three
/// services so they share its atomic temp-file counter and never collide on writes.
pub fn open_backend(config: &Config) -> io::Result<Arc<FileSystem>> {
    FileSystem::new(&config.root)
        .map(Arc::new)
        .map_err(|e| io::Error::other(format!("{e:?}")))
}

/// Host routing, installed on every S3-serving service so path-style works and
/// custom domains / base-domain virtual-hosting resolve when configured. Reads the
/// domain configuration live from the settings store.
fn host_router(settings: &SharedSettings) -> CustomHost {
    CustomHost::new(Arc::clone(settings))
}

/// Build the **authenticated S3 API** service (SDK clients). Anonymous access is
/// rejected: access control is installed with an empty public-bucket set, so only
/// SigV4-authenticated requests pass. With no credentials configured the service is
/// left fully open (local-development behaviour).
pub fn build_api_service(config: &Config, fs: Arc<FileSystem>, settings: &SharedSettings) -> S3Service {
    let mut builder = S3ServiceBuilder::new(SharedFileSystem::new(fs));
    builder.set_host(host_router(settings));

    match config.credentials() {
        Some((access_key, secret_key)) => {
            builder.set_auth(SimpleAuth::from_single(access_key, secret_key));
            // Empty public set => anonymous requests have no public bucket to match
            // and are denied; the public port serves anonymous reads instead.
            builder.set_access(AccessControl::new(HashSet::new()));
            info!("API: authentication enabled; anonymous access rejected");
        }
        None => {
            warn!(
                "no credentials configured (S3_ACCESS_KEY/S3_SECRET_KEY) - the API is fully \
                 open and unauthenticated; intended for local development only"
            );
        }
    }

    builder.build()
}

/// Build the **public** read-only service: [`PublicReadAccess`] permits only
/// `GET`/`HEAD` against configured public buckets and nothing else.
///
/// `PublicReadAccess` ignores credentials, but `s3s` runs the access stage *only*
/// when an auth provider is configured. So we always install one — the real
/// credentials when present, otherwise a throwaway per-process pair (it is never
/// advertised and `PublicReadAccess` disregards it anyway). This keeps the public
/// port strictly read-only and public-scoped even in credential-less dev mode,
/// rather than silently degrading to a fully open read/write endpoint.
pub fn build_public_service(config: &Config, fs: Arc<FileSystem>, settings: &SharedSettings) -> S3Service {
    let mut builder = S3ServiceBuilder::new(SharedFileSystem::new(fs));
    builder.set_host(host_router(settings));
    let (access_key, secret_key) = config
        .credentials()
        .unwrap_or_else(|| (Uuid::new_v4().to_string(), Uuid::new_v4().to_string()));
    builder.set_auth(SimpleAuth::from_single(access_key, secret_key));
    builder.set_access(PublicReadAccess::new(Arc::clone(settings)));
    builder.build()
}

/// Build the **admin panel** service. The [`AdminRoute`] matches every request, so
/// the whole port is the panel; the S3 backend is reached only through the panel's
/// own handlers. Requires credentials (callers gate on [`Config::admin_active`]).
pub fn build_admin_service(config: &Config, fs: Arc<FileSystem>, settings: &SharedSettings) -> S3Service {
    let mut builder = S3ServiceBuilder::new(SharedFileSystem::new(Arc::clone(&fs)));
    let state = Arc::new(AdminState::new(fs, config, Arc::clone(settings)));
    builder.set_route(AdminRoute::new(state));
    builder.build()
}

/// Accept connections and serve `service` until `shutdown` resolves, then drain
/// in-flight connections (up to 10s).
///
/// Generic over the service so it accepts both a bare [`S3Service`] and the
/// [`CorsService`]-wrapped public endpoint; both yield an [`HttpResponse`].
pub async fn serve<S>(
    service: S,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send,
) -> io::Result<()>
where
    S: Service<Request<Incoming>, Response = HttpResponse, Error = HttpError> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
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
///
/// Three single-purpose listeners share one backend: the authenticated S3 API
/// ([`Config::port`]), the public read-only endpoint ([`Config::public_port`]), and
/// — when [`Config::admin_active`] — the admin panel ([`Config::admin_port`]).
pub async fn run(config: Config) -> io::Result<()> {
    let fs = open_backend(&config)?;
    let settings = SettingsStore::open(fs.root())?;

    let api = build_api_service(&config, Arc::clone(&fs), &settings);
    let api_listener = TcpListener::bind((config.host.as_str(), config.port)).await?;
    info!("API listening on http://{}", api_listener.local_addr()?);

    // The public endpoint is wrapped in the CORS layer so cross-origin reads
    // (fonts and other CORS-gated subresources) get the configured
    // `Access-Control-Allow-Origin` header.
    let public = CorsService::new(
        build_public_service(&config, Arc::clone(&fs), &settings),
        Arc::clone(&settings),
    );
    let public_listener = TcpListener::bind((config.host.as_str(), config.public_port)).await?;
    info!("public endpoint listening on http://{}", public_listener.local_addr()?);

    // One shutdown signal fanned out to every listener; each waits on its own clone.
    let (tx, rx) = tokio::sync::watch::channel(());
    let shutdown = || {
        let mut rx = rx.clone();
        async move {
            let _ = rx.changed().await;
        }
    };
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = tx.send(());
    });

    info!("data root: {}", config.root.display());

    let admin_listener = if config.admin_active() {
        let listener = TcpListener::bind((config.host.as_str(), config.admin_port)).await?;
        info!("admin panel listening on http://{}", listener.local_addr()?);
        Some(listener)
    } else {
        if config.admin_enabled {
            warn!("admin panel requested but disabled: no credentials configured");
        }
        None
    };

    match admin_listener {
        Some(admin_listener) => {
            let admin = build_admin_service(&config, Arc::clone(&fs), &settings);
            tokio::try_join!(
                serve(api, api_listener, shutdown()),
                serve(public, public_listener, shutdown()),
                serve(admin, admin_listener, shutdown()),
            )?;
        }
        None => {
            tokio::try_join!(
                serve(api, api_listener, shutdown()),
                serve(public, public_listener, shutdown()),
            )?;
        }
    }

    Ok(())
}
