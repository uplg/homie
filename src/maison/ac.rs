use reqwest::Method;
use serde::Serialize;

use crate::{
    error::Result,
    maison::{MaisonClient, lamps::ActionResponse},
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MitsubishiCommandRequest<'a> {
    host: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_ip: Option<&'a str>,
    command: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
}

impl MaisonClient {
    pub async fn send_mitsubishi_command(
        &self,
        host: &str,
        command: &str,
        model: Option<&str>,
        local_ip: Option<&str>,
    ) -> Result<ActionResponse> {
        let body = MitsubishiCommandRequest {
            host,
            local_ip,
            command,
            model,
        };
        self.request(Method::POST, "broadlink/mitsubishi/send", Some(&body))
            .await
    }
}
