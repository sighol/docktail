use chrono::{DateTime, TimeZone, Utc};
use futures::StreamExt;
use serde::Serialize;
use shiplift::{
    builder::ContainerListOptions, rep::Container as RepContainer, tty::TtyChunk, Docker,
    LogsOptions,
};
use std::{
    collections::{HashMap, HashSet},
    sync::mpsc::{channel, Sender},
    sync::{Arc, Mutex},
    time::Duration,
};

use eyre::{self, ContextCompat, WrapErr};

use tokio::{self, task::JoinHandle};

use tracing::{error, info};
use tracing_bunyan_formatter::{BunyanFormattingLayer, JsonStorageLayer};
use tracing_subscriber::{layer::SubscriberExt, Registry};

use reqwest;

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
    client: reqwest::Client,
}

impl LokiLogger {
    fn new<S: AsRef<str>>(url: S) -> Self {
        Self {
            url: url.as_ref().to_string(),
            client: reqwest::Client::new(),
        }
    }

    async fn log(&self, message: LokiRequest) -> eyre::Result<reqwest::Response> {
        let client = self.client.clone();
        let url = self.url.clone();
        client
            .post(url)
            .json(&message)
            .send()
            .await
            .wrap_err("Reqwest")
    }
}

fn time_offset_since(time: DateTime<Utc>) -> Option<i64> {
    let start = Utc.ymd(1970, 1, 1).and_hms(0, 0, 0);
    let since_start = time - start;
    since_start.num_nanoseconds()
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let formatting_layer =
        BunyanFormattingLayer::new(env!("CARGO_PKG_NAME").into(), std::io::stdout);
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

    let (sender, receiver) = channel::<LokiRequest>();
    tokio::spawn(async move {
        let loki = LokiLogger::new(&loki_url);
        loop {
            let next_message = receiver.recv().wrap_err("Receive channel failed").unwrap();
            match loki.log(next_message).await {
                Ok(_) => (),
                Err(e) => error!("Failed to send message to loki: {}", e),
            }
        }
    });

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
                container_rep.clone(),
                containers_set.clone(),
                start_time,
                sender.clone(),
            );
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

fn spawn_job(
    docker: Docker,
    container_rep: RepContainer,
    set: Arc<Mutex<HashSet<String>>>,
    start_time: DateTime<Utc>,
    sender: Sender<LokiRequest>,
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

        {
            let mut set = set.lock().expect("Failed to lock Arc");
            set.insert(container_rep.id.clone());
        }

        while let Some(log_result) = stream.next().await {
            let result = log_result
                .wrap_err("Failed to read chunk")
                .and_then(|chunk| extract_text(chunk))
                .and_then(|message| create_message(message, map.clone()))
                .and_then(|request| Ok(sender.send(request)));
            match result {
                Ok(_) => (),
                Err(e) => error!("Failed to create loki logger request message {}", e),
            }
        }

        {
            let mut set = set.lock().expect("Failed to lock Arc");
            set.remove(&container_rep.id);
        }
        info!("{} is done!", &container_name);
    })
}

fn create_message(message: String, labels: HashMap<String, String>) -> eyre::Result<LokiRequest> {
    let start = Utc::now();
    let time_ns: i64 = time_offset_since(start).wrap_err("No start time")?;
    let time_ns: String = time_ns.to_string();
    let loki_request = LokiRequest {
        streams: vec![LokiStream {
            stream: labels,
            values: vec![[time_ns, message]],
        }],
    };
    Ok(loki_request)
}

fn extract_text(chunk: TtyChunk) -> eyre::Result<String> {
    let text = match chunk {
        TtyChunk::StdOut(bytes) => std::str::from_utf8(&bytes)?.to_string(),
        TtyChunk::StdErr(bytes) => std::str::from_utf8(&bytes)?.to_string(),
        TtyChunk::StdIn(_) => unreachable!(),
    };

    Ok(text)
}
