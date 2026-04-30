use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::{error::Result, maison::MaisonClient};

#[derive(Debug, Clone, Deserialize)]
pub struct Lamp {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub connected: bool,
    #[serde(default)]
    pub reachable: bool,
}

#[derive(Debug, Deserialize)]
struct ListResponse {
    #[serde(default)]
    lamps: Vec<Lamp>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionResponse {
    pub success: bool,
    pub message: String,
}

#[derive(Serialize)]
struct PowerBody {
    enabled: bool,
}

#[derive(Serialize)]
struct BrightnessBody {
    brightness: u8,
}

#[derive(Serialize)]
struct TemperatureBody {
    temperature: u8,
}

#[derive(Serialize)]
struct ColorBody {
    x: f32,
    y: f32,
}

#[derive(Serialize)]
struct EffectBody<'a> {
    effect: &'a str,
}

impl MaisonClient {
    pub async fn list_zigbee_lamps(&self) -> Result<Vec<Lamp>> {
        let response: ListResponse = self
            .request::<(), _>(Method::GET, "zigbee/lamps", None)
            .await?;
        Ok(response.lamps)
    }

    pub async fn set_lamp_power(&self, lamp_id: &str, enabled: bool) -> Result<ActionResponse> {
        let path = format!("zigbee/lamps/{lamp_id}/power");
        self.request(Method::POST, &path, Some(&PowerBody { enabled }))
            .await
    }

    pub async fn set_lamp_brightness(
        &self,
        lamp_id: &str,
        brightness: u8,
    ) -> Result<ActionResponse> {
        let path = format!("zigbee/lamps/{lamp_id}/brightness");
        self.request(Method::POST, &path, Some(&BrightnessBody { brightness }))
            .await
    }

    pub async fn set_lamp_temperature(
        &self,
        lamp_id: &str,
        temperature: u8,
    ) -> Result<ActionResponse> {
        let path = format!("zigbee/lamps/{lamp_id}/temperature");
        self.request(Method::POST, &path, Some(&TemperatureBody { temperature }))
            .await
    }

    pub async fn set_lamp_color(&self, lamp_id: &str, x: f32, y: f32) -> Result<ActionResponse> {
        let path = format!("zigbee/lamps/{lamp_id}/color");
        self.request(Method::POST, &path, Some(&ColorBody { x, y }))
            .await
    }

    pub async fn set_lamp_effect(&self, lamp_id: &str, effect: &str) -> Result<ActionResponse> {
        let path = format!("zigbee/lamps/{lamp_id}/effect");
        self.request(Method::POST, &path, Some(&EffectBody { effect }))
            .await
    }
}
