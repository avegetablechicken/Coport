//! Traffic totals and the Traffic metrics derived from them: how one device's
//! traffic is described, and how every device's traffic adds up to Merged
//! Traffic. Totals add; metrics are derived once, after the totals are summed.
use crate::data_api::{Service, Stats};
use crate::traffic::identity::{Target, UNIDENTIFIED};
use crate::traffic::{SourceTraffic, TrafficScope};
use serde::Serialize;

/// The time a view covers, in epoch milliseconds, and its bucket width.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub start: i64,
    pub end: i64,
    pub bucket_minutes: u64,
}
impl Window {
    fn bucket_ms(&self) -> i64 {
        self.bucket_minutes.max(1) as i64 * 60_000
    }
    pub fn buckets(&self) -> usize {
        ((self.end - self.start).max(0) as u64).div_ceil(self.bucket_ms() as u64) as usize
    }
    /// Where `other`'s first bucket lands on this window, by time. Parts read
    /// at different times cover different windows, so bars line up by time.
    fn offset_of(&self, other: &Window) -> i64 {
        let buckets = (other.start - self.start) as f64 / self.bucket_ms() as f64;
        // Half-way rounds up, as the panel always rounded it.
        (buckets + 0.5).floor() as i64
    }
}

/// Rates and averages derived from a view's or an account's totals.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Metrics {
    pub avg_ms: Option<u64>,
    /// Over successful calls that streamed output; see ADR-0002.
    pub first_token_ms: Option<u64>,
    /// From each call's first streamed token to its completion.
    pub output_tps: Option<f64>,
    /// All output over all request durations, from each request's start.
    pub output_tps_from_start: Option<f64>,
    pub cache_hit_rate: Option<f64>,
    pub error_rate: Option<f64>,
}
impl Metrics {
    pub fn of(s: &Stats) -> Self {
        Self {
            avg_ms: (s.latency_samples > 0).then(|| s.latency_total_ms / s.latency_samples),
            first_token_ms: s
                .first_token_samples
                .filter(|samples| *samples > 0)
                .map(|samples| s.first_token_total_ms.unwrap_or(0) / samples),
            output_tps: crate::data_api::output_tps(s.output_tps_tokens, s.output_tps_duration_ms),
            output_tps_from_start: crate::data_api::output_tps(
                s.output_tokens.filter(|tokens| *tokens > 0),
                Some(s.latency_total_ms),
            ),
            cache_hit_rate: (s.cache_prompt > 0)
                .then(|| s.cache_read as f64 / s.cache_prompt as f64),
            error_rate: (s.requests > 0).then(|| s.errors as f64 / s.requests as f64),
        }
    }
}

/// Totals with zeroed buckets for a window of `buckets`.
pub fn empty(buckets: usize) -> Stats {
    Stats {
        counts: vec![0; buckets],
        error_counts: vec![0; buckets],
        token_counts: vec![0; buckets],
        ..Default::default()
    }
}

fn add_reported(to: &mut Option<u64>, from: Option<u64>) {
    // Totals a source did not report, as older devices, add nothing.
    if let Some(value) = from {
        *to = Some(to.unwrap_or(0).saturating_add(value));
    }
}

/// Adds `from`'s totals to `to`; its buckets land `offset` buckets along
/// `to`'s, and those outside `to`'s buckets fall off.
fn add_at(to: &mut Stats, from: &Stats, offset: i64) {
    to.requests = to.requests.saturating_add(from.requests);
    to.errors = to.errors.saturating_add(from.errors);
    to.bytes = to.bytes.saturating_add(from.bytes);
    to.cache_read = to.cache_read.saturating_add(from.cache_read);
    to.cache_prompt = to.cache_prompt.saturating_add(from.cache_prompt);
    to.latency_total_ms = to.latency_total_ms.saturating_add(from.latency_total_ms);
    to.latency_samples = to.latency_samples.saturating_add(from.latency_samples);
    for (to, from) in [
        (&mut to.input_tokens, from.input_tokens),
        (&mut to.output_tokens, from.output_tokens),
        (&mut to.cached_input_tokens, from.cached_input_tokens),
        (&mut to.uncached_input_tokens, from.uncached_input_tokens),
        (&mut to.cache_write_tokens, from.cache_write_tokens),
        (&mut to.first_token_total_ms, from.first_token_total_ms),
        (&mut to.first_token_samples, from.first_token_samples),
        (
            &mut to.forwarded_first_token_total_ms,
            from.forwarded_first_token_total_ms,
        ),
        (
            &mut to.forwarded_first_token_samples,
            from.forwarded_first_token_samples,
        ),
        (&mut to.cooldown_total_ms, from.cooldown_total_ms),
        (&mut to.cooldown_samples, from.cooldown_samples),
        (&mut to.output_tps_tokens, from.output_tps_tokens),
        (&mut to.output_tps_duration_ms, from.output_tps_duration_ms),
    ] {
        add_reported(to, from);
    }
    for (to, from) in [
        (&mut to.counts, &from.counts),
        (&mut to.error_counts, &from.error_counts),
        (&mut to.token_counts, &from.token_counts),
    ] {
        for (i, value) in from.iter().enumerate() {
            let slot = i as i64 + offset;
            if let Some(to) = usize::try_from(slot).ok().and_then(|slot| to.get_mut(slot)) {
                *to = to.saturating_add(*value);
            }
        }
    }
}

