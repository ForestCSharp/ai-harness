//! Cumulative token accounting for a session.
//!
//! The per-turn `Usage` the API returns is shown against each reply, but
//! nothing added it up: there was no way to see what a session had cost in
//! total. This module keeps the running totals, and is deliberately the only
//! place that knows how to turn them into dollars — which it will only do when
//! given prices, since rates change and a hardcoded table goes stale silently.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::openrouter::Usage;

/// Running totals for a session.
///
/// Serialised into the session file, so a reloaded session keeps its history of
/// spend. Counts are `u64` rather than the API's `u32` because they accumulate
/// across every request in a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Model round-trips that reported usage.
    pub requests: u64,
    /// Milliseconds spent with a request actually in flight.
    ///
    /// Not wall-clock since the session began: a session that sat idle
    /// overnight would report a meaningless number. Time spent waiting on the
    /// model is the figure that means something.
    pub waiting_ms: u64,
}

impl Ledger {
    /// Fold in one reply's usage.
    pub fn record(&mut self, usage: &Usage) {
        self.prompt_tokens += u64::from(usage.prompt_tokens);
        self.completion_tokens += u64::from(usage.completion_tokens);
        self.requests += 1;
    }

    /// Add time spent on a request. Cancelled and failed requests count too —
    /// that time was really spent.
    pub fn add_wait(&mut self, elapsed: Duration) {
        self.waiting_ms += elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
    }

    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }

    pub fn is_empty(&self) -> bool {
        self.requests == 0
    }

    /// Estimated spend in dollars, given per-million-token prices.
    ///
    /// `None` unless **both** prices are supplied: input and output rates
    /// differ several-fold, so a figure derived from one of them would not be
    /// an estimate, it would be wrong.
    pub fn cost(&self, price_in: Option<f64>, price_out: Option<f64>) -> Option<f64> {
        let (input, output) = (price_in?, price_out?);
        const PER: f64 = 1_000_000.0;
        Some(
            (self.prompt_tokens as f64 / PER) * input
                + (self.completion_tokens as f64 / PER) * output,
        )
    }

    /// A short indicator for the status bar, e.g. `36.7k tok  $0.04`.
    pub fn status_line(&self, price_in: Option<f64>, price_out: Option<f64>) -> String {
        let mut text = format!("{} tok", compact(self.total_tokens()));
        if let Some(cost) = self.cost(price_in, price_out) {
            text.push_str(&format!("  {}", money(cost)));
        }
        text
    }

    /// The multi-line breakdown `/cost` prints.
    pub fn report(&self, price_in: Option<f64>, price_out: Option<f64>) -> String {
        if self.is_empty() {
            return "No model requests yet this session.".to_string();
        }
        let mut text = format!(
            "Session totals\n\
             \x20 requests       {}\n\
             \x20 input tokens   {}\n\
             \x20 output tokens  {}\n\
             \x20 total tokens   {}\n\
             \x20 time waiting   {}",
            self.requests,
            thousands(self.prompt_tokens),
            thousands(self.completion_tokens),
            thousands(self.total_tokens()),
            duration(self.waiting_ms),
        );
        match self.cost(price_in, price_out) {
            Some(cost) => text.push_str(&format!(
                "\n  estimated cost {} (at ${}/M in, ${}/M out)",
                money(cost),
                price_in.unwrap_or(0.0),
                price_out.unwrap_or(0.0)
            )),
            None => text.push_str(
                "\n  Set --price-in and --price-out (dollars per million tokens) \
                 for a cost estimate.",
            ),
        }
        text
    }
}

/// `1234` → `1.2k`, `1_234_567` → `1.23M`. For places where width is scarce.
///
/// Truncates rather than rounding, so a value just under a unit boundary cannot
/// render as `1000.0k` instead of crossing to `M`.
fn compact(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{}.{}k", n / 1_000, (n % 1_000) / 100),
        _ => format!("{}.{:02}M", n / 1_000_000, (n % 1_000_000) / 10_000),
    }
}

/// `1234567` → `1,234,567`. For the report, where exactness is the point.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Dollars, with enough precision to be useful at small amounts.
fn money(dollars: f64) -> String {
    if dollars < 0.01 && dollars > 0.0 {
        format!("${dollars:.4}")
    } else {
        format!("${dollars:.2}")
    }
}

