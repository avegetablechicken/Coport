//! Account Routes, as every App's are shaped: a Route by the login a token
//! is. How an App decides which login that is stays the App's own; the
//! Route it decides, and what its records say, are shared.
use crate::config::Choice;

/// A Route by account, with what its App adds (`ext`).
#[derive(Clone)]
pub struct AccountRoute<I, E = ()> {
    pub token: String,
    pub proxy: Choice,
    /// The account, once the App knows it: locally, or by an Account lookup.
    pub identity: Option<I>,
    /// The token is a Saved login's. Claude counts one its Account lookup
    /// named as matched too.
    pub matched_login: bool,
    /// What records name the Route as `provider`, where the App names one.
    pub provider: Option<String>,
    /// The `routing.account` key, source or fallback that carries the Route.
    pub account_label: Option<String>,
    /// A Saved login or an account the routing maps, not one a fallback
    /// carries.
    pub configured: bool,
    /// An Account lookup must name the account before the request is sent.
    pub needs_profile: bool,
    pub ext: E,
}

/// The account an App's identity records name.
pub trait RouteIdentity {
    fn account_id(&self) -> Option<&str>;
}

impl RouteIdentity for crate::codex::Identity {
    fn account_id(&self) -> Option<&str> {
        Some(&self.account_id)
    }
}

impl RouteIdentity for crate::claude::ClaudeIdentity {
    fn account_id(&self) -> Option<&str> {
        Some(&self.account_id)
    }
}

impl RouteIdentity for crate::antigravity::AntigravityIdentity {
    fn account_id(&self) -> Option<&str> {
        Some(&self.email)
    }
}

impl RouteIdentity for crate::grok::GrokIdentity {
    fn account_id(&self) -> Option<&str> {
        self.account()
    }
}

impl<I: RouteIdentity, E> AccountRoute<I, E> {
    /// The account records name.
    pub fn account_id(&self) -> Option<&str> {
        self.identity.as_ref().and_then(RouteIdentity::account_id)
    }
}
