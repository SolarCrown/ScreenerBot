// Copyright (c) 2024-2026 ScreenerBot (screenerbot.io)
// SPDX-License-Identifier: BUSL-1.1

//! Wallet-credential resolution and the one cache that may ever hold a
//! decrypted Solana keypair in memory.
//!
//! Shared code (`crate::wallets`, `crate::config::wallet`, the webserver)
//! never sees a `Keypair`: it relies on "the main wallet" through the
//! functions here, which resolve the wallet's
//! encrypted credential via `crate::wallets` (ciphertext + nonce only, never
//! Solana-typed), decrypt, use, and drop. The one exception is the main
//! wallet, cached here — not in `crate::wallets` — because it is read on
//! every trade. The main wallet is fixed for the life of the process: it is
//! chosen once, when the wallet store first imports the configured key.

use std::sync::LazyLock;
use tokio::sync::RwLock;

use crate::chains::solana::solana_sdk::pubkey::Pubkey;
use crate::chains::solana::solana_sdk::signature::{Keypair, Signer};
use crate::chains::solana::{Error, Result};

use super::keypair::decrypt_to_keypair;
use zeroize::Zeroizing;

struct CachedMainKeypair {
    wallet_id: i64,
    keypair: Keypair,
}

static MAIN_KEYPAIR_CACHE: LazyLock<RwLock<Option<CachedMainKeypair>>> =
    LazyLock::new(|| RwLock::new(None));

/// Drop the cached trading key. Research mode calls this as it latches, so a
/// key decrypted before the latch does not stay resident. If a reader holds
/// the lock, that reader re-checks the latch before it returns a copy.
pub(crate) fn discard_cached_main_keypair() {
    if let Ok(mut cache) = MAIN_KEYPAIR_CACHE.try_write() {
        *cache = None;
    }
}

/// Refuse key material in research mode and inside a paper scope. Every
/// accessor calls this before any key or database read, and again before it
/// returns key material, so a latch that flips during the read still refuses.
pub(super) fn refuse_key_access(operation: &'static str) -> Result<()> {
    if crate::global::in_research_mode() {
        return Err(Error::ResearchModeRefused { operation });
    }
    if crate::global::in_paper_scope() {
        return Err(Error::PaperScopeRefused { operation });
    }
    Ok(())
}

/// A second `Keypair` with the same secret. The transient byte copy is wiped
/// when it goes out of scope; `Keypair` wipes its own secret on drop.
fn clone_keypair(keypair: &Keypair) -> Result<Keypair> {
    let bytes = Zeroizing::new(keypair.to_bytes());
    Keypair::try_from(&bytes[..]).map_err(|e| Error::InvalidKeypair {
        detail: format!("failed to clone keypair: {e}"),
    })
}

/// The main wallet's keypair, decrypted on first use and cached for the life
/// of the process. Every caller gets its own clone; nothing outside this
/// module ever holds the cached original.
pub async fn main_keypair() -> Result<Keypair> {
    refuse_key_access("main_keypair")?;
    {
        let cache = MAIN_KEYPAIR_CACHE.read().await;
        if let Some(cached) = cache.as_ref() {
            let clone = clone_keypair(&cached.keypair)?;
            drop(cache);
            refuse_key_access("main_keypair")?;
            return Ok(clone);
        }
    }

    let (wallet_id, ciphertext, nonce) = crate::wallets::get_main_wallet_encrypted_key()
        .await
        .map_err(|e| Error::KeypairUnavailable {
            detail: e.to_string(),
        })?
        .ok_or_else(|| Error::KeypairUnavailable {
            detail: "no main wallet configured".to_owned(),
        })?;

    let keypair = decrypt_to_keypair(&ciphertext, &nonce)?;
    let clone = clone_keypair(&keypair)?;
    refuse_key_access("main_keypair")?;

    *MAIN_KEYPAIR_CACHE.write().await = Some(CachedMainKeypair { wallet_id, keypair });
    if let Err(error) = refuse_key_access("main_keypair") {
        discard_cached_main_keypair();
        return Err(error);
    }
    Ok(clone)
}

/// The main wallet's database ID, if a keypair is currently cached — cheap
/// identity check that never touches key material.
pub async fn cached_main_wallet_id() -> Option<i64> {
    if crate::global::in_research_mode() {
        discard_cached_main_keypair();
        return None;
    }
    MAIN_KEYPAIR_CACHE
        .read()
        .await
        .as_ref()
        .map(|c| c.wallet_id)
}

/// Decrypt a listed wallet's keypair by its database ID for one message
/// signature. Not cached and never used to sign a transaction: only the main
/// wallet signs swaps and transfers.
async fn keypair_for_wallet(wallet_id: i64) -> Result<Keypair> {
    refuse_key_access("keypair_for_wallet")?;
    let (ciphertext, nonce) = crate::wallets::get_wallet_encrypted_key(wallet_id)
        .await
        .map_err(|e| Error::KeypairUnavailable {
            detail: e.to_string(),
        })?;
    decrypt_to_keypair(&ciphertext, &nonce)
}

