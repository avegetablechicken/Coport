//! Codex, the first App Coport routed: its saved logins, the model
//! providers its homes define, and the `.env` semantics it reads them with.
pub(crate) mod env;
mod login;
mod providers;

pub use login::Identity;
pub(crate) use login::saved_auth_store;
pub use providers::ProviderCredential;
