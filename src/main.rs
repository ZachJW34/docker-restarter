use bollard::container::{LogsOptions, RestartContainerOptions};
use bollard::secret::ContainerSummary;
use bollard::Docker;
use clap::Parser;
use futures_util::StreamExt;
use log::{debug, error, info};
use std::collections::HashMap;
use std::future::IntoFuture;
use std::process::exit;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::task::JoinHandle;
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

    /// Number of occurrences before a restart is triggered
    #[arg(long = "skip-first", short, value_name = "BOOLEAN", required = true, action = clap::ArgAction::Append)]
    skip_first: Vec<bool>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let args = Args::parse();

    debug!("Raw clap args: {args:?}");

    let watch_count = args.watch.len();

    if (watch_count != args.restart.len())
        || (watch_count != args.pattern.len())
        || (watch_count != args.skip_first.len())
    {
        panic!("Invalid args. Expected format: '--watch <container> --restart [container] --pattern [pattern] --skip-first [boolean]'.\nThe number of --watch, --restart, --pattern and --skip-first should be symmetrical.")
    }

    let configs: Vec<ContainerRestartConfig> = args
        .watch
        .iter()
        .zip(args.restart.iter())
        .zip(args.pattern.iter())
        .zip(args.skip_first.iter())
        .map(|(((watch, restart_raw), pattern), skip_first)| {
            let restart: Vec<String> = restart_raw.split(',').map(|s| s.to_string()).collect();

            ContainerRestartConfig {
                watch: watch.to_string(),
                restart,
                pattern: pattern.to_string(),
                skip_first: *skip_first,
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

    let mut tasks: HashMap<String, JoinHandle<()>> = HashMap::new();
    let mut containers: HashMap<String, MappedContainer> = HashMap::new();

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

                        (
                            container.id.clone(),
                            MappedContainer {
                                id: container.id.clone(),
                                name: container.name.clone(),
                                restart: config.restart.clone(),
                                pattern: config.pattern.clone(),
                                skip_first: config.skip_first,
                            },
                        )
                    })
                    .collect::<HashMap<String, MappedContainer>>())
            });

        let new_containers = match containers_result {
            Ok(cons) => cons,
            Err(err) => {
                error!("Failed to get containers: {}", err);
                info!("Will sleep and try again...");

                sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        for id in containers.keys() {
            if !new_containers.contains_key(id) {
                if let Some(task) = tasks.remove(id) {
                    task.abort();
                }
            }
        }

        containers = new_containers;

        for container in containers.values() {
            if !tasks.contains_key(&container.id) {
                let task_container = container.clone();
                let container_id = container.id.clone();
                let container_name = container.name.clone();
                let docker_clone = docker.clone();

                info!("[{container_name}] Monitoring logs...");

                let task_handle = tokio::spawn(async move {
                    if let Err(e) = monitor_logs(&docker_clone, &task_container).await {
                        error!("[{container_name}] Error monitoring logs for {container_id}: {e}");
                    }
                });

                tasks.insert(container.id.clone(), task_handle);
            }
        }

        sleep(Duration::from_secs(10)).into_future().await
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
    pattern: String,
    skip_first: bool,
}

#[derive(Debug, Clone)]
struct MappedContainer {
    id: String,
    name: String,
    restart: Vec<String>,
    pattern: String,
    skip_first: bool,
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
        .filter_map(|c| {
            return match (c.names, c.id) {
                (Some(names), Some(id)) => {
                    let name = names[0].trim_start_matches("/").to_string();
                    match container_names.contains(&name) {
                        true => Some(Container { id, name }),
                        false => None,
                    }
                }
                _ => None,
            };
        })
        .collect();

    debug!("Filtered containers: {filtered_containers:?}");

    Ok(filtered_containers)
}

async fn monitor_logs(
    docker: &Docker,
    container: &MappedContainer,
    // restart_tx: Sender<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let MappedContainer {
        id,
        name,
        pattern,
        skip_first,
        ..
    } = container;
    let mut first_time = true;
    let mut since = now() - 10;
    let mut log_stream;

    loop {
        log_stream = docker.logs(
            id,
            Some(LogsOptions::<String> {
                stdout: true,
                stderr: true,
                follow: true,
                since,
                ..Default::default()
            }),
        );

        while let Some(log_result) = log_stream.next().await {
            match log_result {
                Ok(log) => {
                    let log_output = log.to_string();
                    debug!("[{name}] New line: '{log_output}'");

                    if log_output.contains(pattern) {
                        info!("[{name}] Pattern detected (first_time={first_time}): '{pattern}' -> '{log_output}'");
                        if !skip_first || !first_time {
                            info!("[{name}] Restarting container: '{pattern}' detected in '{log_output}'");
                            restart_containers(docker, container).await?;
                            info!("[{name}] Successfully restarted container");

                            since = now();
                            break;
                        }
                    }
                    first_time = false;
                }
                Err(e) => {
                    error!("[{name}] Failed to read logs: {e}");
                    break;
                }
            }
        }
    }
}

async fn restart_containers(
    docker: &Docker,
    container: &MappedContainer,
) -> Result<(), Box<dyn std::error::Error>> {
    let containers = get_filtered_containers(docker, &container.restart).await?;
    for container in containers {
        docker
            .restart_container(&container.id, None::<RestartContainerOptions>)
            .await?;
    }
    Ok(())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_secs() as i64
}
