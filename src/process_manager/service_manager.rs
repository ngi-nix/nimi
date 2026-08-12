//! Service Manager Module
//!
//! Contains items useful for spawning and managing the actual processes associated with a
//! `Service`

use std::{
    path::PathBuf,
    process::{ExitStatus, Stdio},
    sync::Arc,
};

use eyre::{Context, Result};
use log::{debug, info};
use thiserror::Error;
use tokio::sync::broadcast;
use tokio::time::timeout as tokio_timeout;
use tokio::{
    process::{Child, Command},
    task::JoinSet,
};

pub mod config_dir;
pub mod logger;

pub use config_dir::ConfigDir;
pub use logger::Logger;
use tokio_util::sync::CancellationToken;

use crate::process_manager::{Service, ServiceEvent, ServiceType, Settings, settings::RestartMode};
use crate::subreaper::{ChildGuard, Subreaper};

/// Responsible for the running of and managing of service state
pub struct ServiceManager {
    settings: Arc<Settings>,
    cancel_tok: CancellationToken,

    name: Arc<String>,
    service: Service,

    current_restart_count: usize,

    config_dir: ConfigDir,
    logs_dir: Arc<Option<PathBuf>>,

    event_tx: broadcast::Sender<ServiceEvent>,
}

/// Errors which can occur during service management
#[derive(Error, Debug)]
pub enum ServiceError {
    /// Error for when the process exits with a non zero exit code
    #[error("Service exited with status - {status:?}")]
    ProcessExited {
        /// Exit status
        status: ExitStatus,
    },
}

/// Used to initialize the Service Manager in a structured manner
pub struct ServiceManagerOpts {
    /// Directory to store logs in
    pub logs_dir: Arc<Option<PathBuf>>,
    /// Temporary directory
    pub tmp_dir: Arc<PathBuf>,

    /// Process manager settings
    pub settings: Arc<Settings>,

    /// Service name
    pub name: Arc<String>,

    /// Service config
    pub service: Service,

    /// Cancellation token
    pub cancel_tok: CancellationToken,

    /// Event bus for lifecycle signals
    pub event_tx: broadcast::Sender<ServiceEvent>,
}

impl ServiceManager {
    /// Creates a new Service Manager
    ///
    /// This creates the corresponding processes and supervises the operation for a given
    /// `Service`.
    ///
    /// This also produces a `ConfigDir` instance per service.
    pub async fn new(opts: ServiceManagerOpts) -> Result<Self> {
        Ok(Self {
            config_dir: ConfigDir::new(&opts.tmp_dir, &opts.service.config_data).await?,

            settings: opts.settings,
            cancel_tok: opts.cancel_tok,

            name: opts.name,
            service: opts.service,

            current_restart_count: 0,
            logs_dir: opts.logs_dir,
            event_tx: opts.event_tx,
        })
    }

