use bollard::container::{LogsOptions, RestartContainerOptions};
use bollard::secret::ContainerSummary;
use bollard::Docker;
use clap::Parser;
use futures_util::StreamExt;
use log::{debug, error, info};
use std::collections::{HashMap, HashSet};
use std::process::exit;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::{sleep, Duration};

#[derive(Parser, Debug)]
#[command(
    author = "Zachary Williams",
    about = "Monitor containers and restart on log patterns."
)]
struct Args {
    /// Containers to watch
    #[arg(long, short, value_name = "CONTAINER", required = true, action = clap::ArgAction::Append)]
    watch: Vec<String>,

    /// Containers to restart (comma-separated)
    #[arg(long, short, value_name = "CONTAINER", required = true, action = clap::ArgAction::Append)]
    restart: Vec<String>,

    /// Patterns to watch for (comma-separated)
    #[arg(long, short, value_name = "PATTERN", required = true, action = clap::ArgAction::Append)]
    pattern: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let args = Args::parse();

    debug!("Raw clap args: {args:?}");

    if args.watch.len() != args.restart.len() || (args.watch.len() != args.pattern.len()) {
        panic!("Invalid args. Expected format: '--watch <container> --restart [container] --pattern [pattern]'")
    }

    let configs: Vec<ContainerRestartConfig> = args
        .watch
        .iter()
        .zip(args.restart.iter())
        .zip(args.pattern.iter())
        .map(|((watch, restart_raw), pattern_raw)| {
            let restart: Vec<String> = restart_raw.split(',').map(|s| s.to_string()).collect();
            let pattern: Vec<String> = pattern_raw.split(',').map(|s| s.to_string()).collect();

            ContainerRestartConfig {
                watch: watch.to_string(),
                restart,
                pattern,
            }
        })
        .collect();

    debug!("Parsed configs: {configs:?}");

    let docker = match Docker::connect_with_socket_defaults() {
        Ok(docker) => docker,
        Err(err) => {
            error!("Failed to connect to Docker with error: {err}");
            exit(1);
        }
    };

    info!("Connected to Docker. Beginning to monitor logs...");

    let mut tasks = HashMap::new();
    let notify_restart = Arc::new(Notify::new());

    loop {
        let containers_names: Vec<_> = configs.iter().map(|c| c.watch.clone()).collect();
        let containers_result = get_filtered_containers(&docker, &containers_names)
            .await
            .and_then(|containers| {
                Ok(containers
                    .iter()
                    .map(|container| {
                        let config = configs
                            .iter()
                            .find(|config| config.watch == container.name)
                            .unwrap();

                        MappedContainer {
                            id: container.id.clone(),
                            name: container.name.clone(),
                            restart: config.restart.clone(),
                            pattern: config.pattern.clone(),
                        }
                    })
                    .collect::<Vec<MappedContainer>>())
            });

        let containers = match containers_result {
            Ok(cons) => cons,
            Err(err) => {
                error!("Failed to get containers: {}", err);
                info!("Will sleep and try again...");

                sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        for container in &containers {
            if !tasks.contains_key(&container.id) {
                let task_container = container.clone();
                let container_id = container.id.clone();
                let container_name = container.name.clone();
                let docker_clone = docker.clone();
                let notify_restart_clone = notify_restart.clone();

                info!("[{container_name}] Monitoring logs w/ id: {container_id}");

                let task_handle = tokio::spawn(async move {
                    if let Err(e) =
                        monitor_logs(&docker_clone, &task_container, notify_restart_clone).await
                    {
                        error!("[{container_name}] Error monitoring logs for {container_id}: {e}");
                    }
                });

                tasks.insert(container.id.clone(), task_handle);
            }
        }

        // Remove tasks for containers no longer running
        let current_containers: HashSet<_> = containers.iter().map(|c| c.id.clone()).collect();
        let stale_containers: Vec<_> = tasks
            .keys()
            .filter(|id| !current_containers.contains(*id))
            .cloned()
            .collect();

        for stale_id in stale_containers {
            if let Some(task) = tasks.remove(&stale_id) {
                task.abort();
            }
        }

        // Wait for notifications about restarts or a periodic delay
        tokio::select! {
            _ = notify_restart.notified() => {}
            _ = sleep(Duration::from_secs(10)) => {}
        }
    }
}

#[derive(Debug)]
struct Container {
    id: String,
    name: String,
}

#[derive(Debug)]

struct ContainerRestartConfig {
    watch: String,
    restart: Vec<String>,
    pattern: Vec<String>,
}

#[derive(Debug, Clone)]
struct MappedContainer {
    id: String,
    name: String,
    restart: Vec<String>,
    pattern: Vec<String>,
}

async fn get_running_containers(
    docker: &Docker,
) -> Result<Vec<ContainerSummary>, Box<dyn std::error::Error>> {
    let mut filter = HashMap::new();
    filter.insert("status".to_string(), vec!["running".to_string()]);

    let containers = docker
        .list_containers(Some(bollard::container::ListContainersOptions {
            all: true,
            filters: filter,
            ..Default::default()
        }))
        .await?;

    debug!("All containers: {containers:?}");

    Ok(containers)
}

async fn get_filtered_containers(
    docker: &Docker,
    container_names: &Vec<String>,
) -> Result<Vec<Container>, Box<dyn std::error::Error>> {
    let containers = get_running_containers(docker).await?;

    let filtered_containers = containers
        .into_iter()
        .filter_map(|c| match (c.names, c.id) {
            (Some(names), Some(id)) => {
                let name = names[0].trim_start_matches("/").to_string();
                match container_names.contains(&name) {
                    true => Some(Container { id, name }),
                    false => None,
                }
            }
            _ => None,
        })
        .collect();

    debug!("Filtered containers: {filtered_containers:?}");

    Ok(filtered_containers)
}

async fn monitor_logs(
    docker: &Docker,
    container: &MappedContainer,
    notify_restart: Arc<Notify>,
) -> Result<(), Box<dyn std::error::Error>> {
    let container_id = &container.id;
    let container_name = &container.name;
    let mut log_stream = docker.logs(
        &container.id,
        Some(LogsOptions::<String> {
            stdout: true,
            stderr: true,
            follow: true,
            ..Default::default()
        }),
    );

    while let Some(log_result) = log_stream.next().await {
        match log_result {
            Ok(log) => {
                let log_output = log.to_string();
                debug!("[{container_name} - {container_id}] LOG: {log_output}");

                for pattern in &container.pattern {
                    if log_output.contains(pattern) {
                        info!("[{container_name}] RESTARTING: '{pattern}' detected in '{log_output}'.");
                        restart_container(docker, container).await?;
                        notify_restart.notify_waiters(); // Notify the main loop about the restart
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                error!("[{container_name}] Failed to read logs: {e}");
                break;
            }
        }
    }

    Ok(())
}

async fn restart_container(
    docker: &Docker,
    container: &MappedContainer,
) -> Result<(), Box<dyn std::error::Error>> {
    let containers = get_filtered_containers(docker, &container.restart).await?;
    for container in containers {
        docker
            .restart_container(&container.id, None::<RestartContainerOptions>)
            .await?;
    }
    info!("[{}] Successfully restarted container", container.name);
    Ok(())
}
