//! The CLI's errors and their exit codes (cli/README.md "Exit codes").

use std::fmt;

/// What went wrong, by exit code.
#[derive(Debug)]
pub enum Error {
    /// The ladder answered with its `{error, message}` (docs/ladder-api.md §2):
    /// `kfm: <message> (<code>)`, exit 1.
    Api { code: String, message: String },
    /// The command line or the situation needs something else from the user
    /// (no team given among several, a missing module): exit 2.
    Usage(String),
    /// A submission under `--wait` failed or was rejected: exit 3.
    Submission(String),
    /// Anything else (I/O, the network, a failed build or match): exit 1.
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Api { .. } | Error::Other(_) => 1,
            Error::Usage(_) => 2,
            Error::Submission(_) => 3,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Api { code, message } => write!(f, "{message} ({code})"),
            Error::Usage(m) | Error::Submission(m) | Error::Other(m) => f.write_str(m),
        }
    }
}

/// `Error::Other` from anything displayable.
pub fn other(message: impl fmt::Display) -> Error {
    Error::Other(message.to_string())
}

/// Adds what was being done to an I/O error.
pub trait Context<T> {
    fn context(self, what: impl FnOnce() -> String) -> Result<T>;
}

impl<T, E: fmt::Display> Context<T> for std::result::Result<T, E> {
    fn context(self, what: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|e| Error::Other(format!("{}: {e}", what())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_errors_show_the_message_then_the_code() {
        let e = Error::Api {
            code: "not_found".into(),
            message: "Not found.".into(),
        };
        assert_eq!(e.to_string(), "Not found. (not_found)");
        assert_eq!(e.exit_code(), 1);
        assert_eq!(Error::Usage("x".into()).exit_code(), 2);
        assert_eq!(Error::Submission("x".into()).exit_code(), 3);
    }
}