    /// Run the `Service` managed by this `ServiceManager`
    ///
    /// This will handle restarts, attach logging processes and manage linking the config
    /// directory.
    pub async fn run(&mut self) -> Result<()> {
        if matches!(self.service.service_type, ServiceType::Oneshot) {
            return self.run_oneshot().await;
        }

        while let Err(e) = self.spawn_service_process().await {
            match e.downcast_ref() {
                Some(ServiceError::ProcessExited { status }) => {
                    info!("Process {} exited with status {}", self.name, status)
                }
                None => return Err(e),
            }

            match self.settings.restart.mode {
                RestartMode::Always => info!("restarting (mode: always)"),
                RestartMode::UpToCount => {
                    if self.current_restart_count >= self.settings.restart.count {
                        info!(
                            "Not restarting (mode: up-to-count {}/{})",
                            self.current_restart_count, self.settings.restart.count
                        );
                        break;
                    }

                    self.current_restart_count += 1;

                    info!(
                        "Restarting (mode: up-to-count {}/{})",
                        self.current_restart_count, self.settings.restart.count
                    );
                }
                RestartMode::Never => {
                    info!("Not restarting (mode: never)");

                    break;
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(self.settings.restart.time) => {},
                _ = self.cancel_tok.cancelled() => {
                    info!("Received shutdown during restart delay for {}", self.name);
                    break;
                }
            }
        }

        Ok(())
    }

    async fn run_oneshot(&mut self) -> Result<()> {
        info!(target: &self.name, "Running oneshot service");
        let result = self.spawn_service_process().await;

        match result {
            Ok(()) => {
                let _ = self
                    .event_tx
                    .send(ServiceEvent::Ready(self.name.to_string()));
            }
            Err(e) => {
                use std::os::unix::process::ExitStatusExt;
                let status = e
                    .downcast_ref::<ServiceError>()
                    .map(|se| {
                        let ServiceError::ProcessExited { status } = se;
                        *status
                    })
                    .unwrap_or_else(|| ExitStatusExt::from_raw(1));
                let _ = self
                    .event_tx
                    .send(ServiceEvent::Failed(self.name.to_string(), status));
                return Err(e);
            }
        }

        Ok(())
    }

    /// Spawn a process with common setup
    ///
    /// Handles Subreaper pausing, env vars, stdout/stderr piping,
    /// and child tracking.
    async fn spawn_process(
        &self,
        binary: &str,
        args: &[&str],
        error_context: &str,
    ) -> Result<(Child, ChildGuard)> {
        let _pause = Subreaper::pause_reaping();
        let mut cmd = Command::new(binary);
        if !args.is_empty() {
            cmd.args(args);
        }
        let child = cmd
            .env("XDG_CONFIG_HOME", &self.config_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .wrap_err_with(|| error_context.to_string())?;
        let guard = Subreaper::track_child(child.id()).wrap_err("Failed to track child")?;
        Ok((child, guard))
    }

    /// Attach loggers to a process and wait for completion.
    async fn run_with_loggers(&self, mut process: Child) -> Result<()> {
        let mut set = JoinSet::new();

        Logger::Stdout.start(
            &mut process.stdout,
            Arc::clone(&self.name),
            Arc::clone(&self.logs_dir),
            &mut set,
        )?;
        Logger::Stderr.start(
            &mut process.stderr,
            Arc::clone(&self.name),
            Arc::clone(&self.logs_dir),
            &mut set,
        )?;

        tokio::select! {
            _ = self.cancel_tok.cancelled() => {
                debug!(target: &self.name, "Received shutdown signal");
                Self::shutdown_process(&mut process, self.settings.restart.time).await?;
            }
            status = process.wait() => {
                let status = status.wrap_err("Failed to get process status")?;
                eyre::ensure!(status.success(), ServiceError::ProcessExited { status });
            }
        }

        set.join_all().await.into_iter().collect()
    }

    /// Runs the pre-start script for this service, if configured.
    async fn run_pre_start(&self, bin: &str) -> Result<()> {
        let error_ctx = format!(
            "Failed to spawn pre-start binary for {}: {:?}",
            self.name, bin
        );
        let (process, _guard) = self.spawn_process(bin, &[], &error_ctx).await?;
        self.run_with_loggers(process).await
    }

    /// Spawns a service process
    pub async fn spawn_service_process(&mut self) -> Result<()> {
        if let Some(pre_start) = &self.service.pre_start {
            info!(target: &self.name, "Running pre-start script ({})", pre_start);
            self.run_pre_start(pre_start).await?;
        }

        let (process, _guard) = self.create_service_child().await?;
        let _guard = Arc::new(_guard);

        let _ = self
            .event_tx
            .send(ServiceEvent::Spawned(self.name.to_string()));

        if let Some(post_start) = &self.service.post_start {
            info!(target: &self.name, "Running post-start script ({})", post_start);
            let post_start = post_start.clone();
            let config_dir_path = self.config_dir.path().clone();
            let event_tx = self.event_tx.clone();
            let name = self.name.to_string();
            let post_start_handle = tokio::spawn(async move {
                let result = Self::run_post_start(&post_start, config_dir_path).await;
                if result.is_ok() {
                    let _ = event_tx.send(ServiceEvent::Ready(name));
                }
                result
            });

            self.run_with_loggers(process).await?;
            post_start_handle.await??;
        } else {
            let _ = self
                .event_tx
                .send(ServiceEvent::Ready(self.name.to_string()));
            self.run_with_loggers(process).await?;
        }

        Ok(())
    }

    async fn run_post_start(bin: &str, config_dir: PathBuf) -> Result<()> {
        let config_dir = Arc::new(config_dir);
        let mut cmd = Command::new(bin);
        cmd.env("XDG_CONFIG_HOME", config_dir.as_ref())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .wrap_err_with(|| format!("Failed to spawn post-start script: {:?}", bin))?;
        let status = child
            .wait()
            .await
            .wrap_err("Failed to wait on post-start script")?;
        if status.success() {
            Ok(())
        } else {
            eyre::bail!(
                "post-start script exited with non-zero status: {:?}",
                status
            );
        }
    }

    /// Kill a service process gracefully
    pub async fn shutdown_process(
        process: &mut Child,
        timeout_duration: std::time::Duration,
    ) -> Result<()> {
        #[cfg(unix)]
        {
            use nix::sys::signal::{Signal, kill};
            use nix::unistd::Pid;

            if let Some(pid) = process.id() {
                let pid = Pid::from_raw(pid as i32);
                let _ = kill(pid, Signal::SIGTERM);
                if tokio_timeout(timeout_duration, process.wait())
                    .await
                    .is_err()
                {
                    let _ = kill(pid, Signal::SIGKILL);
                    let _ = process.wait().await;
                }

                return Ok(());
            }
        }

        process
            .kill()
            .await
            .wrap_err("Failed to kill service process")
    }

    /// Create service child
    ///
    /// Responsible for creating the actual child process for the
    /// service
    pub async fn create_service_child(&self) -> Result<(Child, ChildGuard)> {
        let error_ctx = format!(
            "Failed to start process for service: {:?}",
            self.service.process
        );
        let env: std::collections::HashMap<String, String> = std::env::vars()
            .chain(std::iter::once((
                "XDG_CONFIG_HOME".to_string(),
                std::path::Path::new(&self.config_dir).to_string_lossy().into_owned(),
            )))
            .collect();
        let expanded_args: Vec<String> = self
            .service
            .process
            .argv
            .args()
            .iter()
            .map(|s| {
                shellexpand::full_with_context(s, || None::<String>, |var| {
                    Ok::<_, std::convert::Infallible>(env.get(var).cloned())
                })
                .unwrap()
                .into_owned()
            })
            .collect();
        let args: Vec<&str> = expanded_args.iter().map(|s| s.as_str()).collect();
        for (name, cfg) in &self.service.config_data {
            if cfg.enable {
                info!(target: &**self.name, "Config: {} -> {}", name, cfg.source.display());
            }
        }
        let mut env_vars: Vec<(&String, &String)> = env.iter().collect();
        env_vars.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in env_vars {
            info!(target: &**self.name, "Environment: {}={}", k, v);
        }
        info!(
            target: &**self.name,
            "XDG_CONFIG_HOME={}",
            std::path::Path::new(&self.config_dir).display(),
        );
        info!(
            target: &**self.name,
            "Running: {} {}",
            self.service.process.argv.binary(),
            args.join(" ")
        );
        self.spawn_process(self.service.process.argv.binary(), &args, &error_ctx)
            .await
    }
}
