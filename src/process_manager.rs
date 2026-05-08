//! Process Manager implementation for `Nimi`
//!
//! Can take a rust represntation of some `NixOS` modular services
//! and runs them streaming logs back to the original console.

use eyre::{Context, Result};
use futures::future::OptionFuture;
use libmprocs::{ProcConfig, StopSignal, mprocs};
use log::{debug, info};
use std::process::Stdio;
use std::{
    collections::HashMap, env, io::ErrorKind, path::PathBuf, process::ExitStatus, sync::Arc,
};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast;
use tokio::{fs, process::Command, task::JoinSet};
use tokio_util::sync::CancellationToken;

use crate::config::ServiceOrdering;
use crate::ordering;

pub mod service;
pub mod service_manager;
pub mod settings;

pub use service::{Service, ServiceType};
pub use service_manager::ServiceManager;
pub use settings::Settings;

use crate::process_manager::service_manager::{
    ConfigDir, Logger, ServiceError, ServiceManagerOpts,
};
use crate::subreaper::Subreaper;

/// Lifecycle events emitted by services via the broadcast event bus.
#[derive(Clone, Debug)]
pub enum ServiceEvent {
    /// Service process has been successfully spawned
    Spawned(String),
    /// Service is ready (emitted after postStart succeeds for simple, or on successful exit for oneshot)
    Ready(String),
    /// Service process failed with the given exit status
    Failed(String, ExitStatus),
    /// Service was stopped (graceful shutdown)
    Stopped(String),
    /// Service is being restarted
    Restarting(String),
}

/// Process Manager Struct
///
/// Responsible for starting the services and streaming their outputs to the console
pub struct ProcessManager {
    services: HashMap<String, Service>,
    settings: Settings,
    ordering: HashMap<String, ServiceOrdering>,
    event_tx: broadcast::Sender<ServiceEvent>,
}

impl ProcessManager {
    /// Create a new process manager instance
    pub fn new(
        services: HashMap<String, Service>,
        settings: Settings,
        ordering: HashMap<String, ServiceOrdering>,
    ) -> Self {
        Self {
            services,
            settings,
            ordering,
            event_tx: broadcast::Sender::new(32),
        }
    }

    /// Validate that the ordering config is consistent with the service set.
    ///
    /// Checks that every name referenced in `ordering` (both keys and `after`
    /// entries) corresponds to an actual service, and that the dependency graph
    /// is acyclic.
    pub fn validate_ordering(&self) -> Result<()> {
        for (name, order) in &self.ordering {
            eyre::ensure!(
                self.services.contains_key(name),
                "ordering references unknown service: {name}"
            );
            for dep in &order.after {
                eyre::ensure!(
                    self.services.contains_key(dep),
                    "ordering.{name}.after references unknown service: {dep}"
                );
            }
            for dep in &order.before {
                eyre::ensure!(
                    self.services.contains_key(dep),
                    "ordering.{name}.before references unknown service: {dep}"
                );
            }
        }

        let unit_table = self.build_unit_table();

        ordering::sanity_check_dependencies(&unit_table)
            .map_err(|e| eyre::eyre!("Dependency cycle detected: {}", e))
    }

    fn build_unit_table(&self) -> HashMap<ordering::UnitId, ordering::Dependencies> {
        use ordering::{Dependencies, UnitId};

        let mut unit_table: HashMap<UnitId, Dependencies> = HashMap::new();

        for name in self.services.keys() {
            unit_table.insert(UnitId::new(name), Dependencies::default());
        }

        for (name, order) in &self.ordering {
            let deps = unit_table.entry(UnitId::new(name)).or_default();

            for dep in &order.after {
                deps.after.push(UnitId::new(dep));
            }
            for dep in &order.before {
                deps.before.push(UnitId::new(dep));
            }
        }

        unit_table
    }

