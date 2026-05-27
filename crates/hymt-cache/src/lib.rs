use hymt_core::error::CoreError;

pub fn get(_key: &str) -> Result<Option<String>, CoreError> {
    Ok(None)
}

pub fn set(_key: &str, _value: &str) -> Result<(), CoreError> {
    Ok(())
}