/// Adds totals over the same window, bucket by bucket. Empty buckets take
/// `from`'s length.
pub fn add(to: &mut Stats, from: &Stats) {
    for buckets in [&mut to.counts, &mut to.error_counts, &mut to.token_counts] {
        if buckets.is_empty() {
            buckets.resize(
                from.counts
                    .len()
                    .max(from.error_counts.len())
                    .max(from.token_counts.len()),
                0,
            );
        }
    }
    add_at(to, from, 0);
}

/// Adds one counted occurrence to a total this version reports.
pub fn count(total: &mut Option<u64>, value: u64) {
    add_reported(total, Some(value));
}

type ModelOnly = fn(&mut Stats) -> &mut Option<u64>;
/// Totals measured only for model calls, and so absent from All traffic.
const MODEL_ONLY: [ModelOnly; 8] = [
    |s| &mut s.first_token_total_ms,
    |s| &mut s.first_token_samples,
    |s| &mut s.forwarded_first_token_total_ms,
    |s| &mut s.forwarded_first_token_samples,
    |s| &mut s.cooldown_total_ms,
    |s| &mut s.cooldown_samples,
    |s| &mut s.output_tps_tokens,
    |s| &mut s.output_tps_duration_ms,
];

/// Totals as another device reported them: All traffic has no model-only
/// totals, and a model-only total it did not report stays unknown.
fn scoped(stats: &mut Stats, scope: TrafficScope) {
    if scope == TrafficScope::All {
        for total in MODEL_ONLY {
            *total(stats) = None;
        }
    }
}

/// Totals this version counted itself: with Models it reports every
/// model-only total, measured or zero; All traffic has none.
pub fn reported(stats: &mut Stats, scope: TrafficScope) {
    for total in MODEL_ONLY {
        let total = total(stats);
        *total = match scope {
            TrafficScope::Model => Some(total.unwrap_or(0)),
            TrafficScope::All => None,
        };
    }
}

/// One account's traffic within a device's or Merged Traffic.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub service: Service,
    pub credential: String,
    pub sources: Vec<SourceTraffic>,
    #[serde(flatten)]
    pub stats: Stats,
    #[serde(flatten)]
    pub metrics: Metrics,
}
impl Account {
    pub fn new(
        service: Service,
        credential: String,
        sources: Vec<SourceTraffic>,
        stats: Stats,
    ) -> Self {
        Self {
            service,
            credential,
            sources,
            stats,
            metrics: Metrics::default(),
        }
    }
}

/// Device traffic, or Merged Traffic: totals and metrics over a window, and
/// the same for each account.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub scope: TrafficScope,
    #[serde(flatten)]
    pub window: Window,
    #[serde(flatten)]
    pub stats: Stats,
    #[serde(flatten)]
    pub metrics: Metrics,
    pub credentials: Vec<Account>,
    /// Current configurations that historical traffic can be assigned to.
    pub targets: Vec<Target>,
}
impl View {
    /// Another device's traffic, from the totals of its accounts.
    pub fn reported_by_peer(
        scope: TrafficScope,
        window: Window,
        credentials: Vec<Account>,
    ) -> Self {
        Self::of_accounts(scope, window, credentials, Vec::new(), scoped)
    }

    /// This device's traffic, from the totals it counted for its accounts.
    pub fn counted_here(
        scope: TrafficScope,
        window: Window,
        credentials: Vec<Account>,
        targets: Vec<Target>,
    ) -> Self {
        Self::of_accounts(scope, window, credentials, targets, reported)
    }

