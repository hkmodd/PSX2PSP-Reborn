use std::path::PathBuf;

/// Every failure the core can produce. No panics on malformed input: hostile files map to `Format`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("I/O error on {path}: {source}")]
    IoPath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid {what}: {detail}")]
    Format { what: &'static str, detail: String },

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("operation cancelled")]
    Cancelled,
}

impl Error {
    pub(crate) fn format(what: &'static str, detail: impl Into<String>) -> Self {
        Self::Format { what, detail: detail.into() }
    }

    pub fn io_path(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Self::IoPath { path, source }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
