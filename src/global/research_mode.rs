// Copyright (c) 2024-2026 ScreenerBot (screenerbot.io)
// SPDX-License-Identifier: BUSL-1.1

//! Process-wide research mode.
//!
//! Paper trading refuses signing only on the task that entered `paper_scope`.
//! A spawned task leaves that scope. Research mode is different: once engaged
//! it stays on for the life of the process, including every task spawned
//! afterwards. The live signer is unchanged until something engages it.
//! Opening the research store and running a research collector both engage it,
//! so backtests and forward tests cannot load a key or submit a swap in a
//! process that is doing research.

use std::sync::atomic::{AtomicBool, Ordering};

static RESEARCH_MODE: AtomicBool = AtomicBool::new(false);

/// Whether this process has engaged research mode.
pub fn in_research_mode() -> bool {
    RESEARCH_MODE.load(Ordering::SeqCst)
}

/// Latch research mode on. There is no way to turn it off. The cached trading
/// key, if one was already decrypted, is dropped. A task that already copied
/// the key re-checks the latch before it returns that copy.
pub fn engage_research_mode() {
    RESEARCH_MODE.store(true, Ordering::SeqCst);
    crate::chains::discard_cached_signer();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn research_mode_stays_on_inside_a_spawned_task() {
        assert!(!in_research_mode());
        let spawned_before = tokio::spawn(async { in_research_mode() }).await.unwrap();
        assert!(!spawned_before);

        engage_research_mode();
        assert!(in_research_mode());

        let spawned_after = tokio::spawn(async {
            tokio::task::yield_now().await;
            in_research_mode()
        })
        .await
        .unwrap();
        assert!(spawned_after);
        assert!(in_research_mode());
    }
}
