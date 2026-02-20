use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("s3: {0}")]
    S3(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_sqlite_error() {
        let err = Error::Sqlite(rusqlite::Error::QueryReturnedNoRows);
        let msg = err.to_string();
        assert!(msg.starts_with("sqlite: "), "got: {msg}");
    }

    #[test]
    fn display_io_error() {
        let err = Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
        assert!(err.to_string().starts_with("io: "), "got: {err}");
    }

    #[test]
    fn display_s3_error() {
        let err = Error::S3("bucket missing".into());
        assert_eq!(err.to_string(), "s3: bucket missing");
    }

    #[test]
    fn display_other_error() {
        let err = Error::Other("something broke".into());
        assert_eq!(err.to_string(), "something broke");
    }

    #[test]
    fn from_rusqlite_error() {
        let sqlite_err = rusqlite::Error::QueryReturnedNoRows;
        let err: Error = sqlite_err.into();
        assert!(matches!(err, Error::Sqlite(_)));
    }

    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "test");
        let err: Error = io_err.into();
        assert!(matches!(err, Error::Io(_)));
    }
}
