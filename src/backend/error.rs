//! Internal error type for the on-disk storage backend.
//!
//! Adapted from the Apache-2.0 `s3s-fs` reference implementation; the span-trace
//! branch (which depended on the `binary`/`tracing-error` stack) has been removed.

use s3s::S3Error;
use s3s::S3ErrorCode;
use s3s::StdError;

use tracing::error;

#[derive(Debug)]
pub struct Error {
    source: StdError,
}

pub type Result<T = (), E = Error> = std::result::Result<T, E>;

impl Error {
    #[must_use]
    #[track_caller]
    pub fn new(source: StdError) -> Self {
        log(&*source);
        Self { source }
    }

    #[must_use]
    #[track_caller]
    pub fn from_string(s: impl Into<String>) -> Self {
        Self::new(s.into().into())
    }
}

impl<E> From<E> for Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    #[track_caller]
    fn from(source: E) -> Self {
        Self::new(Box::new(source))
    }
}

impl From<Error> for S3Error {
    fn from(e: Error) -> Self {
        S3Error::with_source(S3ErrorCode::InternalError, e.source)
    }
}

#[inline]
pub(crate) fn log(source: &dyn std::error::Error) {
    error!(target: "s3_storage_internal_error", error = %source);
}

macro_rules! try_ {
    ($result:expr) => {
        match $result {
            Ok(val) => val,
            Err(err) => {
                $crate::backend::error::log(&err);
                return Err(::s3s::S3Error::internal_error(err));
            }
        }
    };
}