    fn of_accounts(
        scope: TrafficScope,
        window: Window,
        mut credentials: Vec<Account>,
        targets: Vec<Target>,
        report: fn(&mut Stats, TrafficScope),
    ) -> Self {
        let mut stats = empty(window.buckets());
        report(&mut stats, scope);
        for account in &mut credentials {
            report(&mut account.stats, scope);
            add(&mut stats, &account.stats);
        }
        finish_accounts(&mut credentials);
        Self {
            scope,
            window,
            metrics: Metrics::of(&stats),
            stats,
            credentials,
            targets,
        }
    }
}

/// Derives each account's metrics and orders them as every view does: known
/// accounts by received bytes, largest first, then Unidentified.
fn finish_accounts(accounts: &mut [Account]) {
    for account in accounts.iter_mut() {
        account.metrics = Metrics::of(&account.stats);
    }
    // Ties fall back to a fixed order, so no account's place depends on which
    // device was read first.
    accounts.sort_by(|a, b| {
        (a.credential == UNIDENTIFIED)
            .cmp(&(b.credential == UNIDENTIFIED))
            .then(b.stats.bytes.cmp(&a.stats.bytes))
            .then(b.stats.requests.cmp(&a.stats.requests))
            .then(a.service.cmp(&b.service))
            .then_with(|| a.credential.cmp(&b.credential))
    });
}