fn duration(ms: u64) -> String {
    let total = ms / 1000;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(prompt: u32, completion: u32) -> Usage {
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
        }
    }

    #[test]
    fn recording_accumulates_across_requests() {
        let mut ledger = Ledger::default();
        assert!(ledger.is_empty());

        ledger.record(&usage(100, 20));
        ledger.record(&usage(250, 30));

        assert_eq!(ledger.prompt_tokens, 350);
        assert_eq!(ledger.completion_tokens, 50);
        assert_eq!(ledger.total_tokens(), 400);
        assert_eq!(ledger.requests, 2);
        assert!(!ledger.is_empty());
    }

    #[test]
    fn waiting_time_accumulates() {
        let mut ledger = Ledger::default();
        ledger.add_wait(Duration::from_millis(1500));
        ledger.add_wait(Duration::from_millis(500));
        assert_eq!(ledger.waiting_ms, 2000);
    }

    #[test]
    fn cost_needs_both_prices() {
        let mut ledger = Ledger::default();
        ledger.record(&usage(1_000_000, 1_000_000));

        assert_eq!(ledger.cost(None, None), None);
        assert_eq!(
            ledger.cost(Some(1.0), None),
            None,
            "half a price is not an estimate"
        );
        assert_eq!(ledger.cost(None, Some(2.0)), None);
        assert_eq!(ledger.cost(Some(1.0), Some(2.0)), Some(3.0));
    }

    #[test]
    fn cost_weights_input_and_output_separately() {
        let mut ledger = Ledger::default();
        // 2M in at $3, 0.5M out at $10 → $6 + $5.
        ledger.record(&usage(2_000_000, 500_000));
        let cost = ledger.cost(Some(3.0), Some(10.0)).unwrap();
        assert!((cost - 11.0).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn an_empty_ledger_reports_plainly_rather_than_showing_zeroes() {
        let report = Ledger::default().report(None, None);
        assert!(report.contains("No model requests yet"), "{report}");
    }

    #[test]
    fn the_report_names_the_missing_prices_when_cost_is_unknown() {
        let mut ledger = Ledger::default();
        ledger.record(&usage(10, 2));
        let report = ledger.report(None, None);
        assert!(report.contains("--price-in"), "{report}");
        assert!(!report.contains("estimated cost"), "{report}");
    }

    #[test]
    fn the_report_shows_cost_when_priced() {
        let mut ledger = Ledger::default();
        ledger.record(&usage(1_000_000, 0));
        let report = ledger.report(Some(2.5), Some(10.0));
        assert!(report.contains("estimated cost"), "{report}");
        assert!(report.contains("$2.50"), "{report}");
    }

    #[test]
    fn the_status_line_stays_short() {
        let mut ledger = Ledger::default();
        ledger.record(&usage(34_500, 2_200));
        assert_eq!(ledger.status_line(None, None), "36.7k tok");
        assert!(ledger.status_line(Some(1.0), Some(1.0)).contains('$'));
    }

    #[test]
    fn compact_switches_units_at_the_right_thresholds() {
        assert_eq!(compact(0), "0");
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1_000), "1.0k");
        assert_eq!(compact(36_700), "36.7k");
        // Must not round up past its own unit.
        assert_eq!(compact(999_999), "999.9k");
        assert_eq!(compact(1_240_000), "1.24M");
    }

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn small_amounts_keep_enough_precision_to_be_useful() {
        // Rounding a fraction of a cent to $0.00 would tell the user nothing.
        assert_eq!(money(0.0042), "$0.0042");
        assert_eq!(money(1.5), "$1.50");
        assert_eq!(money(0.0), "$0.00");
    }

    #[test]
    fn durations_scale_by_magnitude() {
        assert_eq!(duration(4_000), "4s");
        assert_eq!(duration(74_000), "1m 14s");
        assert_eq!(duration(7_400_000), "2h 3m");
    }

    #[test]
    fn a_ledger_round_trips_through_json() {
        let mut ledger = Ledger::default();
        ledger.record(&usage(7, 3));
        ledger.add_wait(Duration::from_millis(250));

        let json = serde_json::to_string(&ledger).unwrap();
        assert_eq!(serde_json::from_str::<Ledger>(&json).unwrap(), ledger);
    }
}
