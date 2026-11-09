# Security and malware review

Reviewed commit: `f26d8524f` (`chore(release): bump version to 0.2.25`), branch `main` as of 2026-11-09.
Scope: first-party Rust, embedded dashboard JavaScript, Electron shell, `install.sh`, `build.rs`, GitHub workflows, and the root and `electron/` lockfiles.
This review did not change any functional code.

## Verdict

**No malware, backdoor, or hidden fund-drainer was found in the first-party source.**

Private keys are not sent to `screenerbot.io` or to any other host. The only on-chain value transfer this program adds on top of a swap is a disclosed 0.5% platform fee, and the accounts that receive it resolve to one project wallet (`D6g8i5HkpesqiYF6YVCL93QD3py5gYwYU9ZrcRfBSayN`). There is no second skim, no `approve` / `setAuthority` to a foreign address, and no obfuscated payload.

**Confidence: high** for that conclusion, for the surface that was actually read (outbound call sites, key lifecycle, swap instruction builders, cost guard, update downloader, dashboard auth, MCP tool registry, install and build scripts). It is not a proof that every transitive crate and every dashboard template is free of bugs. Confidence is **not** a statement about the `screenerbot.io` server, which is not in this repository.

The issues below are design and dependency findings. They are real, and they are not evidence of a planted backdoor.

### What was not checked

- Source of every transitive crate in `Cargo.lock` and every package in the npm lockfiles. `cargo audit` and `npm audit` were run; the crates and packages themselves were not read.
- `cargo-deny` was not run. It was not installed, and a second from-source install was not done after `cargo-audit` 0.22.2 completed.
- The `screenerbot.io` service that receives account logins, data-service queries, and (only if the user turns it on) signed transactions.
- Release binaries, installers, and the `curl | bash` installer as served from the website. Only the source in this tree was reviewed.
- A live mainnet swap. No transaction was built against a funded wallet or submitted.
- An exhaustive DOM-XSS pass over every dashboard template. Token names and other chain data are rendered in many pages; that was not line-audited.
- Whether each of the eight Jito tip addresses is still on Jito's current tip-account list. They are labeled as Jito tips in source and are not a platform fee.

## Findings

| ID | Severity | Topic | Malware? |
| --- | --- | --- | --- |
| H1 | High | New MCP pairings default to full access, including unattended trades and config writes. No key-export tool. | No. Opt-in pairing, intentional default. |
| M1 | Medium | The data-service base URL is editable and receives the account access token. | No. Footgun. Refresh token is not sent. |
| M2 | Medium | If Jupiter/Raptor simulation is down, sells skip the inner-instruction and balance checks. | No. Trust in the aggregator when simulation fails. |
| M3 | Medium | `GET /api/config` returns bot tokens, LLM keys, RPC URLs, and the lockscreen hash to anyone who can call the local API. | No. |
| L1 | Low | Wallet encryption is machine-bound AES-256-GCM, not a passphrase. Same-user code can decrypt. | No. Documented. |
| L2 | Low | Dashboard and lockscreen passwords are a single fast BLAKE3 hash. | No. |
| L3 | Low | `POST /api/wallets/{id}/export` returns the base58 key with no step-up beyond dashboard auth. | No. |
| L4 | Low | Promo capture mode can `eval` a string from the local promo driver. | No. Not loaded on a normal launch. |
| L5 | Low | Electron disables the Chromium sandbox. `nodeIntegration` stays off. | No. |
| L6 | Low | `cargo audit` and `npm audit` report known vulnerabilities in dependencies. | No. No typosquat or malicious package name found in direct dependencies. |
| I1 | Info | 0.5% platform fee to the wallet below. Disclosed in the trade UI. | No. |
| I2 | Info | Account password and hostname are sent to `screenerbot.io` on sign-in. | No. Intentional login. |
| I3 | Info | Referral activation is opt-in and sends public addresses plus a non-transaction signature. | No. |
| I4 | Info | Signed transactions are relayed to `screenerbot.io` only when gateway mode is on. Default is off. | No. |

### H1 — Paired agents can trade and rewrite config without a per-trade prompt

Creating a pairing is an explicit owner action, and the secret is returned once. The policy stored for a new pairing is full access unless the caller passes a narrower one:

```118:128:src/agent_control/pairing.rs
/// `permissions` is the connection's own policy. `None` means the default a new
/// connection gets: full access, which the owner then limits per connection in
/// Settings → Agent Connections.
pub fn create(
    label: &str,
    agent_kind: &str,
    permissions: Option<ToolPermissions>,
) -> Result<NewPairing> {
    let label = validate_label(label)?;
    let agent_kind = validate_kind(agent_kind)?;
    let permissions = permissions.unwrap_or_else(ToolPermissions::full_access);
```

For MCP and for scheduled automation, `Allow` means execute with no human prompt. Trading tools are in that set:

```98:104:src/agent_control/mod.rs
        InvocationSource::ScheduledFull | InvocationSource::Mcp { .. } => {
            if permission == PermissionLevel::Allow {
                Decision::Execute
            } else {
                Decision::RequireApproval
            }
        }
```

The in-app assistant is stricter: a tool with `requires_confirmation` (the trading tools) still waits for a person. The shipped `agent_control` policy is also `allow` on every category, including `trading` and `config` (`src/config/schemas/agent_control.rs:23-53`). A test locks that in as "unrestricted" (`src/agent_control/permissions.rs:411-413`).

What a paired client can do:

- Submit buys, adds, sells, and closes (`buy_token`, `add_to_position`, `sell_token`, `close_position`).
- Read and write configuration, including RPC URLs and the data-service endpoint (see M1). Wallet key fields are the exception.

What it cannot do:

- Export or overwrite the trading key. The only blocked config paths are `wallet_encrypted` and `wallet_nonce` (`src/agent_control/config_access.rs:25-27`). There is no export tool in the registry.

MCP itself is stdio-only and talks to the loopback agent bridge with a pairing secret (`src/mcp/mod.rs`). The bridge is exempt from the GUI session token, so on a headless bind of `0.0.0.0` the pairing secret is the control that matters (`src/webserver/routes/agent_bridge/mod.rs`). `agent_control.enabled` defaults to `true`.

This is a dangerous default for a process that holds a hot wallet. It is also what the code says it ships. Narrow new pairings to `ask_user` or `deny` for `trading` and `config` if an agent is connected.

### M1 — Configurable data-service URL receives the account access token

The account server and the transaction relay are constants, on purpose, because a config-editable URL would let a `config.toml` edit redirect credentials or signed transactions (`src/account/client.rs:7-18`, `src/rpc/gateway.rs:59-64`).

The data-service base URL does not follow that rule. It defaults to `https://screenerbot.io/data` and is a normal config field (`src/config/schemas/tokens.rs:215-220`, and the OHLCV copy at `src/config/schemas/ohlcv.rs:226`). Every request to it attaches the current account access token:

```416:421:src/data_server/mod.rs
    let url = format!("{endpoint}{path}");
    let client = crate::net::client();
    fetch_json(&AccountCredential, path, chain, |token| {
        let request = service_request(&client, method.clone(), &url, chain, query)
            .bearer_auth(token)
            .timeout(timeout);
```

Bodies on those calls are chain queries (mints, pool ids, public addresses, signature lists). Private keys were not found in them. The refresh token is stored separately and is not attached here; the access token is described as memory-only with a 15-minute lifetime (`src/account/store.rs:20-21`).

An owner, or a paired agent with config permission (H1), can point this URL at another host. The next signed-in data request will present the bearer token to that host. The token can then be replayed against the real `screenerbot.io` until it expires. It cannot, by itself, sign a transaction.

The referral URL is also editable (`src/config/schemas/referral.rs:45-50`). That request does not carry the bearer token. See I3.

### M2 — Aggregator sells proceed when simulation is unavailable

`cost_guard` runs on Jupiter and Raptor transactions before submit. At the top level it allows only compute-budget instructions, ATA creates for the wallet, system transfers to the wallet's wrapped-SOL account, the platform fee wallet, or a labeled Jito tip account, and token `CloseAccount` back to the wallet. Any other system transfer is `TransferToStranger`. Top-level `approve` and `setAuthority` are rejected (`src/chains/solana/swaps/cost_guard/message.rs:264-287` and the tests in `cost_guard/tests.rs`).

If simulation did not run and the input mint is not native SOL, those top-level checks have already passed and the function then returns success without the inner-instruction grant check or the balance check:

```339:349:src/chains/solana/swaps/cost_guard.rs
    let outcome = match simulated {
        Simulated::Ran(outcome) => outcome,
        Simulated::Unavailable { detail } => {
            if crate::chains::adapter().is_native_asset(&quote.input_mint) {
                return Err(Refusal::Unavailable(detail.to_owned()));
            }
            logger::warning(
                LogTag::Swap,
                &format!("{router} preflight simulation unavailable, proceeding: {detail}"),
            );
            return Ok(());
        }
    };
```

