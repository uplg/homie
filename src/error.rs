use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("Maison API error: {0}")]
    Maison(String),

    #[error("Twitch error: {0}")]
    Twitch(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Toml(#[from] toml::de::Error),

    #[error(transparent)]
    Url(#[from] url::ParseError),
}

impl Error {
    pub fn config(msg: impl Into<String>) -> Self {
        Self::Config(msg.into())
    }

    pub fn maison(msg: impl Into<String>) -> Self {
        Self::Maison(msg.into())
    }

    pub fn twitch(msg: impl Into<String>) -> Self {
        Self::Twitch(msg.into())
    }
}
