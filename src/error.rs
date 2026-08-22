use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("{0}")]
    Config(#[from] ConfigError),

    #[error("{0}")]
    Runtime(String),
}

impl AppError {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Config(_) => 2,
            Self::Runtime(_) => 1,
        }
    }
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        Self::Runtime(error.to_string())
    }
}

impl From<toml::ser::Error> for AppError {
    fn from(error: toml::ser::Error) -> Self {
        Self::Runtime(error.to_string())
    }
}