A buy (native SOL in) is refused when simulation is down. A sell can still be submitted. A compromised or malicious Jupiter/Raptor response could, in that window, include a router instruction whose inner calls move more than the quote. This repository does not add that instruction itself.

Direct pool swaps are built locally and do not go through `cost_guard`. Their only extra transfers are the platform fee and a wrapped-SOL close back to the owner (see I1).

### M3 — Authenticated config API returns third-party secrets

`GET /api/config` blanks the webserver password hash, salt, and TOTP secret (`src/webserver/routes/config/getters.rs:76-84`). It returns the rest of the config cloned, including `telegram`, `llm`, `gui`, and `chains` (`getters.rs:31-57`).

Those sections hold the Telegram bot token, LLM provider API keys, RPC URLs (often with a provider key in the query string), and the lockscreen password hash and salt. Export sanitizes the same fields only when `sanitize_secrets` is true (`src/webserver/routes/config/types.rs:107-144`). The live GET does not.

`wallet_encrypted` / `wallet_nonce` are not fields of this response. The lockscreen is a UI overlay; it is not middleware on `/api/*`.

On the default headless bind (`127.0.0.1:8080`, `auth_enabled = false`) any local process that can connect and send a loopback `Host` can read this. That is the same trust boundary as the process itself, but it is wider than "the dashboard window" and it includes secrets the UI treats as hidden. Remote bind without a password is refused (below).

### L1 — Keys are encrypted to the machine, not to a password

`encrypt_private_key` / `decrypt_private_key` use AES-256-GCM. The 32-byte key is `BLAKE3(machine_id || b"screenerbot-wallet-encryption-v1")` with no salt iteration and no user passphrase (`src/secure_storage/encryption.rs:15-16`, `53-65`, `67-91`). The nonce is 12 random bytes. The same function encrypts the account refresh token.

The project states the limit directly: this stops a copied file from being useful on another machine, and it does not stop code running as this user (`src/account/store.rs:7-18`). The account file is created mode `0600` on Unix (`store.rs:106-111`). A copy of the data directory together with the machine id (on Linux, typically `/etc/machine-id`) decrypts the trading key.

Import accepts a base58 Solana keypair or a JSON byte array (`src/chains/solana/accounts/keypair.rs`). There is no BIP39 or seed-phrase path. `Debug` on the request types that carry a key prints `[redacted]` (`src/webserver/routes/initialization/types.rs:46-50`, `src/wallets/types.rs`). Export logs the wallet name and address, not the key (`src/wallets/manager/export.rs:32-35`). No production log format string that interpolates a private key was found.

### L2 — Password hash is fast

`hash_password` is one BLAKE3 keyed hash over the password (`src/secure_storage/password.rs:16-41`). The comment says the speed is for PIN UX. It is not a memory-hard password KDF. The same function covers the lockscreen and the headless dashboard password. A weak password in a leaked `config.toml` is cheap to guess. Comparison is constant-time (`password.rs:44-65`).

### L3 — Key export is one authenticated POST

`POST /api/wallets/{id}/export` (`src/webserver/routes/wallets/mod.rs:40`) decrypts and returns the base58 private key in JSON (`src/wallets/manager/export.rs:27-40`). The handler does not ask for the dashboard password again, a TOTP code, or the lockscreen PIN. The dashboard shows a warning and a reveal button; that check is in the page, not on the server.

Anyone who can already call the API can export. On a default loopback install with auth off, that is any local caller. Same-user code can also decrypt the stored blob without this route (L1), so the route does not create a new trust boundary. It does make the key available to XSS in the dashboard origin if one exists. This review did not complete a DOM-XSS audit.

`POST /api/initialization/wallet/generate` returns a new key once, before it is stored (`src/webserver/routes/initialization/handlers.rs:418-430`). Setup posts the imported key to the local server only; it is encrypted before it is written.

### L4 — `eval` exists only in promo capture

`src/webserver/templates/scripts/promo/runtime.js:728-731` compiles `expression` with `AsyncFunction`. The comment and `electron/src/promo_bridge.js` say the bridge is loaded only when `SCREENERBOT_PROMO_CONTROL` is set. A normal launch does not start it. The string is not fetched from the network by this code.