/// Merged Traffic: the sum of every device's traffic, each given once. The
/// newest view's window is the time axis; an older view's buckets line up by
/// time, its oldest ones falling off and its missing recent ones staying
/// empty. An account named alike on several devices is one account.
pub fn merge<'a>(views: impl IntoIterator<Item = &'a View>) -> Option<View> {
    let views: Vec<&View> = views.into_iter().collect();
    let axis = views
        .iter()
        .copied()
        .reduce(|newest, view| {
            if view.window.end > newest.window.end {
                view
            } else {
                newest
            }
        })?
        .window;
    let mut stats = empty(axis.buckets());
    let mut credentials: Vec<Account> = Vec::new();
    for view in &views {
        let offset = axis.offset_of(&view.window);
        add_at(&mut stats, &view.stats, offset);
        for account in &view.credentials {
            let i = match credentials
                .iter()
                .position(|c| c.service == account.service && c.credential == account.credential)
            {
                Some(i) => i,
                None => {
                    credentials.push(Account::new(
                        account.service,
                        account.credential.clone(),
                        Vec::new(),
                        empty(axis.buckets()),
                    ));
                    credentials.len() - 1
                }
            };
            let merged = &mut credentials[i];
            add_at(&mut merged.stats, &account.stats, offset);
            merged.sources.extend(account.sources.iter().cloned());
        }
    }
    finish_accounts(&mut credentials);
    Some(View {
        scope: views[0].scope,
        window: axis,
        metrics: Metrics::of(&stats),
        stats,
        credentials,
        targets: views
            .iter()
            .find(|view| !view.targets.is_empty())
            .map(|view| view.targets.clone())
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: i64 = 60_000;

    fn stats(requests: u64, buckets: &[u64]) -> Stats {
        Stats {
            requests,
            counts: buckets.to_vec(),
            error_counts: vec![0; buckets.len()],
            token_counts: vec![0; buckets.len()],
            ..Default::default()
        }
    }
    fn window(start_minute: i64, end_minute: i64) -> Window {
        Window {
            start: start_minute * MINUTE,
            end: end_minute * MINUTE,
            bucket_minutes: 1,
        }
    }
    fn view(window: Window, accounts: Vec<(&str, Stats)>) -> View {
        View::reported_by_peer(
            TrafficScope::Model,
            window,
            accounts
                .into_iter()
                .map(|(credential, stats)| {
                    Account::new(Service::Codex, credential.into(), Vec::new(), stats)
                })
                .collect(),
        )
    }

    #[test]
    fn merged_traffic_is_exactly_the_sum_of_the_device_views() {
        let totals = |requests: u64, input: Option<u64>| Stats {
            requests,
            errors: 1,
            bytes: requests * 10,
            latency_total_ms: requests * 100,
            latency_samples: requests,
            cache_read: 1,
            cache_prompt: 4,
            counts: vec![requests, 0],
            error_counts: vec![1, 0],
            token_counts: vec![0, requests],
            input_tokens: input,
            ..Default::default()
        };
        let both = window(0, 2);
        let mut local = View::reported_by_peer(
            TrafficScope::Model,
            both,
            vec![
                Account::new(
                    Service::Codex,
                    "shared".into(),
                    vec![SourceTraffic::new("old", None, None, 2)],
                    totals(2, Some(20)),
                ),
                Account::new(
                    Service::Codex,
                    UNIDENTIFIED.into(),
                    Vec::new(),
                    totals(1, Some(10)),
                ),
            ],
        );
        local.targets = vec![Target::named("target")];
        let peer = view(
            both,
            vec![(UNIDENTIFIED, totals(1, None)), ("shared", totals(4, None))],
        );
        let total = merge([&local, &peer]).unwrap();
        let s = &total.stats;
        assert_eq!(
            (s.requests, s.errors, s.bytes),
            (8, 4, 80),
            "each account adds one error"
        );
        assert_eq!(
            (&s.counts, &s.error_counts, &s.token_counts),
            (&vec![8, 0], &vec![4, 0], &vec![0, 8])
        );
        assert_eq!(total.metrics.avg_ms, Some(100));
        assert_eq!(total.metrics.cache_hit_rate, Some(4.0 / 16.0));
        assert_eq!(total.metrics.error_rate, Some(4.0 / 8.0));
        // Usage reported by some devices counts; none reported stays unknown.
        assert_eq!(s.input_tokens, Some(30));
        assert_eq!(s.output_tokens, None);
        // An account named alike on both devices is one account; Unidentified is one entry, last.
        let accounts: Vec<_> = total
            .credentials
            .iter()
            .map(|c| (c.credential.as_str(), c.stats.requests, c.stats.bytes))
            .collect();
        assert_eq!(accounts, [("shared", 6, 60), (UNIDENTIFIED, 2, 20)]);
        assert_eq!(total.credentials[0].sources.len(), 1);
        assert_eq!(total.targets.len(), 1);
    }

    #[test]
    fn accounts_alike_in_bytes_keep_one_order_whichever_device_comes_first() {
        let account = |service, credential: &str, requests| {
            let mut stats = stats(requests, &[requests]);
            stats.bytes = 100;
            Account::new(service, credential.into(), Vec::new(), stats)
        };
        let one = window(0, 1);
        let local = View::reported_by_peer(
            TrafficScope::Model,
            one,
            vec![
                account(Service::Claude, "b", 1),
                account(Service::Codex, UNIDENTIFIED, 9),
            ],
        );
        let peer = View::reported_by_peer(
            TrafficScope::Model,
            one,
            vec![
                account(Service::Codex, "z", 1),
                account(Service::Claude, "a", 1),
                account(Service::Codex, "busy", 2),
            ],
        );
        let order = |views: [&View; 2]| -> Vec<(Service, String)> {
            merge(views)
                .unwrap()
                .credentials
                .into_iter()
                .map(|account| (account.service, account.credential))
                .collect()
        };
        let expected = [
            (Service::Codex, "busy"),
            (Service::Codex, "z"),
            (Service::Claude, "a"),
            (Service::Claude, "b"),
            (Service::Codex, UNIDENTIFIED),
        ]
        .map(|(service, credential)| (service, credential.to_owned()));
        assert_eq!(order([&local, &peer]), expected);
        assert_eq!(order([&peer, &local]), expected);
    }

    #[test]
    fn merged_traffic_lines_up_parts_read_at_different_times_by_time_not_index() {
        // This device was read now; the peer at the last refresh, two minutes earlier.
        let local = view(window(10, 14), vec![("shared", stats(4, &[1, 1, 1, 1]))]);
        let peer = view(
            window(8, 12),
            vec![("shared", stats(100, &[10, 20, 30, 40]))],
        );
        let total = merge([&local, &peer]).unwrap();
        // Peer minutes 10 and 11 land on the axis' first two bars; its older ones fall off,
        // and the two recent minutes it has not reported yet stay as this device alone.
        assert_eq!(total.window, window(10, 14));
        assert_eq!(total.stats.counts, [31, 41, 1, 1]);
        assert_eq!(total.credentials[0].stats.counts, [31, 41, 1, 1]);
        // Totals add whole even where buckets fall off.
        assert_eq!(total.stats.requests, 104);
        // The newest part sets the axis wherever it is given.
        assert_eq!(merge([&peer, &local]).unwrap().stats.counts, [31, 41, 1, 1]);
        // Aligned windows add index by index.
        let aligned = view(
            window(10, 14),
            vec![("shared", stats(100, &[10, 20, 30, 40]))],
        );
        assert_eq!(
            merge([&local, &aligned]).unwrap().stats.counts,
            [11, 21, 31, 41]
        );
    }

    #[test]
    fn a_window_a_little_off_a_minute_lines_up_with_the_nearest_bucket() {
        let axis = Window {
            start: 10 * MINUTE + 25_000,
            end: 14 * MINUTE + 25_000,
            bucket_minutes: 1,
        };
        assert_eq!(axis.offset_of(&window(8, 12)), -2);
        assert_eq!(axis.offset_of(&window(11, 15)), 1);
        // Half a bucket rounds up, negative or not.
        let half = |start| Window {
            start,
            end: start + 4 * MINUTE,
            bucket_minutes: 1,
        };
        assert_eq!(half(0).offset_of(&half(30_000)), 1);
        assert_eq!(half(0).offset_of(&half(-30_000)), 0);
        assert_eq!(half(0).offset_of(&half(-90_000)), -1);
    }

    fn first_token(total: Option<u64>, samples: Option<u64>) -> Stats {
        Stats {
            first_token_total_ms: total,
            first_token_samples: samples,
            ..stats(1, &[1])
        }
    }

    #[test]
    fn merged_first_token_averages_the_devices_that_report_it() {
        let one = window(0, 1);
        let current = view(one, vec![("shared", first_token(Some(900), Some(2)))]);
        let quiet = view(one, vec![("shared", first_token(Some(0), Some(0)))]);
        let older = view(one, vec![("shared", first_token(None, None))]);
        let total = merge([&current, &older, &quiet]).unwrap();
        assert_eq!(total.stats.requests, 3);
        assert_eq!(
            (
                total.stats.first_token_total_ms,
                total.stats.first_token_samples
            ),
            (Some(900), Some(2))
        );
        assert_eq!(total.metrics.first_token_ms, Some(450));
        assert_eq!(total.credentials[0].metrics.first_token_ms, Some(450));
        // Only older devices: not reported at all.
        let old = merge([&older]).unwrap();
        assert_eq!(old.stats.first_token_samples, None);
        assert_eq!(old.metrics.first_token_ms, None);
        // Reported without streamed output: reported, but unknown.
        let quiet = merge([&quiet]).unwrap();
        assert_eq!(quiet.stats.first_token_samples, Some(0));
        assert_eq!(quiet.metrics.first_token_ms, None);
    }

    #[test]
    fn first_token_keeps_historic_samples_beside_forwarded_ones() {
        let one = window(0, 1);
        let part = |service, total, samples, forwarded: Option<(u64, u64)>| {
            let stats = Stats {
                forwarded_first_token_total_ms: forwarded.map(|f| f.0),
                forwarded_first_token_samples: forwarded.map(|f| f.1),
                ..first_token(Some(total), Some(samples))
            };
            View::reported_by_peer(
                TrafficScope::Model,
                one,
                vec![Account::new(service, "work".into(), Vec::new(), stats)],
            )
        };
        // Aggregate-only reproduction of the frozen 24-hour audit; no identities.
        let total = merge(&[
            part(Service::Codex, 7533138, 1023, Some((709946, 92))),
            part(Service::Antigravity, 16230735, 2763, Some((0, 0))),
            part(Service::Claude, 958942, 393, None),
            part(Service::Grok, 616150, 179, None),
        ])
        .unwrap();
        assert_eq!(total.stats.first_token_total_ms, Some(25338965));
        assert_eq!(total.stats.first_token_samples, Some(4358));
        assert_eq!(total.stats.forwarded_first_token_samples, Some(92));
        assert_eq!(total.metrics.first_token_ms, Some(5814));
        let codex = total
            .credentials
            .iter()
            .find(|c| c.service == Service::Codex)
            .unwrap();
        assert_eq!(codex.metrics.first_token_ms, Some(7363));
        // A measured zero is valid.
        assert_eq!(
            Metrics::of(&first_token(Some(0), Some(1))).first_token_ms,
            Some(0)
        );
    }

    #[test]
    fn output_tps_merges_paired_generation_totals() {
        let one = window(0, 1);
        let part = |tokens: Option<u64>, ms: Option<u64>| {
            let stats = Stats {
                output_tokens: Some(9999),
                latency_total_ms: 99999,
                output_tps_tokens: tokens,
                output_tps_duration_ms: ms,
                ..stats(1, &[1])
            };
            view(one, vec![("work", stats)])
        };
        let total = merge(&[
            part(Some(100), Some(2000)),
            part(Some(300), Some(8000)),
            part(None, None),
            part(Some(0), Some(0)),
        ])
        .unwrap();
        assert_eq!(total.metrics.output_tps, Some(40.0));
        assert_eq!(total.credentials[0].metrics.output_tps, Some(40.0));
        for unknown in [part(None, None), part(Some(0), Some(0))] {
            assert_eq!(merge([&unknown]).unwrap().metrics.output_tps, None);
        }
    }

    #[test]
    fn output_tps_from_request_start_divides_all_output_by_all_durations() {
        let one = window(0, 1);
        let part = |output, latency, tokens, ms| {
            view(
                one,
                vec![(
                    "work",
                    Stats {
                        output_tokens: Some(output),
                        latency_total_ms: latency,
                        latency_samples: 1,
                        output_tps_tokens: Some(tokens),
                        output_tps_duration_ms: Some(ms),
                        ..stats(1, &[1])
                    },
                )],
            )
        };
        let merged = merge(&[part(100, 1000, 99, 500), part(300, 9000, 299, 7500)]).unwrap();
        assert_eq!(merged.metrics.output_tps_from_start, Some(40.0));
        assert_eq!(merged.metrics.output_tps, Some(49.75));
        let from_start = |output, latency| {
            Metrics::of(&Stats {
                output_tokens: output,
                latency_total_ms: latency,
                ..Default::default()
            })
            .output_tps_from_start
        };
        assert_eq!(from_start(Some(1000), 20000), Some(50.0));
        for (output, latency) in [(None, 20000), (Some(0), 20000), (Some(1000), 0)] {
            assert_eq!(from_start(output, latency), None);
        }
    }

    #[test]
    fn all_traffic_has_no_model_only_totals() {
        let mut peer = first_token(Some(900), Some(2));
        peer.output_tps_tokens = Some(10);
        let all = View::reported_by_peer(
            TrafficScope::All,
            window(0, 1),
            vec![Account::new(
                Service::Codex,
                "work".into(),
                Vec::new(),
                peer.clone(),
            )],
        );
        assert_eq!(all.stats.first_token_samples, None);
        assert_eq!(all.credentials[0].stats.output_tps_tokens, None);
        assert_eq!(all.metrics.first_token_ms, None);
        // With Models another device's unreported totals stay unknown ...
        let older = View::reported_by_peer(TrafficScope::Model, window(0, 1), Vec::new());
        assert_eq!(older.stats.first_token_samples, None);
        // ... while this device reports every one, measured or zero.
        let here = View::counted_here(TrafficScope::Model, window(0, 1), Vec::new(), Vec::new());
        assert_eq!(here.stats.first_token_samples, Some(0));
        assert_eq!(here.stats.output_tps_tokens, Some(0));
        assert_eq!(here.metrics.first_token_ms, None);
        assert_eq!(here.metrics.output_tps, None);
        let here = View::counted_here(TrafficScope::All, window(0, 1), Vec::new(), Vec::new());
        assert_eq!(here.stats.first_token_samples, None);
    }

    #[test]
    fn error_rate_is_unknown_without_requests() {
        let rate = |requests, errors| {
            Metrics::of(&Stats {
                requests,
                errors,
                ..Default::default()
            })
            .error_rate
        };
        assert_eq!(rate(0, 0), None);
        assert_eq!(rate(4, 0), Some(0.0));
        assert_eq!(rate(4, 1), Some(0.25));
    }

    #[test]
    fn a_view_serializes_its_window_totals_and_metrics_flat() {
        let view = view(window(0, 1), vec![("work", first_token(Some(10), Some(1)))]);
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["start"], 0);
        assert_eq!(json["end"], MINUTE);
        assert_eq!(json["bucketMinutes"], 1);
        assert_eq!(json["scope"], "model");
        assert_eq!(json["requests"], 1);
        assert_eq!(json["firstTokenMs"], 10);
        assert_eq!(json["errorRate"], 0.0);
        assert_eq!(json["inputTokens"], serde_json::Value::Null);
        assert_eq!(json["credentials"][0]["service"], "Codex");
        assert_eq!(json["credentials"][0]["credential"], "work");
        assert_eq!(json["credentials"][0]["firstTokenMs"], 10);
        // Unreported model-only totals are absent, not zero.
        assert!(json.get("outputTpsTokens").is_none());
    }
}