    async fn run_startup_process(&self, bin: &str, cancel_tok: &CancellationToken) -> Result<()> {
        let mut set = JoinSet::new();

        let _pause = Subreaper::pause_reaping();
        let mut process = Command::new(bin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .wrap_err_with(|| format!("Failed to spawn startup binary: {:?}", bin))?;
        let _child_guard =
            Subreaper::track_child(process.id()).wrap_err("Failed to track startup child")?;

        let name = Arc::new("startup".to_owned());
        let logs_dir = Arc::from(None);

        Logger::Stdout.start(
            &mut process.stdout,
            Arc::clone(&name),
            Arc::clone(&logs_dir),
            &mut set,
        )?;
        Logger::Stderr.start(
            &mut process.stderr,
            Arc::clone(&name),
            Arc::clone(&logs_dir),
            &mut set,
        )?;

        tokio::select! {
            _ = cancel_tok.cancelled() => {
                debug!(target: &name, "Received shutdown signal");
                ServiceManager::shutdown_process(&mut process, self.settings.restart.time).await?;
            }
            status = process.wait() => {
                let status = status.wrap_err("Failed to get process status")?;
                eyre::ensure!(
                    status.success(),
                    ServiceError::ProcessExited { status }
                );
            }
        }

        set.join_all().await.into_iter().collect()
    }

    /// Create logs dir
    ///
    /// Creates the logs directory for the process manager
    /// to have it's services create textual log files in
    pub async fn create_logs_dir(logs_path: &str) -> Result<PathBuf> {
        let cwd = env::current_dir()?;

        let target = cwd.join(logs_path);

        let mut logs_no = 0;
        loop {
            let sub_dir = target.join(format!("logs-{logs_no}"));
            logs_no += 1;

            match fs::create_dir_all(&sub_dir).await {
                Ok(()) => return Ok(sub_dir),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(e).wrap_err_with(|| {
                        format!("Failed to create logs dir: {}", sub_dir.to_string_lossy())
                    });
                }
            };
        }
    }

    /// Spawn Child Processes
    ///
    /// Spawns every service this process manager manages into a `JoinSet`,
    /// respecting `ordering` constraints. Services wait for their `after`
    /// dependencies to have spawned before starting.
    pub async fn spawn_child_processes(
        self,
        cancel_tok: &CancellationToken,
    ) -> Result<JoinSet<Result<()>>> {
        self.validate_ordering()?;

        let mut join_set = tokio::task::JoinSet::new();

        let settings = Arc::new(self.settings);
        let logs_dir = Arc::new(
            OptionFuture::from(
                settings
                    .logging
                    .logs_dir
                    .as_deref()
                    .map(Self::create_logs_dir),
            )
            .await
            .transpose()?,
        );
        let tmp_dir = Arc::new(env::temp_dir());

        let after_deps: HashMap<String, Vec<String>> = self
            .services
            .keys()
            .map(|name| {
                let deps = self
                    .ordering
                    .get(name)
                    .map(|o| o.after.clone())
                    .unwrap_or_default();
                (name.clone(), deps)
            })
            .collect();

        for (name, service) in self.services {
            let deps = after_deps.get(&name).cloned().unwrap_or_default();
            let cancel = cancel_tok.clone();
            let event_tx = self.event_tx.clone();
            let mut event_rx = self.event_tx.subscribe();

            let opts = ServiceManagerOpts {
                logs_dir: Arc::clone(&logs_dir),
                tmp_dir: Arc::clone(&tmp_dir),

                settings: Arc::clone(&settings),

                name: Arc::new(name.clone()),
                service,
                cancel_tok: cancel_tok.clone(),
                event_tx: event_tx.clone(),
            };

            join_set.spawn(async move {
                // Wait for all `after` dependencies to be satisfied.
                // For simple deps: satisfied when they are spawned.
                // For oneshot deps: satisfied when they complete (Ready or Failed).
                let mut satisfied: HashMap<String, bool> =
                    deps.iter().map(|d| (d.clone(), false)).collect();

                loop {
                    let all_done = satisfied.values().all(|v| *v);
                    if all_done { 
                        break;
                    }

                    tokio::select! {
                        event = event_rx.recv() => {
                            let event = event.map_err(|e| eyre::eyre!("event channel closed: {e}"))?;
                            match event {
                                ServiceEvent::Spawned(ref dep) => {
                                    // Simple deps are satisfied on spawn
                                    if let Some(sat) = satisfied.get_mut(dep) {
                                        *sat = true;
                                    }
                                }
                                ServiceEvent::Ready(ref dep) | ServiceEvent::Failed(ref dep, _) => {
                                    // Oneshot deps are satisfied on completion
                                    if let Some(sat) = satisfied.get_mut(dep) {
                                        *sat = true;
                                    }
                                }
                                _ => {}
                            }
                        }
                        _ = cancel.cancelled() => return Ok(()),
                    }
                }

                let mut manager = ServiceManager::new(opts).await?;
                manager.run().await
            });
        }

        Ok(join_set)
    }

