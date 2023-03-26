use chrono::{DateTime, Utc};
use futures::StreamExt;

use hyper::{
    header::CONTENT_TYPE,
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server,
};
use lazy_static::lazy_static;
use prometheus::{opts, register_counter, Counter, Encoder, TextEncoder};
use shiplift::{
    builder::ContainerListOptions, rep::Container as RepContainer, tty::TtyChunk, Docker,
    LogsOptions,
};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use eyre::{self, Result, WrapErr};

use tokio::{
    self,
    sync::mpsc::{unbounded_channel, UnboundedSender},
    task::JoinHandle,
    time::sleep,
};

use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, Registry};

mod loki;
use loki::{LokiLogger, LokiRequest, LokiStream};

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

#[tokio::main]
async fn main() -> Result<()> {
    let subscriber = Registry::default().with(tracing_logfmt::layer());
    tracing::subscriber::set_global_default(subscriber).unwrap();

    tokio::spawn(async { prometheus_server().await });

    let start_time: DateTime<Utc> = Utc::now();
    let docker = Docker::new();
    let loki_url = match std::env::var("LOKI_URL") {
        Ok(x) => x,
        Err(_) => {
            error!("Please set the LOKI_URL environment variable");
            std::process::exit(1);
        }
    };
    info!("Starting docktail...");
    info!("Logging to {}", &loki_url);

    let (sender, mut receiver) = unbounded_channel::<LokiStream>();
    tokio::spawn(async move {
        let loki = LokiLogger::new(loki_url);

        loop {
            let mut loki_request = LokiRequest { streams: vec![] };
            loop {
                match receiver.try_recv() {
                    Ok(m) => {
                        LOKI_STREAM_COUNTER.inc();
                        loki_request.streams.push(m)
                    }
                    Err(_) => {
                        // Either disconnect or empty. In both cases, we are done retrieving streams.
                        break;
                    }
                }
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

fn spawn_job(
    docker: Docker,
    container_rep: RepContainer,
    set: Arc<Mutex<HashSet<String>>>,
    start_time: DateTime<Utc>,
    sender: UnboundedSender<LokiStream>,
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
                .map(|message| create_message(message, map.clone()))
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

fn create_message(message: String, labels: HashMap<String, String>) -> LokiStream {
    let time_ns: String = Utc::now().timestamp_nanos().to_string();
    LokiStream {
        stream: labels,
        values: vec![[time_ns, message]],
    }
}
