use futures::StreamExt;
use shiplift::{builder::ContainerListOptions, tty::TtyChunk, Docker, LogsOptions};

use serde::Serialize;
use std::{
    collections::HashMap,
    error::Error,
    time::{SystemTime, UNIX_EPOCH},
};

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
        println!("Sending request: {:?}", &loki_request);
        match client.post(url).json(&loki_request).send().await {
            Ok(response) => {
                println!("Received response: {:?}", response);
                Ok(())
            }
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
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let docker = Docker::new();
    let loki_url = match std::env::var("LOKI_URL") {
        Ok(x) => x,
        Err(_) => {
            eprintln!("Please set the LOKI_URL environment variable");
            std::process::exit(1);
        }
    };

    let containers = docker
        .containers()
        .list(&ContainerListOptions::builder().all().build())
        .await?;

    let containers = containers
        .into_iter()
        .filter(|x| x.state == "running")
        .collect::<Vec<_>>();

    for container in containers.iter() {
        println!(
            "container {}. state={}, name={}",
            container.id,
            container.state,
            container.names.get(0).unwrap_or(&"oops".to_string())
        );
    }

    let mut jobs = vec![];
    for container_rep in containers {
        let docker = docker.clone();
        let loki_url = loki_url.clone();
        let job = tokio::spawn(async move {
            let container = docker.containers().get(container_rep.id);
            // let job = tokio::spawn(async move {
            let mut stream = container.logs(
                &LogsOptions::builder()
                    .stdout(true)
                    .stderr(true)
                    .tail("1")
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

            while let Some(log_result) = stream.next().await {
                match log_result {
                    Ok(chunk) => print_chunk(&loki, chunk)
                        .await
                        .expect("Failed to push loki log"),
                    Err(e) => eprintln!("Error: {}", e),
                }
            }
        });

        jobs.push(job);
    }

    for j in jobs {
        j.await?;
    }

    Ok(())
}

async fn print_chunk(loki: &LokiLogger, chunk: TtyChunk) -> Result<(), Box<dyn Error>> {
    let text = match chunk {
        TtyChunk::StdOut(bytes) => std::str::from_utf8(&bytes).unwrap().to_string(),
        TtyChunk::StdErr(bytes) => std::str::from_utf8(&bytes).unwrap().to_string(),
        TtyChunk::StdIn(_) => unreachable!(),
    };

    loki.log_to_loki(text).await
}