/// Sign an arbitrary text message with the main wallet. The one place the
/// bot signs something that is not a transaction: proving wallet ownership
/// to screenerbot.io during account sign-in.
pub async fn sign_message_with_main_wallet(message: &str) -> Result<String> {
    refuse_key_access("sign_message_with_main_wallet")?;
    let keypair = main_keypair().await?;
    Ok(keypair.sign_message(message.as_bytes()).to_string())
}

/// Sign a per-address message with every active wallet's key, for the
/// referral activation announce: proves ownership of every held wallet
/// without ever handing the keys themselves to the caller. A wallet whose
/// key fails to decrypt is skipped (and logged), not fatal to the batch.
pub async fn sign_message_for_active_wallets(
    message_for: impl Fn(&str) -> String,
) -> Result<Vec<(String, String)>> {
    refuse_key_access("sign_message_for_active_wallets")?;
    let wallets =
        crate::wallets::list_active_wallets()
            .await
            .map_err(|e| Error::KeypairUnavailable {
                detail: e.to_string(),
            })?;
    let mut signed = Vec::with_capacity(wallets.len());

    for wallet in wallets {
        match keypair_for_wallet(wallet.id).await {
            Ok(keypair) => {
                let message = message_for(&wallet.address);
                signed.push((
                    wallet.address,
                    keypair.sign_message(message.as_bytes()).to_string(),
                ));
            }
            Err(err) => {
                crate::logger::warning(
                    crate::logger::LogTag::Wallet,
                    &format!(
                        "Skipping wallet id={} ({}): {err}",
                        wallet.id, wallet.address
                    ),
                );
            }
        }
    }

    Ok(signed)
}

// =============================================================================
// CONFIGURED WALLET (sync bridge for early-startup / non-async callers)
// =============================================================================

/// Async form of [`configured_keypair`], for callers already running on an
/// executor. Prefer this over the sync bridge when the caller is `async fn`.
pub async fn configured_keypair_async() -> Result<Keypair> {
    refuse_key_access("configured_keypair_async")?;
    if crate::wallets::is_initialized().await {
        return main_keypair().await;
    }
    configured_keypair_from_legacy_config()
}

/// Async, key-free form of [`configured_address`]. Once the wallet store is
/// initialized this never touches `main_keypair()` or
/// `MAIN_KEYPAIR_CACHE` — it reads `crate::wallets::get_main_address()`,
/// which resolves the cached wallet record only. Any read-only caller that
/// wants an address (dashboard, wallet-monitor snapshots/metrics) should
/// prefer this over `configured_pubkey`/`configured_keypair`, which force a
/// decrypt.
pub async fn configured_address_async() -> Result<String> {
    if crate::wallets::is_initialized().await {
        return crate::wallets::get_main_address()
            .await
            .map_err(|e| Error::KeypairUnavailable {
                detail: e.to_string(),
            });
    }
    configured_keypair_from_legacy_config().map(|kp| kp.pubkey().to_string())
}

/// The configured trading wallet's keypair — the wallet store's trading
/// wallet once the store is initialized, else the keypair stored directly in
/// `config.toml`. Callable from sync or async context.
///
/// On a multi-thread Tokio runtime this drives the async lookup via
/// `tokio::task::block_in_place`, which tells the runtime this worker thread
/// is about to block so it can move the thread's other queued tasks
/// elsewhere before `futures::executor::block_on` parks it — the safe way to
/// wait synchronously without starving the runtime.
///
/// `block_in_place` panics on a current-thread runtime (there is no other
/// worker to move work to), and blocking there with a bare `block_on` risks
/// a real deadlock if the awaited path ever needs a task that only the
/// current thread could poll. Rather than gamble on that never happening,
/// a current-thread caller gets a clear error telling it to use
/// `configured_keypair_async()` instead — never a hang.
pub fn configured_keypair() -> Result<Keypair> {
    refuse_key_access("configured_keypair")?;
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => match handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(|| {
                futures::executor::block_on(configured_keypair_async())
            }),
            _ => Err(Error::KeypairUnavailable {
                detail: "configured_keypair() cannot run synchronously on a current-thread \
                    Tokio runtime (would risk a deadlock) - call configured_keypair_async() \
                    instead"
                    .to_owned(),
            }),
        },
        Err(_) => configured_keypair_from_legacy_config(),
    }
}

/// Fallback: decrypt the wallet stored directly in `config.toml`, used only
/// before the wallet store initializes.
fn configured_keypair_from_legacy_config() -> Result<Keypair> {
    refuse_key_access("configured_keypair_from_legacy_config")?;
    crate::config::with_config(|cfg| {
        if cfg.wallet_encrypted.is_empty() || cfg.wallet_nonce.is_empty() {
            return Err(Error::KeypairUnavailable {
                detail: "wallet not configured - encrypted private key is missing".to_owned(),
            });
        }

        let encrypted = crate::secure_storage::EncryptedData {
            ciphertext: cfg.wallet_encrypted.clone(),
            nonce: cfg.wallet_nonce.clone(),
        };

        let private_key = crate::secure_storage::decrypt_private_key(&encrypted).map_err(|e| {
            Error::KeypairUnavailable {
                detail: format!("failed to decrypt wallet: {e}"),
            }
        })?;

        super::keypair::parse_private_key(&private_key)
    })
}

