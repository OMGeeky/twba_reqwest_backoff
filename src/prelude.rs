pub(crate) use std::error::Error as StdError;

pub use crate::ReqwestBackoffError;

pub(crate) use tracing::{info, warn};
pub(crate) type Result<T> = std::result::Result<T, ReqwestBackoffError>;
