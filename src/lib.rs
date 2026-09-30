pub mod claude;
mod claude_api;
mod codex_env;
pub mod config;
pub mod identity;
pub mod logger;
pub mod routing;
pub mod server;
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
