use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::Serialize;
use shiplift::{
    builder::ContainerListOptions, rep::Container as RepContainer, tty::TtyChunk, Docker,
    LogsOptions,
};
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::{self, task::JoinHandle};

use tracing::{error, info};
use tracing_bunyan_formatter::{BunyanFormattingLayer, JsonStorageLayer};
use tracing_subscriber::{layer::SubscriberExt, Registry};

// The loki code is taken from this repository
// https://github.com/nwmqpa/loki-logger
// That is AGPL, which I don't want this repository to be.
// But, the code is basically just using the Loki's API. So is it really copyrightable?
#[derive(Serialize, Debug)]
struct LokiStream {
    stream: HashMap<String, String>,
    values: Vec<[String; 2]>,
}

#[derive(Serialize, Debug)]
struct LokiRequest {
    streams: Vec<LokiStream>,
}

struct LokiLogger {
    url: String,
    initial_labels: Option<HashMap<String, String>>,
    client: reqwest::Client,
}

impl LokiLogger {
    fn new<S: AsRef<str>>(url: S, initial_labels: Option<HashMap<String, String>>) -> Self {
        Self {
            url: url.as_ref().to_string(),
            initial_labels,
            client: reqwest::Client::new(),
        }
    }

    async fn log_to_loki(&self, message: String) -> Result<(), Box<dyn Error>> {
        let client = self.client.clone();
        let url = self.url.clone();

        let labels = match &self.initial_labels {
            Some(x) => x.clone(),
            None => HashMap::new(),
        };

        let loki_request = make_request(message, labels)?;
        match client.post(url).json(&loki_request).send().await {
            Ok(_) => Ok(()),
            Err(x) => Err(Box::new(x)),
        }
    }
}

fn make_request(
    message: String,
    labels: HashMap<String, String>,
) -> Result<LokiRequest, Box<dyn Error>> {
    let start = SystemTime::now();
    let time_ns = time_offset_since(start)?;
    let loki_request = LokiRequest {
        streams: vec![LokiStream {
            stream: labels,
            values: vec![[time_ns, message]],
        }],
    };
    Ok(loki_request)
}

fn time_offset_since(start: SystemTime) -> Result<String, Box<dyn Error>> {
    let since_start = start.duration_since(UNIX_EPOCH)?;
    let time_ns = since_start.as_nanos().to_string();
    Ok(time_ns)
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let formatting_layer = BunyanFormattingLayer::new("tracing_demo".into(), std::io::stdout);
    let subscriber = Registry::default()
        .with(JsonStorageLayer)
        .with(formatting_layer);
    tracing::subscriber::set_global_default(subscriber).unwrap();

    info!("Orphan event without a parent span");

    let start_time: DateTime<Utc> = Utc::now();
    let docker = Docker::new();
    let loki_url = match std::env::var("LOKI_URL") {
        Ok(x) => x,
        Err(_) => {
            error!("Please set the LOKI_URL environment variable");
            std::process::exit(1);
        }
    };

    let containers_set = Arc::new(Mutex::new(HashSet::<String>::new()));

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
                loki_url.clone(),
                container_rep.clone(),
                containers_set.clone(),
                start_time,
            );
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

fn spawn_job(
    docker: Docker,
    loki_url: String,
    container_rep: RepContainer,
    set: Arc<Mutex<HashSet<String>>>,
    start_time: DateTime<Utc>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let container = docker.containers().get(&container_rep.id);

        // If the container is newer than this program, then we should grab all
        // log lines, so that we don't miss anything. However, for older
        // containers, it is more likely that it was this program that was
        // restarted, so we shouldn't add duplicated lines.
        let tail = if container_rep.created > start_time {
            "all"
        } else {
            "0"
        };

        info!(
            "container {}. state={}, name={}, tail={}",
            container_rep.id,
            container_rep.state,
            container_rep.names.get(0).unwrap_or(&"oops".to_string()),
            tail,
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

        let loki = LokiLogger::new(&loki_url, Some(map));

        {
            let mut set = set.lock().expect("Failed to lock Arc");
            set.insert(container_rep.id.clone());
        }

        while let Some(log_result) = stream.next().await {
            match log_result {
                Ok(chunk) => match push_to_loki(&loki, chunk).await {
                    Ok(_) => (),
                    Err(e) => error!("Failed to push to loki log: {}", e),
                },
                Err(e) => error!("Error: {}", e),
            }
        }

        {
            let mut set = set.lock().expect("Failed to lock Arc");
            set.remove(&container_rep.id);
        }
        info!("{} is done!", &container_name);
    })
}

async fn push_to_loki(loki: &LokiLogger, chunk: TtyChunk) -> Result<(), Box<dyn Error>> {
    let text = match chunk {
        TtyChunk::StdOut(bytes) => std::str::from_utf8(&bytes).unwrap().to_string(),
        TtyChunk::StdErr(bytes) => std::str::from_utf8(&bytes).unwrap().to_string(),
        TtyChunk::StdIn(_) => unreachable!(),
    };

    loki.log_to_loki(text).await
}
