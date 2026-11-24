//! IaR Live Audio's channel list: the channels an API key may send to.

use std::time::Duration;

use serde::{Deserialize, Deserializer};

/// Where the channel list comes from, in production.
pub const CHANNELS_URL: &str = "https://api-gateway.iamresponding.com/api/apikey/channels";
/// Where audio goes: one WebSocket per channel, at `<base><channel id>`.
pub const WS_BASE: &str = "wss://wslb.iamresponding.com:443/";

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct IarChannel {
    #[serde(deserialize_with = "text")]
    pub id: String,
    #[serde(deserialize_with = "text")]
    pub name: String,
    #[serde(deserialize_with = "text")]
    pub description: String,
}

/// IDs come as text or as numbers; either way they're kept as text, as IaR-Stream does.
fn text<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(s) => s,
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    })
}

/// The channels `api_key` may send to. The key is sent in the `X-Api-Key` header and never
/// appears in an error message.
pub fn fetch(url: &str, api_key: &str) -> Result<Vec<IarChannel>, String> {
    let agent = ureq::Agent::new_with_config(ureq::config::Config::builder().timeout_global(Some(TIMEOUT)).build());
    let mut response = agent.get(url).header("X-Api-Key", api_key).call().map_err(describe)?;
    response.body_mut().read_json::<Vec<IarChannel>>().map_err(|_| "IaR Live Audio sent a channel list this plugin can't read".to_string())
}

fn describe(e: ureq::Error) -> String {
    match e {
        ureq::Error::StatusCode(401 | 403) => "IaR Live Audio didn't accept that API key".into(),
        ureq::Error::StatusCode(code) => format!("IaR Live Audio answered with HTTP {code}"),
        _ => "couldn't reach IaR Live Audio".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trunk_recorder_plugin::testing::MockServer;

    #[test]
    fn reads_the_channel_list_with_the_key_in_the_header() {
        let server = MockServer::start(|_| (200, r#"[{"id":"42","name":"Dispatch","description":"Main"},{"id":43,"name":"Tac"}]"#.into()));
        let url = format!("{}/api/apikey/channels", server.url());
        let list = fetch(&url, "secret-key").expect("channels");
        assert_eq!(
            list,
            vec![
                IarChannel { id: "42".into(), name: "Dispatch".into(), description: "Main".into() },
                IarChannel { id: "43".into(), name: "Tac".into(), description: String::new() },
            ]
        );
        let req = &server.requests()[0];
        assert_eq!(req.method, "GET");
        assert_eq!(req.header("x-api-key"), Some("secret-key"));
    }

    #[test]
    fn a_rejected_key_says_so_without_the_key() {
        let server = MockServer::start(|_| (401, "nope".into()));
        let err = fetch(&format!("{}/api/apikey/channels", server.url()), "secret-key").unwrap_err();
        assert_eq!(err, "IaR Live Audio didn't accept that API key");
        assert!(!err.contains("secret-key"));
    }

    #[test]
    fn other_failures_name_the_status() {
        let server = MockServer::start(|_| (503, String::new()));
        let err = fetch(&format!("{}/api/apikey/channels", server.url()), "k").unwrap_err();
        assert_eq!(err, "IaR Live Audio answered with HTTP 503");
    }
}
