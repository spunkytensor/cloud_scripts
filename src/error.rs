use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Cli(String),
    #[error("invalid local state: {0}")]
    State(String),
    #[error("provider rejected the request: {0}")]
    Backend(String),
    #[error("provider result is uncertain; retry to reconcile: {0}")]
    Uncertain(String),
    #[error("remote worker operation failed: {0}")]
    Remote(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Toml(#[from] toml::de::Error),
    #[error("{0}")]
    TomlSerialize(#[from] toml::ser::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Cli(_) => 2,
            Self::State(_)
            | Self::Io(_)
            | Self::Json(_)
            | Self::Toml(_)
            | Self::TomlSerialize(_) => 3,
            Self::Uncertain(_) => 4,
            Self::Remote(_) => 5,
            Self::Backend(_) => 4,
        }
    }
}
