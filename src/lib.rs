pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod config;
pub mod credential_proxy;
pub mod external_access;
pub mod grok;
pub mod identity;
mod keychain;
pub mod local_tls;
pub mod observability;
pub mod refresh_routes;
mod route_activation;
pub mod routing;
mod saved_login;
pub mod server;
pub mod service;

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
