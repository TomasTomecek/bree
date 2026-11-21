//! The one error type of the configuration and run code: a finished,
//! human-readable message. `main` prints it as `bree: <message>` and exits 2.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Error(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<crate::yaml::Error> for Error {
    fn from(e: crate::yaml::Error) -> Self {
        Error(e.to_string())
    }
}
