//! Telegram bot adapter for CN↔EN translation.
//!
//! Keeps Telegram types out of hymt-core. Pure claim / authorization / language
//! routing is unit-tested without network access; the long-poll loop talks to
//! the Bot API via `reqwest`.

mod logic;
mod poll;

pub use poll::run_telegram_bot;
