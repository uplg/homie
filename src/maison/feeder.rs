use reqwest::Method;
use serde::Serialize;

use crate::{
    error::Result,
    maison::{MaisonClient, lamps::ActionResponse},
};

#[derive(Serialize)]
struct FeedBody {
    portion: u64,
}

impl MaisonClient {
    pub async fn feeder_feed(&self, device_id: &str, portion: u64) -> Result<ActionResponse> {
        let path = format!("devices/{device_id}/feeder/feed");
        self.request(Method::POST, &path, Some(&FeedBody { portion }))
            .await
    }
}