`electron/src/l10n.js:83-89` uses `new Function` to load the vendored Fluent bundle from disk, not from the network.

### L5 — Electron sandbox

The dashboard window sets `contextIsolation: true` and `nodeIntegration: false`, and `sandbox: false` (`electron/src/main.js:1108-1113`). The preload script exposes a small `contextBridge` API. The splash HTML has a CSP of `script-src 'self'` (`electron/src/index.html:9`). The Rust dashboard responses were not found to set a Content-Security-Policy header. No `openDevTools` or remote-debugging flag was found in `electron/`.

The Windows VC++ redistributable is not downloaded and executed silently. If the bundled file is missing, the shell opens Microsoft's `https://aka.ms/vs/17/release/vc_redist.*.exe` in the browser (`electron/src/main.js:1028-1038`).

### L6 — Dependency audits

Commands run on 2026-11-09 against the lockfiles in this tree. No install of the application dependencies was required for npm (`npm audit --package-lock-only`).

**`cargo audit` 0.22.2** (RustSec advisory database): exit 1, **28 vulnerability entries**, plus unmaintained warnings, unsound warnings, and **one yanked crate** (`yoke-derive` 0.8.3). The vulnerability entries are in transitive crates, not in a direct dependency added under a lookalike name. Several advisories are reported twice because two versions are in the lockfile. Representative IDs:

| Advisory | Crate | Note |
| --- | --- | --- |
| RUSTSEC-2022-0093 | `ed25519-dalek` 1.0.1 | Historical signature-verification oracle. Transitive (Solana stack). Not a key-exfiltration bug, and this repo does not expose a verify-oracle API. |
| RUSTSEC-2024-0344 | `curve25519-dalek` 3.2.0 | Timing variability in scalar subtraction. Transitive. |
| RUSTSEC-2026-0346 through 0349 | `rustls` 0.21.12 and 0.23.45 | Session `Debug` leaking TLS secrets locally; ServerHello downgrade; ECH and certificate-decompression issues. The app pins `rustls` 0.23. |
| RUSTSEC-2026-0382, 0098, 0099, 0104 | `rustls-webpki` | Certificate-validation bugs. |
| RUSTSEC-2026-0381, 0383 | `webpki-roots`, `webpki-root-certs` | Distrusted emSign intermediates still in the bundled roots. |
| RUSTSEC-2026-0194, 0195 | `quick-xml` 0.26.0 | Parser denial of service. |
| RUSTSEC-2026-0387, 0385 | `crypto-bigint`, `ctutils` | Constant-time gaps. |
| RUSTSEC-2026-0391, 0398, 0403, 0410 | `zstd`, `zstd-safe`, `zstd-sys` | Decompression memory-safety bugs. The app's own `flate2` use is the Rust backend, not zstd; zstd is transitive. |

**Root `npm audit`:** 6 issues (5 moderate, 1 high). All are in the lint/dev tree: `fast-uri` (GHSA-w74x-v6f9-j3xg and related), `minimatch` (GHSA-g3hv-75wg-m249), `source-map-js` (GHSA-68fv-2mgg-jv7q). They do not ship inside the trading binary.

**`electron/` `npm audit`:** 28 issues (4 moderate, 24 high) in the Electron Forge packaging tree and its HTTP stack: `extract-zip` symlink traversal (GHSA-jmr9-qjv8-65gv, GHSA-7pqw-9j4j-h8q3), `undici` 7.x (including GHSA-2m5v-pch6-74hx, a TLS option-drop bug, and several denial-of-service issues), `image-size`, plus the same `fast-uri` and `minimatch` issues. These are devDependencies of the desktop packager. The dashboard the user trades from is the Rust server, not this Node process.

Direct dependencies were read from `Cargo.toml`, `package.json`, and `electron/package.json`. Names are the well-known crates and packages (`tokio`, `axum`, `reqwest`, `solana-sdk`, `aes-gcm`, `eslint`, `electron`, `@electron-forge/*`). No typosquat stood out. That is not a full typosquat review of the transitive closure.

`package.json` has no `preinstall` or `postinstall`. `electron/package.json` `prestart` only copies the version out of `Cargo.toml`. The Electron lockfile marks install scripts on optional packaging natives (`@bitdisaster/exe-icon-extractor`, `fs-xattr`, `macos-alias`), which is normal for those packages.

### I1 — Platform fee: where it goes, and that it is disclosed

