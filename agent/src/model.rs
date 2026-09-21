use std::collections::BTreeMap;

use reqwest::{
    Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

impl Endpoint {
    pub fn validate(&self) -> Result<(), &'static str> {
        let url = Url::parse(&self.url).map_err(|_| "invalid HTTP URL")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err("URLs must use HTTP(S), without userinfo or fragments");
        }
        self.header_map()?;
        Ok(())
    }

    pub fn header_map(&self) -> Result<HeaderMap, &'static str> {
        let mut result = HeaderMap::new();
        for (name, value) in &self.headers {
            let name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|_| "invalid HTTP header")?;
            if matches!(
                name.as_str(),
                "host" | "content-length" | "transfer-encoding" | "connection" | "upgrade"
            ) {
                return Err("transport headers are managed by the agent");
            }
            let value = HeaderValue::from_str(value).map_err(|_| "invalid HTTP header")?;
            result.insert(name, value);
        }
        Ok(result)
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub id: String,
    pub input: Endpoint,
    pub output: Endpoint,
    pub callback: Endpoint,
    pub args: Vec<String>,
    pub output_extension: String,
}

impl Job {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.id.is_empty()
            || self.id.len() > 80
            || !self
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        {
            return Err("id must contain 1..80 ASCII letters, digits, dashes, underscores or dots");
        }
        if self.output_extension.is_empty()
            || self.output_extension.len() > 10
            || !self
                .output_extension
                .bytes()
                .all(|c| c.is_ascii_alphanumeric())
        {
            return Err("output_extension must contain 1..10 ASCII letters or digits");
        }
        if self.args.len() > 128 || self.args.iter().any(|a| a.len() > 4096 || a.contains('\0')) {
            return Err("too many or oversized FFmpeg arguments");
        }
        let inputs: Vec<_> = self
            .args
            .iter()
            .enumerate()
            .filter(|(_, a)| a.as_str() == "{input}")
            .collect();
        if inputs.len() != 1
            || inputs[0].0 == 0
            || self.args[inputs[0].0 - 1] != "-i"
            || self.args.iter().filter(|a| a.as_str() == "-i").count() != 1
            || self
                .args
                .iter()
                .filter(|a| a.as_str() == "{output}")
                .count()
                != 1
            || self.args.last().map(String::as_str) != Some("{output}")
        {
            return Err(
                "args must have one '-i', followed by '{input}', and end with one '{output}'",
            );
        }
        self.input.validate()?;
        self.output.validate()?;
        self.callback.validate()?;
        Ok(())
    }
}

#[derive(Clone, Default, Serialize)]
pub struct UploadResponse {
    pub status: u16,
    pub headers: Vec<ResponseHeader>,
    pub body_base64: String,
    pub body_truncated: bool,
}

#[derive(Clone, Serialize)]
pub struct ResponseHeader {
    pub name: String,
    pub value_base64: String,
}

#[derive(Clone, Default, Serialize)]
pub struct Outcome {
    pub error: Option<String>,
    pub ffmpeg_exit_code: Option<i32>,
    pub upload_response: Option<UploadResponse>,
}

#[derive(Serialize)]
pub struct Event<'a> {
    pub version: u8,
    pub id: &'a str,
    pub sequence: u64,
    pub state: &'a str,
    pub terminal: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<&'a Outcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ffmpeg_log: Option<String>,
    pub ffmpeg_log_truncated: bool,
}
