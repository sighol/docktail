use std::collections::HashMap;

use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    pub targets: Vec<Target>,
    pub tags: HashMap<String, String>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Target {
    pub url: String,
    pub basic_auth: Option<BasicAuth>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct BasicAuth {
    pub username: String,
    pub password: String,
}
