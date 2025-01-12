mod config;
mod loki;

use chrono::{DateTime, Utc};
use clap::Parser;
use eyre::{self, Result, WrapErr};
use futures::StreamExt;
use hyper::{
    header::CONTENT_TYPE,
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server,
};
use itertools::{self, Itertools};
use lazy_static::lazy_static;
use loki::{LokiLogger, LokiRequest, LokiStream};
use prometheus::{opts, register_counter, Counter, Encoder, TextEncoder};
use shiplift::{
    builder::ContainerListOptions, rep::Container as RepContainer, tty::TtyChunk, Docker,
    LogsOptions,
};
use std::{
    collections::{HashMap, HashSet},
    fs,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{self, task::JoinHandle, time::sleep};
use tracing::{error, info, level_filters::LevelFilter};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::{layer::SubscriberExt, Registry};

lazy_static! {
    static ref LOKI_REQUEST_COUNTER: Counter = register_counter!(opts!(
        "docktail_loki_requests",
        "Number of loki requests made."
    ))
    .unwrap();
    static ref LOKI_STREAM_COUNTER: Counter = register_counter!(opts!(
        "docktail_loki_streams",
        "Number of loki streams made. (Loki log lines)"
    ))
    .unwrap();
}

async fn serve_req(_req: Request<Body>) -> Result<Response<Body>, hyper::Error> {
    let encoder = TextEncoder::new();

    let metric_families = prometheus::gather();
    let mut buffer = vec![];
    encoder.encode(&metric_families, &mut buffer).unwrap();

    let response = Response::builder()
        .status(200)
        .header(CONTENT_TYPE, encoder.format_type())
        .body(Body::from(buffer))
        .unwrap();

    Ok(response)
}

async fn prometheus_server() {
    let addr = ([0, 0, 0, 0], 9898).into();
    info!("Prometheus server listening on http://{}", addr);

    let serve_future = Server::bind(&addr).serve(make_service_fn(|_| async {
        Ok::<_, hyper::Error>(service_fn(serve_req))
    }));

    if let Err(err) = serve_future.await {
        error!("server error: {}", err);
    }
}

fn setup_tracing() {
    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();
    if std::env::var("PRODUCTION").is_ok() {
        let subscriber = Registry::default()
            .with(tracing_logfmt::layer())
            .with(env_filter);
        tracing::subscriber::set_global_default(subscriber).unwrap();
    } else {
        let subscriber = Registry::default()
            .with(tracing_subscriber::fmt::layer())
            .with(env_filter);
        tracing::subscriber::set_global_default(subscriber).unwrap();
    }
}

#[derive(Clone)]
struct LogMessage {
    time: DateTime<Utc>,
    container: String,
    message: String,
}

async fn spawn_logger(
    mut receiver: tokio::sync::broadcast::Receiver<LogMessage>,
    tags: HashMap<String, String>,
    target: config::Target,
) {
    tokio::spawn(async move {
        let loki = LokiLogger::new(target.url, target.basic_auth);

        loop {
            let mut logs = vec![];
            loop {
                match receiver.try_recv() {
                    Ok(m) => {
                        LOKI_STREAM_COUNTER.inc();
                        logs.push(m)
                    }
                    Err(_) => {
                        // Either disconnect or empty. In both cases, we are done retrieving streams.
                        break;
                    }
                }
            }
            let mut loki_request = LokiRequest { streams: vec![] };
            logs.sort_by_key(|log| log.container.clone());

            for (container_name, chunk) in &logs.into_iter().chunk_by(|log| log.container.clone()) {
                let mut labels = HashMap::<String, String>::new();
                labels.insert("container".into(), container_name);
                for (key, value) in tags.iter() {
                    labels.insert(key.clone(), value.clone());
                }
                loki_request.streams.push(LokiStream {
                    stream: labels,
                    values: chunk
                        .map(|message| {
                            let time = message
                                .time
                                .timestamp_nanos_opt()
                                .unwrap_or_default()
                                .to_string();
                            [time, message.message]
                        })
                        .collect(),
                });
            }

            let num_requests = loki_request.streams.len();
            if num_requests > 0 {
                LOKI_REQUEST_COUNTER.inc();
                match loki.log(loki_request).await {
                    Ok(_) => (),
                    Err(e) => error!("Failed to send message to loki: {:?}", e),
                }
            }

            sleep(Duration::from_millis(500)).await;
        }
    });
}

#[derive(Parser)]
struct Cli {
    #[arg(long)]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();
    let config = fs::read_to_string(args.config)?;
    let config: config::Config = serde_json::from_str(&config)?;
    setup_tracing();

    tokio::spawn(async { prometheus_server().await });

    let start_time: DateTime<Utc> = Utc::now();
    let docker = Docker::new();
    info!("Starting docktail...");

    let (sender, receiver) = tokio::sync::broadcast::channel(100);

    let tags = config.tags.clone();
    for target in config.targets.into_iter() {
        spawn_logger(sender.subscribe(), tags.clone(), target).await;
    }
    drop(receiver);

    let containers_set = Arc::new(Mutex::new(HashSet::<String>::new()));

    info!("Looking for containers...");
    loop {
        let containers = docker
            .containers()
            .list(&ContainerListOptions::builder().all().build())
            .await?;

        let containers = {
            let set = containers_set.clone();
            let set = set.lock().unwrap();
            containers
                .into_iter()
                .filter(|x| x.state == "running" && !set.contains(&x.id))
                .collect::<Vec<_>>()
        };

        for container_rep in containers {
            spawn_job(
                docker.clone(),
                container_rep.clone(),
                containers_set.clone(),
                start_time,
                sender.clone(),
            );
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

fn container_is_running_this_process(container_rep: &RepContainer) -> bool {
    // docker containers have a HOSTNAME environment that is a substring of the
    // container id.
    let hostname = std::env::var("HOSTNAME").unwrap_or("no hostname".to_string());
    container_rep.id.contains(&hostname)
}

fn spawn_job(
    docker: Docker,
    container_rep: RepContainer,
    set: Arc<Mutex<HashSet<String>>>,
    start_time: DateTime<Utc>,
    sender: tokio::sync::broadcast::Sender<LogMessage>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let container = docker.containers().get(&container_rep.id);

        // If the container is newer than this program, then we should grab all
        // log lines, so that we don't miss anything. However, for older
        // containers, it is more likely that it was this program that was
        // restarted, so we shouldn't add duplicated lines.
        let tail = if container_rep.created > start_time {
            "all"
        } else if container_is_running_this_process(&container_rep) {
            // If this is docktail, then we want all the logs.
            "all"
        } else {
            "0"
        };

        info!(
            container = container_rep.id,
            state = container_rep.state,
            name = container_rep.names.get(0).unwrap_or(&"oops".to_string()),
            tail = tail,
            "Started listening"
        );

        let mut stream = container.logs(
            &LogsOptions::builder()
                .stdout(true)
                .stderr(true)
                .tail(tail)
                .follow(true)
                .build(),
        );

        let container_name = match container_rep.names.first() {
            Some(x) => x,
            None => "unknown container",
        };
        let mut map = HashMap::<String, String>::new();
        map.insert("container".into(), container_name.into());

        {
            let mut set = set.lock().expect("Failed to lock Arc");
            set.insert(container_rep.id.clone());
        }

        while let Some(log_result) = stream.next().await {
            let result = log_result
                .wrap_err("Failed to read chunk")
                .and_then(|chunk| extract_text(chunk))
                .map(|message| LogMessage {
                    time: Utc::now(),
                    container: container_name.into(),
                    message: message,
                })
                .map(|request| sender.send(request));
            match result {
                Ok(_) => (),
                Err(e) => error!("Failed to create loki logger request message {:?}", e),
            }
        }

        let mut set = set.lock().expect("Failed to lock Arc");
        set.remove(&container_rep.id);
        info!("{} is done!", &container_name);
    })
}

fn extract_text(chunk: TtyChunk) -> Result<String> {
    Ok(std::str::from_utf8(&chunk)?.to_string())
}