    fn spawn_shutdown_task(&self, cancel_tok: &CancellationToken) {
        let token = cancel_tok.clone();
        tokio::spawn(async move {
            let mut sigterm =
                signal(SignalKind::terminate()).wrap_err("Failed to register SIGTERM handler")?;
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = sigterm.recv() => {},
            }
            token.cancel();
            Ok::<_, eyre::Report>(())
        });
    }

    /// Run the services defined for the process manager instance
    ///
    /// Terminates on `Ctrl-C`
    pub async fn run(self) -> Result<()> {
        info!("Starting process manager...");

        let cancel_tok = CancellationToken::new();
        self.spawn_shutdown_task(&cancel_tok);

        if let Some(startup) = &self.settings.startup.run_on_startup {
            info!("Running startup binary ({})...", startup);
            self.run_startup_process(startup, &cancel_tok)
                .await
                .wrap_err("Failed to run startup process")?;
        }

        let mut services_set = self.spawn_child_processes(&cancel_tok).await?;

        while let Some(res) = services_set.join_next().await {
            let flat: Result<()> = res.map_err(Into::into).and_then(std::convert::identity);

            if let Err(e) = flat {
                cancel_tok.cancel();
                return Err(e);
            }
        }

        info!("Shutting down process manager...");

        Ok(())
    }

    /// Run the services defined for the process manager instance
    /// in mprocs
    pub async fn run_mprocs(self) -> Result<()> {
        info!("Starting process manager...");

        let cancel_tok = CancellationToken::new();
        self.spawn_shutdown_task(&cancel_tok);

        if let Some(startup) = &self.settings.startup.run_on_startup {
            info!("Running startup binary ({})...", startup);
            self.run_startup_process(startup, &cancel_tok)
                .await
                .wrap_err("Failed to run startup process")?;
        }

        let tmp_dir = env::temp_dir();

        for (name, service) in &self.services {
            ConfigDir::new(&tmp_dir, &service.config_data)
                .await
                .wrap_err_with(|| format!("Failed to create config dir for {}", name))?;
        }

        mprocs::run_with_config(self.into(), libmprocs::Settings::default())
            .await
            .map_err(|e| eyre::eyre!("{e:?}"))
            .wrap_err("Failed to launch mprocs TUI")
    }
}

impl From<ProcessManager> for Vec<ProcConfig> {
    fn from(value: ProcessManager) -> Self {
        value
            .services
            .into_iter()
            .map(|(name, service)| {
                let deps = value
                    .ordering
                    .get(&name)
                    .map(|o| o.after.clone())
                    .unwrap_or_default();

                ProcConfig {
                    name,
                    cmd: service.process.into(),
                    cwd: std::env::current_dir().ok().map(|p| p.into_os_string()),
                    env: None,
                    autostart: true,
                    autorestart: value.settings.autorestart(),

                    stop: StopSignal::SIGTERM,

                    deps,

                    mouse_scroll_speed: 5,
                    scrollback_len: 1000,
                    log: None,
                }
            })
            .collect()
    }
}
