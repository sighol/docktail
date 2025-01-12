use serde::Serialize;
use std::collections::HashMap;

use eyre::{self, WrapErr};

use reqwest;

use crate::config::BasicAuth;

#[derive(Serialize, Debug)]
pub struct LokiRequest {
    pub streams: Vec<LokiStream>,
}

#[derive(Serialize, Clone, Debug)]
pub struct LokiStream {
    pub stream: HashMap<String, String>,
    pub values: Vec<[String; 2]>,
}

pub struct LokiLogger {
    url: String,
    client: reqwest::Client,
    basic_auth: Option<BasicAuth>,
}

impl LokiLogger {
    pub fn new(url: String, basic_auth: Option<BasicAuth>) -> Self {
        Self {
            url: url,
            client: reqwest::Client::new(),
            basic_auth,
        }
    }

    pub async fn log(&self, message: LokiRequest) -> eyre::Result<reqwest::Response> {
        let client = self.client.clone();
        let url = self.url.clone();
        let mut request = client.post(url);

        if let Some(basic_auth) = &self.basic_auth {
            request = request.basic_auth(
                basic_auth.username.clone(),
                Some(basic_auth.password.clone()),
            );
        }

        request
            .json(&message)
            .send()
            .await
            .and_then(|x| x.error_for_status())
            .wrap_err("reqwest")
    }
}