/// The configured trading wallet's public key, parsed from
/// [`configured_address`] — never decrypts a keypair merely to derive a
/// public key that is already recoverable from the stored address string.
pub fn configured_pubkey() -> Result<Pubkey> {
    use std::str::FromStr;

    let address = configured_address()?;
    Pubkey::from_str(&address).map_err(|_| Error::InvalidAddress {
        kind: "wallet",
        value: address,
    })
}

/// The configured trading wallet's address as a base58 string — the only
/// one of this trio that shared code outside `crate::chains::solana` may
/// call (`crate::config::get_wallet_pubkey_string`).
///
/// Key-free once the wallet store is initialized (see
/// `configured_address_async`); only the `config.toml` fallback used before
/// then decrypts. Same runtime-flavor bridging as `configured_keypair`.
pub fn configured_address() -> Result<String> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => match handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(|| {
                futures::executor::block_on(configured_address_async())
            }),
            _ => Err(Error::KeypairUnavailable {
                detail: "configured_address() cannot run synchronously on a current-thread \
                    Tokio runtime (would risk a deadlock) - call configured_address_async() \
                    instead"
                    .to_owned(),
            }),
        },
        Err(_) => configured_keypair_from_legacy_config().map(|kp| kp.pubkey().to_string()),
    }
}

/// Stores a fresh key, encrypted, as the configured trading wallet of the loaded config
/// (the default config when none is loaded yet) and returns its address.
#[cfg(test)]
pub(crate) fn configure_test_trading_wallet() -> String {
    let (address, secret) = super::keypair::new_test_key();
    let encrypted =
        crate::secure_storage::encrypt_private_key(&secret).expect("encrypt the test key");
    let config = crate::config::utils::CONFIG
        .get_or_init(|| std::sync::RwLock::new(crate::config::schemas::Config::default()));
    let mut config = config
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    config.wallet_encrypted = encrypted.ciphertext;
    config.wallet_nonce = encrypted.nonce;
    address
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(result: Result<impl Sized>, operation: &'static str) {
        match result {
            Err(Error::PaperScopeRefused { operation: refused }) => {
                assert_eq!(refused, operation)
            }
            Err(other) => panic!("{operation} reached {other} inside a paper scope"),
            Ok(_) => panic!("{operation} returned key material inside a paper scope"),
        }
    }

    #[tokio::test]
    async fn paper_scope_refuses_every_key_accessor() {
        crate::global::paper_scope(async {
            refused(main_keypair().await, "main_keypair");
            refused(keypair_for_wallet(1).await, "keypair_for_wallet");
            refused(
                sign_message_with_main_wallet("message").await,
                "sign_message_with_main_wallet",
            );
            refused(
                sign_message_for_active_wallets(|address| address.to_owned()).await,
                "sign_message_for_active_wallets",
            );
            refused(configured_keypair_async().await, "configured_keypair_async");
            refused(configured_keypair(), "configured_keypair");
            refused(
                configured_keypair_from_legacy_config(),
                "configured_keypair_from_legacy_config",
            );
            refused(
                super::super::keypair::decrypt_to_keypair("ciphertext", "nonce"),
                "decrypt_to_keypair",
            );
            refused(
                super::super::keypair::export_private_key("ciphertext", "nonce"),
                "export_private_key",
            );
            refused(
                super::super::keypair::parse_private_key("key"),
                "parse_private_key",
            );
        })
        .await;
    }

    #[test]
    fn clone_keypair_round_trips_the_public_key() {
        let original = Keypair::new();
        let cloned = clone_keypair(&original).unwrap();
        assert_eq!(original.pubkey(), cloned.pubkey());
    }

    #[tokio::test]
    async fn research_mode_refuses_key_parsing_from_a_spawned_task() {
        let (address, secret) = super::super::keypair::new_test_key();
        let parsed = super::super::keypair::parse_private_key(&secret).expect("parse before latch");
        assert_eq!(parsed.pubkey().to_string(), address);

        crate::global::engage_research_mode();
        let secret_for_task = secret.clone();
        let parsed =
            tokio::spawn(async move { super::super::keypair::parse_private_key(&secret_for_task) })
                .await
                .expect("spawn");
        match parsed {
            Err(Error::ResearchModeRefused { operation }) => {
                assert_eq!(operation, "parse_private_key")
            }
            other => panic!("spawned parse returned {other:?}"),
        }
        let loaded = tokio::spawn(main_keypair()).await.expect("spawn");
        match loaded {
            Err(Error::ResearchModeRefused { operation }) => assert_eq!(operation, "main_keypair"),
            other => panic!("spawned main_keypair returned {other:?}"),
        }
        assert!(cached_main_wallet_id().await.is_none());
    }
}