Rate is 50 basis points, not configurable, shared by every router (`src/chains/solana/swaps/revenue.rs:27-43`):

| Constant | Address | Role |
| --- | --- | --- |
| `FEE_WALLET` | `D6g8i5HkpesqiYF6YVCL93QD3py5gYwYU9ZrcRfBSayN` | Native SOL fees from the direct engine and from Raptor. |
| `FEE_TOKEN_ACCOUNT_USDC` | `GqnJw7x5Ss6jR5XU65v3siWmWaMae5VanxaCv7XU742v` | USDC fees. A unit test checks this is the fee wallet's associated USDC account (`revenue.rs:160-174`). |
| `FEE_TOKEN_ACCOUNT_WSOL` | `9yiZThTzanryu3mg1VVu6Qy4HiqKhydCAUqcasLHPxWB` | WSOL fees for Jupiter, which can only be paid to a token account. |

The trade panel discloses this as "Platform fee" / "0.5% — supports development. Already built into the quote above." (`locales/en/trade.ftl:22-23`). The same string exists in the other locale catalogs.

On 2026-11-09 the public mainnet RPC (`getMultipleAccounts`, `jsonParsed`) showed:

- `FEE_TOKEN_ACCOUNT_USDC` is an SPL token account for USDC whose owner is `FEE_WALLET`.
- `FEE_TOKEN_ACCOUNT_WSOL` is a native-mint SPL account. Its token-account owner is `FnTdf2xmCUXrW7PMRtsSiQsnCzGZ31HTY2Sb36vfWtVn`, which is an account of Jupiter's referral program (`REFER4ZgmyYx9c6He5XfaTMiGfdLwRnkV4RPp9t9iF3`), not a second user wallet hardcoded in this repo.
- That referral account's data, after an 8-byte discriminator, starts with `FEE_WALLET`, then Jupiter's referral project key `DkiqsTrw1u1bYFumumC7sCG2S8K25qc2vemJFHyW2wJc` (the project id in Jupiter's own referral docs), and the name `ScreenerBot`.

So Jupiter WSOL fees accrue in the project's referral token account, and the partner key that can claim them is the same fee wallet. `DkiqsTrw…` is not a destination this repository hardcodes; it is a field inside the on-chain referral account. Jupiter's own program may keep a share of integrator fees under Jupiter's rules. That share is not an extra transfer written here. Raptor is asked for `feeBps: 50` and `feeAccount: FEE_WALLET` (`src/chains/solana/swaps/routers/raptor.rs:172-182`). Jupiter's quote sets `platform_fee_bps` to the same 50 (`src/chains/solana/swaps/routers/jupiter.rs` quote path) and the swap build sets `fee_account` from `fee_reference_for_pair` (`jupiter.rs:606-617`).

Direct-swap transfers (`src/chains/solana/swaps/direct/fee.rs:165-197`):

- SOL: `system_instruction::transfer` from the user wallet to `FEE_WALLET`.
- USDC: SPL `transfer_checked` to `FEE_TOKEN_ACCOUNT_USDC`.

The other production `system_instruction::transfer` is the wrap into the user's own WSOL account (`src/chains/solana/swaps/direct/plan.rs:105-108`). The production `close_account` sends the reclaimed lamports to `intent.owner`, the user (`plan.rs:161-169`). No production `approve` or `set_authority` call site was found outside tests.

`src/chains/evm` is chain metadata and well-known Base addresses (WETH, USDC, Permit2, Universal Router). No EVM transaction is built or signed there.

Eight addresses in `src/chains/solana/transactions/program_ids.rs:190-199` are labeled Jito tip accounts and are the only other system-transfer destinations the cost guard accepts. They are priority-fee tips, not the platform fee.

### I2 — Account login sends the password and a hostname label

`API_BASE` is `https://screenerbot.io` (`src/account/client.rs:27`). The password grant posts `email`, `password`, `device_label`, `platform`, `app_version`, and `scope` to `POST /api/app/token` (`client.rs:351-362`). The password is not written to config and not logged by this function. `device_label` is `ScreenerBot on <hostname>` (`client.rs:152-177`). The same device label, OS, and version go out on wallet sign-in and the device-code flow. Wallet sign-in sends the public address and a signature over the server's challenge, not the private key (`client.rs:396-413`).

Tokens are stored encrypted (L1). Access tokens stay in memory.

### I3 — Referral is opt-in

With an empty code, the service does not start (`src/config/schemas/referral.rs:64-69`, `src/services/implementations/referral_service.rs:9-19`). With a code, one JSON body is posted to `referral.endpoint` (default `https://screenerbot.io/api/referral/activate`): `referral_code`, `proofs[]` of `{wallet, issued_at, signature}`, `platform`, `version` (`referral_service.rs:52-74`). The signed text is a domain-separated statement, not a Solana transaction. Balances, config, and machine id are not in that body. The endpoint is config-editable, so a local edit can send those proofs elsewhere. The proofs do not spend funds.

### I4 — Transaction relay is off by default

`use_gateway_rpc` defaults to `false` (`src/config/schemas/account.rs:44-57`). When it is on, or when chain access is in service mode, `sendTransaction` and a few related methods are posted as JSON-RPC to the constant `https://screenerbot.io/data/v1/rpc` with the bearer token (`src/rpc/gateway.rs:51-64`, `100-111`). The `sendTransaction` params are the already-signed transaction, base64, not the key. The comment in `account.rs` says this is off by default because it hands every signed transaction to that server. `auto_wallet_signin` also defaults to `false`, so the trading key is not used to sign a login message unless the user opts in (`account.rs:29-42`).

## Dashboard and API authentication

| Mode | Bind | Auth |
| --- | --- | --- |
| GUI / Electron | `127.0.0.1` and a port in 49152–65535 (`src/webserver/server.rs:32-37`, `165-187`) | `X-ScreenerBot-Token`, 64 chars, printed to stdout for the shell. Not embedded in HTML. |
| Headless | Config, default `127.0.0.1:8080` (`src/config/schemas/webserver.rs:29-41`) | Password session cookie `screenerbot_session` only if `auth_enabled`. Default is `false`, and there is no default password. |

`validate_headless_bind` refuses a non-loopback host when auth is off (`src/webserver/server.rs:369-378`). A public bind therefore requires a password to have been set. The cookie is `HttpOnly; SameSite=Strict` (`src/webserver/routes/auth/helpers.rs`). There is no separate CSRF token. Loopback requests with auth off must present a loopback `Host`, and `Origin` if present must match (`src/webserver/local_request.rs`, `src/webserver/middleware.rs:149-153`). That blocks a browser page on another origin, including a DNS-rebinding host name. It does not block another process on the same machine.

SSE paths ending in `/stream` are exempt from the GUI token because `EventSource` cannot set headers (`middleware.rs:161-171`). They still go through the loopback `Host`/`Origin` check in GUI mode. Static assets, `/api/health`, and `/oauth/callback` are also token-exempt.

No CORS layer is installed. `tower-http` is built with the `cors` feature, and a search of `src/` found no `CorsLayer`.

## Outbound hosts

Application-initiated HTTP and TCP. Browser "open this explorer link" targets (solscan, dexscreener, birdeye, and similar) are omitted; the page opens them locally and this process does not POST secrets to them.

### `screenerbot.io` (first-party)

| Endpoint | When | Data sent |
| --- | --- | --- |
| `POST /api/app/token` | Sign-in, refresh | Password grant: email, password, device label, OS, version, scopes. Other grants: auth code, refresh token, device code, or wallet signature. |
| `POST /api/app/revoke` | Sign-out | Refresh token. |
| `POST /api/app/wallet/nonce` | Wallet sign-in | Public wallet address. |
| `POST /api/app/device` | Headless device flow | Client id, scopes, device label, OS, version. |
| `GET /api/account/session` | Profile | Bearer access token only. |
| `GET/POST https://screenerbot.io/data/v1/…` | Signed in, source enabled | Bearer token, `x-screenerbot-version`, `chain`, mints, pool ids, public addresses, signature lists. Default base URL is config. |
| `POST https://screenerbot.io/data/v1/rpc` | Gateway on, or service chain access | Bearer token, version, JSON-RPC (`sendTransaction`, blockhash, signature status, simulate, fee for message). Signed transactions only for `sendTransaction`. URL is not config. |
| `POST /api/referral/activate` | Referral code set | Code, wallet proofs, platform, version. |
| `GET /api/releases/check`, `/releases/history`, `/api/releases/download` | Update check | Version and platform. `User-Agent: ScreenerBot/<version>`. |
| `GET /releases/<version>/…` | Update download mirror | Artifact bytes. |
| `GET /api/boost`, `GET /api/token-profile/public` | Featured / profile feeds | No auth, no body. |

Update downloads must be `https` on `screenerbot.io`, `github.com`, or `*.githubusercontent.com`, with no userinfo, including after redirects (`src/version/download.rs:737-745`). Integrity is SHA-256 against the manifest, not a signature from a release key. Applying an update restarts into the downloaded build; opening a full installer is a separate API action (`src/version/installer.rs`), not an automatic exec on the check response.

`install.sh` downloads the release from `https://screenerbot.io` and checksums from `https://github.com/farfary/ScreenerBot/releases/download`, then runs `screenerbot --version` as a probe. The documented `curl … install.sh | bash` is the operator's own invocation of that script, not something the bot does later.

### Other hosts the process contacts

| Host | Role | Sensitive data |
| --- | --- | --- |
| User-configured Solana RPC and its `wss://` counterpart | Chain reads and, when the gateway is off, transaction submit | Signed transactions, public addresses. The URL is whatever the user (or a paired agent) set. |
| `https://api.jup.ag` or `https://lite-api.jup.ag` | Quotes and swap builds. API key header only if configured. | Swap request: public key, quote, fee account. Not the private key. |
| `https://raptor-beta.solanatracker.io` | Optional router | Quote parameters, public wallet, fee wallet. |
| `https://data.solanatracker.io` | Optional OHLCV | Mint / pool queries, API key if set. |
| `https://api.dexscreener.com` | Market data | Mint in the path. |
| `https://api.geckoterminal.com` | Market data and OHLCV | Pool / token ids. |
| `https://api.rugcheck.xyz` | Token risk, direct fallback | Mint. |
| `https://lite-api.jup.ag` | Token list / price | Mint ids. |
| `https://api.coingecko.com`, `https://api.llama.fi` | Prices | Public market queries. |
| `https://api.telegram.org` | Bot, if enabled | Bot token is in the URL. Message text. |
| `api.openai.com`, `api.anthropic.com`, `api.groq.com`, `api.deepseek.com`, `generativelanguage.googleapis.com`, `api.together.xyz`, `openrouter.ai`, `api.mistral.ai` | LLM, if a provider key is set | Prompts and API keys. |
| `https://www.google.com`, `https://solana.com` | Connectivity HTTP probes | No body of user data. |
| `8.8.8.8:53`, `1.1.1.1:53` | Connectivity TCP probes | No payload. `src/config/schemas/connectivity.rs:50-58`. |
| `https://ipfs.io`, `https://arweave.net`, token-metadata URLs | Logo / metadata fetch when a mint URI says so | The URI itself. |
| `https://github.com`, `https://objects.githubusercontent.com` | Update artifacts and `SHA256SUMS` | Versioned file download. |
| `https://fonts.googleapis.com`, `https://fonts.gstatic.com` | Electron splash fonts | None. |
| `https://aka.ms` | Browser open for the VC++ redist, only if the bundle is missing | None. |

No hardcoded public IP is used as an application server. No `curl` or `wget` runs from the Rust process. `build.rs` only stamps an asset version and generates locale data. `.github/workflows/ci.yml:61` downloads a pinned `cargo-nextest` tarball from `get.nexte.st` inside CI. `build.yml` is the release build. Neither workflow posts secrets to a third-party collector.

Logging redacts API keys in URLs for known provider shapes (`src/logger/redact.rs`). That is a log filter, not a network control.

## Obfuscation

Searched `src/`, `electron/`, `tools/`, and `scripts/` for `eval(`, `new Function`, `AsyncFunction`, `atob`, `fromCharCode`, long opaque base64 blobs, and `include_bytes!` of a binary. The only dynamic code is L4 and the Fluent loader. `include_bytes!` in `src/webserver/embeds.rs` embeds dashboard assets (HTML, CSS, JS, images, fonts), not an executable. No `.exe`, `.dll`, `.so`, `.wasm`, or other binary blob is in the tree. No download-and-exec of an unexpected binary was found in the app.

## Bottom line

Ship and run this tree as a normal self-hosted trading bot that charges a disclosed 0.5% fee to a single project wallet, talks to `screenerbot.io` for an optional account and data service, and keeps the trading key on the machine. Do not treat the machine-bound encryption as protection against other programs running as the same user. Do not pair an MCP client until its trading and config permissions have been narrowed. Do not point `tokens.sources.screenerbot_server.endpoint` at a host you do not trust while signed in.
