//! Route activation: whether each of an App's account routes has a login on
//! this computer that it serves. Each App lists only what its logins show;
//! the states and their precedence are decided here, once for every App.
use std::collections::BTreeMap;

/// What one login shows about the account route it belongs to. A login
/// whose account no route names is left out.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Login {
    /// Local data names the login's account route.
    Active(String),
    /// An Account lookup found the login's account, and it names this route
    /// although local data does not.
    Remote(String),
    /// The login cannot be read without running a login shell.
    Unreadable,
    /// Account discovery has yet to look the login's account up.
    Unidentified,
    /// Account discovery failed to look the login's account up.
    ProbeFailed,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Activation {
    Active,
    Remote,
    ProbeFailed,
    Unknown,
    Inactive,
}
impl Activation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Remote => "remote",
            Self::ProbeFailed => "probe_failed",
            Self::Unknown => "unknown",
            Self::Inactive => "inactive",
        }
    }
}

/// The activation of each of `routes`: active where a login's local data
/// names it; else remote where a lookup did; else, while some login is not
/// accounted for, probe failed if discovery failed for one, or unknown;
/// else inactive. Only `routes` are reported, never a fallback.
pub(crate) fn route_activation<'a>(
    routes: impl IntoIterator<Item = &'a String>,
    logins: impl IntoIterator<Item = Login>,
) -> BTreeMap<String, &'static str> {
    let mut states: BTreeMap<String, Option<Activation>> = routes
        .into_iter()
        .map(|route| (route.clone(), None))
        .collect();
    let (mut remote, mut failed, mut pending) = (Vec::new(), false, false);
    for login in logins {
        match login {
            Login::Active(route) => {
                if let Some(state) = states.get_mut(&route) {
                    *state = Some(Activation::Active);
                }
            }
            Login::Remote(route) => remote.push(route),
            Login::ProbeFailed => failed = true,
            Login::Unreadable | Login::Unidentified => pending = true,
        }
    }
    for route in remote {
        if let Some(state @ None) = states.get_mut(&route) {
            *state = Some(Activation::Remote);
        }
    }
    let rest = if failed {
        Activation::ProbeFailed
    } else if pending {
        Activation::Unknown
    } else {
        Activation::Inactive
    };
    states
        .into_iter()
        .map(|(route, state)| (route, state.unwrap_or(rest).as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Login::*, *};

    #[test]
    fn each_route_takes_the_strongest_evidence_any_login_gives() {
        let routes = ["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let active = |route: &str| Active(route.into());
        let remote = |route: &str| Remote(route.into());
        let cases: &[(&[Login], [&str; 3])] = &[
            (&[], ["inactive"; 3]),
            (&[active("a")], ["active", "inactive", "inactive"]),
            // A fallback or unknown route is not reported.
            (&[active("fallback")], ["inactive"; 3]),
            (&[active("a"), Unreadable], ["active", "unknown", "unknown"]),
            (&[Unidentified], ["unknown"; 3]),
            (&[Unidentified, ProbeFailed], ["probe_failed"; 3]),
            (
                &[remote("b"), ProbeFailed],
                ["probe_failed", "remote", "probe_failed"],
            ),
            // Local data outranks a lookup for the same route.
            (
                &[remote("a"), active("a")],
                ["active", "inactive", "inactive"],
            ),
            (
                &[remote("c"), remote("c")],
                ["inactive", "inactive", "remote"],
            ),
        ];
        for (logins, expected) in cases {
            let states = route_activation(&routes, logins.iter().cloned());
            assert_eq!(
                states.values().copied().collect::<Vec<_>>(),
                expected,
                "{logins:?}"
            );
        }
    }
}
