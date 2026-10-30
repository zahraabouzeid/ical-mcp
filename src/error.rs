#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct IcalError(String);

impl IcalError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

pub type Result<T> = std::result::Result<T, IcalError>;

impl From<IcalError> for String {
    fn from(error: IcalError) -> Self {
        error.0
    }
}
