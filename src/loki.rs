use serde::Serialize;
use std::collections::HashMap;

use eyre::{self, WrapErr};

use reqwest;

// The loki code is taken from this repository
// https://github.com/nwmqpa/loki-logger
// That is AGPL, which I don't want this repository to be.
// But, the code is basically just using the Loki's API. So is it really copyrightable?
#[derive(Serialize, Debug)]
pub struct LokiStream {
    pub stream: HashMap<String, String>,
    pub values: Vec<[String; 2]>,
}

#[derive(Serialize, Debug)]
pub struct LokiRequest {
    pub streams: Vec<LokiStream>,
}

pub struct LokiLogger {
    url: String,
    client: reqwest::Client,
}

impl LokiLogger {
    pub fn new<S: AsRef<str>>(url: S) -> Self {
        Self {
            url: url.as_ref().to_string(),
            client: reqwest::Client::new(),
        }
    }

    pub async fn log(&self, message: LokiRequest) -> eyre::Result<reqwest::Response> {
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
