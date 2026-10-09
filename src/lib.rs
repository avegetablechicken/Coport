pub mod claude;
mod claude_api;
mod claude_settings;
mod codex_env;
pub mod config;
pub mod external_access;
pub mod identity;
mod keychain;
pub mod local_tls;
pub mod logger;
pub mod model_calls;
mod observation_memory;
mod request_ids;
pub mod routing;
pub mod server;
pub mod service;
mod tunnel;
pub mod url_routing;

#[derive(Debug, Clone)]
pub struct Error {
    pub status: u16,
    pub message: &'static str,
}
impl Error {
    pub fn new(status: u16, message: &'static str) -> Self {
        Self { status, message }
    }
    pub fn config(message: &'static str) -> Self {
        Self::new(502, message)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
