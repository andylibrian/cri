use crate::{
    config::{Config, LogScope},
    cri_service::CRIService,
    criapi::{
        image_service_server::ImageServiceServer, runtime_service_server::RuntimeServiceServer,
    },
    storage::{default_key_value_storage::DefaultKeyValueStorage, KeyValueStorage},
    unix_stream,
};
use anyhow::{bail, Context, Result};
use clap::crate_name;
use futures_util::stream::TryStreamExt;
use log::{debug, info};
use std::env;
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::{
    fs,
    signal::unix::{signal, SignalKind},
};
use tonic::{transport, Request, Status};

/// Server is the main instance to run the Container Runtime Interface
pub struct Server {
    config: Config,
}

impl Server {
    /// Create a new server instance
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Start a new server with its default values
    pub async fn start(self) -> Result<()> {
        self.set_logging_verbosity()
            .context("set logging verbosity")?;

        // Setup the storage and pass it to the service
        let storage = DefaultKeyValueStorage::open(&self.config.storage_path())?;
        let cri_service = CRIService::new(storage.clone());

        // Build a new socket from the config
        let mut uds = self.unix_domain_listener().await?;

        // Handle shutdown based on signals
        let mut shutdown_terminate = signal(SignalKind::terminate())?;
        let mut shutdown_interrupt = signal(SignalKind::interrupt())?;

        info!(
            "Runtime server listening on {}",
            self.config.sock_path().display()
        );

        tokio::select! {
            res = transport::Server::builder()
                .add_service(RuntimeServiceServer::with_interceptor(cri_service.clone(), Self::intercept))
                .add_service(ImageServiceServer::with_interceptor(cri_service, Self::intercept))
                .serve_with_incoming(uds.incoming().map_ok(unix_stream::UnixStream)) => {
                res.context("run GRPC server")?
            }
            _ = shutdown_interrupt.recv() => {
                info!("Got interrupt signal, shutting down server");
            }
            _ = shutdown_terminate.recv() => {
                info!("Got termination signal, shutting down server");
            }
        }

        self.cleanup(storage)
    }

    /// Create a new UnixListener from the configs socket path.
    async fn unix_domain_listener(&self) -> Result<UnixListener> {
        let sock_path = self.config.sock_path();
        if !sock_path.is_absolute() {
            bail!(
                "specified socket path {} is not absolute",
                sock_path.display()
            )
        }
        if sock_path.exists() {
            fs::remove_file(sock_path)
                .await
                .with_context(|| format!("unable to remove socket file {}", sock_path.display()))?;
        } else {
            let sock_dir = sock_path.parent().context("get socket path directory")?;
            fs::create_dir_all(sock_dir)
                .await
                .with_context(|| format!("create socket dir {}", sock_dir.display()))?;
        }

        Ok(UnixListener::bind(sock_path).context("bind socket from path")?)
    }

    /// Initialize the logger and set the verbosity to the provided level.
    fn set_logging_verbosity(&self) -> Result<()> {
        // Set the logging verbosity via the env
        let level = if self.config.log_scope() == LogScope::Global {
            self.config.log_level().to_string()
        } else {
            format!("{}={}", crate_name!(), self.config.log_level())
        };
        env::set_var("RUST_LOG", level);

        // Initialize the logger
        env_logger::try_init().context("init env logger")
    }

    /// This function will get called on each inbound request, if a `Status`
    /// is returned, it will cancel the request and return that status to the
    /// client.
    fn intercept(req: Request<()>) -> std::result::Result<Request<()>, Status> {
        debug!("{:?}", req);
        Ok(req)
    }

    /// Cleanup the server and persist any data if necessary.
    fn cleanup(self, mut storage: DefaultKeyValueStorage) -> Result<()> {
        debug!("Cleaning up server");
        storage.persist().context("persist storage")?;
        std::fs::remove_file(self.config.sock_path())
            .with_context(|| format!("remove socket path {}", self.config.sock_path().display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigBuilder;
    use tempfile::{tempdir, NamedTempFile};

    #[tokio::test]
    async fn unix_domain_listener_success() -> Result<()> {
        let sock_path = &tempdir()?.path().join("test.sock");
        let config = ConfigBuilder::default().sock_path(sock_path).build()?;
        let sut = Server::new(config);

        assert!(!sock_path.exists());
        sut.unix_domain_listener().await?;
        assert!(sock_path.exists());

        Ok(())
    }

    #[tokio::test]
    async fn unix_domain_listener_success_exists() -> Result<()> {
        let sock_path = NamedTempFile::new()?;
        let config = ConfigBuilder::default()
            .sock_path(sock_path.path())
            .build()?;
        let sut = Server::new(config);

        assert!(sock_path.path().exists());
        sut.unix_domain_listener().await?;
        assert!(sock_path.path().exists());

        Ok(())
    }

    #[tokio::test]
    async fn unix_domain_listener_fail_not_absolute() -> Result<()> {
        let config = ConfigBuilder::default()
            .sock_path("not/absolute/path")
            .build()?;
        let sut = Server::new(config);

        assert!(sut.unix_domain_listener().await.is_err());

        Ok(())
    }
}
