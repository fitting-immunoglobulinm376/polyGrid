//! Polymarket Perps REST adapter (Polygon EIP-712 + proxy session).
//!
//! Public market data uses GET `/v1/info/*`. Trading uses proxy-signed ops;
//! private reads use `polymarket-proxy` + `polymarket-secret` headers.
//! WebSocket is not required for v1 (REST polling, same pattern as the old HL adapter).

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use grid_engine::{FillEvent, LiveOrder, OrderIntent, RunMode, Side};
use k256::ecdsa::{RecoveryId, Signature, SigningKey};
use rand::RngCore;
use rust_decimal::{Decimal, RoundingStrategy};
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha3::{Digest, Keccak256};
use tracing::{info, warn};

use crate::traits::{Balance, Exchange, ExchangeError, ExchangeResult, MarketInfo};

const BASE_URL: &str = "https://api.perpetuals.polymarket.com";
const CHAIN_ID: u64 = 137;
/// Default proxy credential lifetime (7 days).
const PROXY_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// Renew when fewer than this many ms remain.
const PROXY_RENEW_SLACK_MS: u64 = 60 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedProxyCreds {
    proxy_sk: String,
    proxy_addr: String,
    secret: String,
    expires_at_ms: u64,
}

#[derive(Clone)]
struct InstrumentMeta {
    instrument_id: u64,
    /// UX symbol without quote suffix (e.g. BTC).
    base: String,
    /// Full exchange symbol (e.g. BTC-USD).
    full_symbol: String,
    quantity_decimals: u32,
    price_decimals: u32,
    min_notional: Decimal,
    max_leverage: u32,
    isolated_only: bool,
}

#[derive(Clone)]
pub struct PolymarketExchange {
    mode: RunMode,
    base_url: String,
    client: reqwest::Client,
    /// EOA private key hex (no 0x).
    private_key: Option<String>,
    /// EOA address 0x…
    address: Option<String>,
    /// Ephemeral proxy signing key hex.
    proxy_sk: Option<String>,
    proxy_addr: Option<String>,
    secret: Option<String>,
    proxy_expiry_ms: u64,
    instruments: HashMap<String, InstrumentMeta>,
    /// instrument_id → UX base symbol
    id_to_base: HashMap<u64, String>,
    open_orders: HashMap<String, LiveOrder>,
    last_seen_fills: Vec<String>,
    pending_immediate_fills: Vec<FillEvent>,
    fills_primed: bool,
    session_start_ms: u64,
}

impl PolymarketExchange {
    pub fn new(mode: RunMode) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            mode,
            base_url: BASE_URL.to_string(),
            client,
            private_key: None,
            address: None,
            proxy_sk: None,
            proxy_addr: None,
            secret: None,
            proxy_expiry_ms: 0,
            instruments: HashMap::new(),
            id_to_base: HashMap::new(),
            open_orders: HashMap::new(),
            last_seen_fills: Vec::new(),
            pending_immediate_fills: Vec::new(),
            fills_primed: false,
            session_start_ms: 0,
        }
    }

    pub fn set_private_key(&mut self, key: &str) -> ExchangeResult<()> {
        let key = key.trim().trim_start_matches("0x");
        let bytes = hex::decode(key).map_err(|e| ExchangeError::InvalidKey(e.to_string()))?;
        if bytes.len() != 32 {
            return Err(ExchangeError::InvalidKey(
                "private key must be 32 bytes hex".into(),
            ));
        }
        let signing_key = SigningKey::from_bytes((&bytes[..]).into())
            .map_err(|e| ExchangeError::InvalidKey(e.to_string()))?;
        let addr = address_from_verifying_key(&signing_key);
        self.private_key = Some(key.to_string());
        self.address = Some(addr);
        // Force new proxy session for the new EOA.
        self.clear_proxy_state();
        Ok(())
    }

    pub fn address(&self) -> Option<&str> {
        self.address.as_deref()
    }

    fn clear_proxy_state(&mut self) {
        self.proxy_sk = None;
        self.proxy_addr = None;
        self.secret = None;
        self.proxy_expiry_ms = 0;
    }

    fn clear_persisted_credentials(&mut self) {
        if let Some(path) = self.credentials_path() {
            if path.exists() {
                match fs::remove_file(&path) {
                    Ok(()) => info!("removed stale proxy credentials {}", path.display()),
                    Err(e) => warn!("failed to remove {}: {e}", path.display()),
                }
            }
        }
        self.clear_proxy_state();
    }

    /// Drop in-memory (+ optional disk) proxy so the next ensure_session recreates it.
    pub fn invalidate_proxy_session(&mut self, remove_persisted: bool) {
        if remove_persisted {
            self.clear_persisted_credentials();
        } else {
            self.clear_proxy_state();
        }
    }

    /// Re-attach bot orders after the exchange client was accidentally recreated.
    pub fn restore_tracked_orders(&mut self, orders: &[LiveOrder]) {
        for order in orders {
            if order.exchange_id.is_none() {
                continue;
            }
            self.open_orders
                .entry(order.client_id.clone())
                .or_insert_with(|| order.clone());
        }
    }

    pub async fn account_equity_usdc(&self) -> ExchangeResult<Decimal> {
        self.account_equity_usdc_for(None).await
    }

    /// Equity / free collateral (pUSD). `symbol` is accepted for API parity; portfolio is account-wide.
    pub async fn account_equity_usdc_for(
        &self,
        _symbol: Option<&str>,
    ) -> ExchangeResult<Decimal> {
        let portfolio = self.get_private_json("/v1/account/portfolio").await?;
        let withdrawable = json_decimal(portfolio.get("withdrawable")).unwrap_or(Decimal::ZERO);
        let equity = portfolio
            .get("margin")
            .and_then(|m| json_decimal(m.get("total_account_value")))
            .unwrap_or(Decimal::ZERO);
        if withdrawable > Decimal::ZERO {
            Ok(withdrawable)
        } else {
            Ok(equity)
        }
    }

    pub async fn max_side_notional(&self, leverage: u32) -> ExchangeResult<Decimal> {
        self.max_side_notional_for(None, leverage).await
    }

    pub async fn max_side_notional_for(
        &self,
        symbol: Option<&str>,
        leverage: u32,
    ) -> ExchangeResult<Decimal> {
        let equity = self.account_equity_usdc_for(symbol).await?;
        let lev = Decimal::from(leverage.max(1));
        Ok((equity * lev * dec!(0.75)).round_dp(2))
    }

    pub async fn preflight_grid_notional(
        &self,
        intents: &[OrderIntent],
        leverage: u32,
    ) -> ExchangeResult<()> {
        let mut buy_ntl = Decimal::ZERO;
        let mut sell_ntl = Decimal::ZERO;
        for i in intents {
            let n = i.price * i.size;
            match i.side {
                Side::Buy => buy_ntl += n,
                Side::Sell => sell_ntl += n,
            }
        }
        let symbol = intents.first().map(|i| i.symbol.as_str());
        let max_side = self.max_side_notional_for(symbol, leverage).await?;
        let equity = self.account_equity_usdc_for(symbol).await?;
        let worst = buy_ntl.max(sell_ntl);
        if max_side <= Decimal::ZERO {
            return Err(ExchangeError::Other(format!(
                "账户可用保证金不足（约 {equity} pUSD），无法按 {leverage}x 挂网格。\
请先在 https://polymarket.com 充值 pUSD 后再试。"
            )));
        }
        if worst > max_side {
            let suggest_total = (max_side * dec!(2) * dec!(0.95)).round_dp(0);
            return Err(ExchangeError::Other(format!(
                "网格单边名义约 {worst} pUSD，超过当前 {leverage}x 杠杆允许的约 {max_side} pUSD \
（可用保证金约 {equity} pUSD）。请把「总名义投入」降到约 {suggest_total} 以下，\
或减少网格数量 / 提高杠杆 / 增加保证金；若该币种已有仓位也会占用保证金。"
            )));
        }
        Ok(())
    }

    pub async fn get_perp_position(
        &self,
        symbol: &str,
    ) -> ExchangeResult<(
        Decimal,
        Option<Decimal>,
        Option<Decimal>,
        Option<Decimal>,
    )> {
        let meta = self.resolve_instrument(symbol)?;
        let portfolio = self.get_private_json("/v1/account/portfolio").await?;
        let mut size = Decimal::ZERO;
        let mut entry = None;
        let mut upnl = None;
        let mut liquidation = None;
        if let Some(positions) = portfolio.get("positions").and_then(|a| a.as_array()) {
            for p in positions {
                let iid = p
                    .get("instrument_id")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let sym = p.get("symbol").and_then(|s| s.as_str()).unwrap_or("");
                if iid != meta.instrument_id
                    && !sym.eq_ignore_ascii_case(&meta.full_symbol)
                    && !sym.eq_ignore_ascii_case(&meta.base)
                    && !sym.eq_ignore_ascii_case(symbol)
                {
                    continue;
                }
                size = json_decimal(p.get("size")).unwrap_or(Decimal::ZERO);
                entry = json_decimal(p.get("entry_price")).filter(|px| *px > Decimal::ZERO);
                upnl = json_decimal(p.get("unrealized_pnl"));
                liquidation =
                    json_decimal(p.get("liquidation_price")).filter(|px| *px > Decimal::ZERO);
                break;
            }
        }
        Ok((size, entry, upnl, liquidation))
    }

    /// Net funding cash flow for this bot session. Negative means paid, positive received.
    pub async fn get_session_funding_pnl(&self, symbol: &str) -> ExchangeResult<Decimal> {
        let meta = self.resolve_instrument(symbol)?;
        let path = format!(
            "/v1/account/funding?instrument_id={}&start_timestamp={}",
            meta.instrument_id, self.session_start_ms
        );
        let raw = self.get_private_json(&path).await.unwrap_or(json!({}));
        let mut total = Decimal::ZERO;
        if let Some(arr) = raw
            .get("data")
            .and_then(|d| d.as_array())
            .or_else(|| raw.as_array())
        {
            for row in arr {
                if let Some(f) = json_decimal(row.get("funding")) {
                    total += f;
                }
            }
        }
        Ok(total)
    }

    pub fn adopt_open_orders(&mut self, symbol: &str, orders: &[LiveOrder]) {
        let key = self.normalize_symbol(symbol);
        let keyed: Vec<(String, String)> = self
            .open_orders
            .iter()
            .map(|(id, o)| (id.clone(), o.symbol.clone()))
            .collect();
        for (id, sym) in keyed {
            let ok = self.normalize_symbol(&sym);
            if ok.eq_ignore_ascii_case(&key)
                || sym.eq_ignore_ascii_case(symbol)
                || ok.eq_ignore_ascii_case(symbol)
            {
                self.open_orders.remove(&id);
            }
        }
        for o in orders {
            self.open_orders.insert(o.client_id.clone(), o.clone());
        }
    }

    pub async fn has_open_orders(&self, symbol: &str) -> ExchangeResult<bool> {
        let meta = self.resolve_instrument(symbol)?;
        let path = format!(
            "/v1/account/open-orders?instrument_id={}",
            meta.instrument_id
        );
        let open = self.get_private_json(&path).await?;
        Ok(open.as_array().is_some_and(|orders| !orders.is_empty()))
    }

    pub async fn prime_seen_fills(&mut self) -> ExchangeResult<()> {
        if self.secret.is_none() {
            self.fills_primed = true;
            self.session_start_ms = Self::now_ms();
            return Ok(());
        }
        self.session_start_ms = Self::now_ms();
        let fills = self
            .get_private_json("/v1/account/fills")
            .await
            .unwrap_or(json!({}));
        let arr = fills_array(&fills);
        for f in &arr {
            let tid = fill_tid(f);
            if !self.last_seen_fills.contains(&tid) {
                self.last_seen_fills.push(tid);
            }
        }
        if self.last_seen_fills.len() > 500 {
            let excess = self.last_seen_fills.len() - 400;
            self.last_seen_fills.drain(0..excess);
        }
        info!(
            "primed {} historical fill id(s); ignoring them as new",
            arr.len()
        );
        self.fills_primed = true;
        Ok(())
    }

    pub fn has_meta(&self) -> bool {
        !self.instruments.is_empty()
    }

    /// Connect only when meta has never been loaded (safe for UI balance polls).
    pub async fn ensure_connected(&mut self) -> ExchangeResult<()> {
        if !self.has_meta() {
            self.refresh_instruments().await?;
        }
        if self.private_key.is_some() {
            self.ensure_session().await?;
        }
        Ok(())
    }

    pub async fn set_leverage(
        &mut self,
        symbol: &str,
        leverage: u32,
        is_cross: bool,
    ) -> ExchangeResult<()> {
        self.ensure_session().await?;
        let meta = self.resolve_instrument(symbol)?;
        if meta.isolated_only && is_cross {
            return Err(ExchangeError::Other(format!(
                "{symbol} 仅支持逐仓（isolated），请关闭全仓后重试。"
            )));
        }
        let lev = leverage.max(1).min(meta.max_leverage);
        let compact = rmpv::Value::Array(vec![
            rmpv::Value::String("updateLeverage".into()),
            rmpv::Value::Array(vec![
                rmpv::Value::Integer(meta.instrument_id.into()),
                rmpv::Value::Integer(u64::from(lev).into()),
                rmpv::Value::Boolean(is_cross),
            ]),
        ]);
        let op_json = json!({
            "type": "updateLeverage",
            "args": {
                "iid": meta.instrument_id,
                "lev": lev,
                "cross": is_cross
            }
        });
        let resp = self
            .signed_trade("PATCH", "/v1/trade/leverage", compact, op_json)
            .await?;
        if resp.get("status").and_then(|s| s.as_str()) == Some("err") {
            return Err(ExchangeError::Api(friendly_pm_error(&resp)));
        }
        Ok(())
    }

    async fn refresh_instruments(&mut self) -> ExchangeResult<()> {
        let url = format!("{}/v1/info/instruments", self.base_url);
        let raw = self.get_public(&url).await?;
        let arr = raw
            .as_array()
            .ok_or_else(|| ExchangeError::Api("instruments not array".into()))?;
        self.instruments.clear();
        self.id_to_base.clear();
        for item in arr {
            let iid = item
                .get("instrument_id")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if iid == 0 {
                continue;
            }
            let full = item
                .get("symbol")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            if full.is_empty() {
                continue;
            }
            let base = item
                .get("base_asset")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| base_from_full(&full));
            let meta = InstrumentMeta {
                instrument_id: iid,
                base: base.clone(),
                full_symbol: full.clone(),
                quantity_decimals: item
                    .get("quantity_decimals")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(4) as u32,
                price_decimals: item
                    .get("price_decimals")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(2) as u32,
                min_notional: json_decimal(item.get("min_notional")).unwrap_or(Decimal::ONE),
                max_leverage: item
                    .get("max_leverage")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(20) as u32,
                isolated_only: item
                    .get("isolated_only")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            };
            self.id_to_base.insert(iid, base.clone());
            self.instruments.insert(base.to_ascii_uppercase(), meta.clone());
            self.instruments
                .insert(full.to_ascii_uppercase(), meta);
        }
        Ok(())
    }

    async fn ensure_session(&mut self) -> ExchangeResult<()> {
        let now = Self::now_ms();
        if self.secret.is_some()
            && self.proxy_sk.is_some()
            && self.proxy_expiry_ms > now + PROXY_RENEW_SLACK_MS
        {
            return Ok(());
        }
        if self.try_load_persisted_credentials() {
            if self.proxy_expiry_ms > now + PROXY_RENEW_SLACK_MS {
                if self.probe_private_session().await {
                    info!(
                        "restored Polymarket proxy session from disk (exp={})",
                        self.proxy_expiry_ms
                    );
                    return Ok(());
                }
                warn!(
                    "persisted proxy session rejected by API for {}; recreating",
                    self.address.as_deref().unwrap_or("?")
                );
                self.clear_persisted_credentials();
            } else {
                // Expired on disk — drop before recreate.
                self.clear_persisted_credentials();
            }
        }
        match self.create_proxy_session().await {
            Ok(()) => Ok(()),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("proxy_limit_reached") {
                    // Free our deterministic proxy slot (if any), then retry once.
                    if let Ok(det) = self.deterministic_proxy_keypair() {
                        warn!(
                            "proxy_limit_reached; trying deleteProxy for deterministic {}",
                            det.1
                        );
                        let _ = self.delete_proxy_eoa(&det.1).await;
                    }
                    self.create_proxy_session().await
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Cheap auth check used after restoring disk credentials.
    async fn probe_private_session(&self) -> bool {
        match self.get_private_json("/v1/account/portfolio").await {
            Ok(_) => true,
            Err(e) => {
                warn!("proxy session probe failed: {e}");
                false
            }
        }
    }

    fn credentials_path(&self) -> Option<PathBuf> {
        let addr = self.address.as_ref()?;
        let root = if let Ok(home) = std::env::var("POLYGRID_HOME") {
            PathBuf::from(home.trim()).join("data")
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("data")
        };
        Some(root.join(format!(
            "pm_proxy_{}.json",
            addr.trim_start_matches("0x").to_ascii_lowercase()
        )))
    }

    fn try_load_persisted_credentials(&mut self) -> bool {
        let Some(path) = self.credentials_path() else {
            return false;
        };
        let Ok(text) = fs::read_to_string(&path) else {
            return false;
        };
        let Ok(v) = serde_json::from_str::<PersistedProxyCreds>(&text) else {
            return false;
        };
        if v.proxy_sk.trim().is_empty() || v.secret.trim().is_empty() {
            return false;
        }
        self.proxy_sk = Some(v.proxy_sk.trim_start_matches("0x").to_string());
        self.proxy_addr = Some(v.proxy_addr);
        self.secret = Some(v.secret);
        self.proxy_expiry_ms = v.expires_at_ms;
        true
    }

    fn persist_credentials(&self) {
        let (Some(path), Some(sk), Some(addr), Some(secret)) = (
            self.credentials_path(),
            self.proxy_sk.as_ref(),
            self.proxy_addr.as_ref(),
            self.secret.as_ref(),
        ) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let creds = PersistedProxyCreds {
            proxy_sk: sk.clone(),
            proxy_addr: addr.clone(),
            secret: secret.clone(),
            expires_at_ms: self.proxy_expiry_ms,
        };
        if let Ok(text) = serde_json::to_string_pretty(&creds) {
            if let Err(e) = fs::write(&path, text) {
                warn!("failed to persist proxy credentials to {}: {e}", path.display());
            } else {
                info!("persisted proxy credentials to {}", path.display());
            }
        }
    }

    /// Derive a stable proxy key from the EOA key so sessions can be recovered.
    fn deterministic_proxy_keypair(&self) -> ExchangeResult<(String, String)> {
        let eoa = self
            .private_key
            .as_ref()
            .ok_or(ExchangeError::NotConnected)?;
        let eoa_bytes = hex::decode(eoa).map_err(|e| ExchangeError::InvalidKey(e.to_string()))?;
        let mut material = Vec::with_capacity(32 + 24);
        material.extend_from_slice(b"polyGrid-perps-proxy-v1");
        material.extend_from_slice(&eoa_bytes);
        let sk_bytes = keccak(&material);
        let signing = SigningKey::from_bytes((&sk_bytes[..]).into())
            .map_err(|e| ExchangeError::Other(e.to_string()))?;
        let addr = address_from_verifying_key(&signing);
        Ok((hex::encode(sk_bytes), addr))
    }

    /// Delete a proxy credential. Signed as EIP-712 `Op` by the EOA (not the proxy).
    pub async fn delete_proxy_eoa(&self, proxy_addr: &str) -> ExchangeResult<()> {
        let eoa_sk = self
            .private_key
            .as_ref()
            .ok_or(ExchangeError::NotConnected)?;
        let ts = self.server_time_ms().await.unwrap_or_else(|_| Self::now_ms());
        let mut salt_bytes = [0u8; 8];
        rand::thread_rng().fill_bytes(&mut salt_bytes);
        let salt = u64::from_be_bytes(salt_bytes);
        let compact = rmpv::Value::Array(vec![
            rmpv::Value::String("deleteProxy".into()),
            rmpv::Value::Array(vec![rmpv::Value::String(proxy_addr.into())]),
        ]);
        let sig = sign_op(eoa_sk, &compact, salt, ts)?;
        let body = json!({
            "op": {
                "type": "deleteProxy",
                "args": { "proxy": proxy_addr }
            },
            "sig": sig,
            "salt": salt,
            "ts": ts
        });
        let url = format!("{}/v1/account/proxy", self.base_url);
        let resp = self
            .client
            .delete(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ExchangeError::Api(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ExchangeError::Api(e.to_string()))?;
        if !status.is_success() {
            return Err(ExchangeError::Api(friendly_http_error(status, &text)));
        }
        let v: Value = serde_json::from_str(&text).unwrap_or(json!({}));
        if v.get("status").and_then(|s| s.as_str()) == Some("err") {
            return Err(ExchangeError::Api(friendly_pm_error(&v)));
        }
        info!("deleted Polymarket proxy {proxy_addr}");
        Ok(())
    }

    async fn create_proxy_session(&mut self) -> ExchangeResult<()> {
        let eoa_sk = self
            .private_key
            .as_ref()
            .ok_or(ExchangeError::NotConnected)?;
        let owner = self
            .address
            .as_ref()
            .ok_or(ExchangeError::NotConnected)?
            .clone();

        let (proxy_sk_hex, proxy_addr) = self.deterministic_proxy_keypair()?;
        // If this deterministic proxy already exists, delete then recreate to refresh secret.
        let _ = self.delete_proxy_eoa(&proxy_addr).await;

        let ts = self.server_time_ms().await.unwrap_or_else(|_| Self::now_ms());
        let mut salt_bytes = [0u8; 8];
        rand::thread_rng().fill_bytes(&mut salt_bytes);
        let salt = u64::from_be_bytes(salt_bytes);
        let expiry = ts.saturating_add(PROXY_TTL_MS);

        let sig = sign_create_proxy(eoa_sk, &proxy_addr, expiry, salt, ts)?;

        let body = json!({
            "op": {
                "type": "createProxy",
                "args": {
                    "owner": owner,
                    "proxy": proxy_addr,
                    "expiry": expiry
                }
            },
            "sig": sig,
            "salt": salt,
            "ts": ts,
            "label": "polyGrid"
        });
        let url = format!("{}/v1/account/proxy", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ExchangeError::Api(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ExchangeError::Api(e.to_string()))?;
        if !status.is_success() {
            return Err(ExchangeError::Api(friendly_http_error(status, &text)));
        }
        let v: Value =
            serde_json::from_str(&text).map_err(|e| ExchangeError::Api(format!("{e}: {text}")))?;
        let secret = v
            .get("secret")
            .and_then(|s| s.as_str())
            .ok_or_else(|| ExchangeError::Api(format!("createProxy missing secret: {text}")))?
            .to_string();
        if v.get("status").and_then(|s| s.as_str()) == Some("err") {
            return Err(ExchangeError::Api(friendly_pm_error(&v)));
        }
        self.proxy_sk = Some(proxy_sk_hex);
        self.proxy_addr = Some(proxy_addr.clone());
        self.secret = Some(secret);
        self.proxy_expiry_ms = expiry;
        self.persist_credentials();
        info!("Polymarket proxy session ready proxy={proxy_addr} exp={expiry}");
        Ok(())
    }

    async fn server_time_ms(&self) -> ExchangeResult<u64> {
        let url = format!("{}/v1/info/time", self.base_url);
        let v = self.get_public(&url).await?;
        v.get("time")
            .and_then(|t| t.as_u64().or_else(|| t.as_i64().map(|i| i as u64)))
            .ok_or_else(|| ExchangeError::Api("time missing".into()))
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    fn normalize_symbol(&self, symbol: &str) -> String {
        let s = symbol.trim();
        if let Some(meta) = self.lookup_instrument(s) {
            return meta.base.clone();
        }
        base_from_full(s)
    }

    fn lookup_instrument(&self, symbol: &str) -> Option<&InstrumentMeta> {
        let key = symbol.trim().to_ascii_uppercase();
        self.instruments.get(&key).or_else(|| {
            let base = base_from_full(&key).to_ascii_uppercase();
            self.instruments.get(&base)
        })
    }

    fn resolve_instrument(&self, symbol: &str) -> ExchangeResult<InstrumentMeta> {
        self.lookup_instrument(symbol)
            .cloned()
            .ok_or_else(|| ExchangeError::Other(format!("未知交易对 {symbol}（Polymarket Perps）")))
    }

    async fn get_public(&self, url: &str) -> ExchangeResult<Value> {
        let res = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| ExchangeError::Api(e.to_string()))?;
        let status = res.status();
        let text = res
            .text()
            .await
            .map_err(|e| ExchangeError::Api(e.to_string()))?;
        if !status.is_success() {
            return Err(ExchangeError::Api(friendly_http_error(status, &text)));
        }
        serde_json::from_str(&text).map_err(|e| ExchangeError::Api(format!("{e}: {text}")))
    }

    async fn get_private_json(&self, path: &str) -> ExchangeResult<Value> {
        let proxy = self
            .proxy_addr
            .as_ref()
            .ok_or(ExchangeError::NotConnected)?;
        let secret = self.secret.as_ref().ok_or(ExchangeError::NotConnected)?;
        let url = format!("{}{path}", self.base_url);
        let res = self
            .client
            .get(&url)
            .header("polymarket-proxy", proxy)
            .header("polymarket-secret", secret)
            .send()
            .await
            .map_err(|e| ExchangeError::Api(e.to_string()))?;
        let status = res.status();
        let text = res
            .text()
            .await
            .map_err(|e| ExchangeError::Api(e.to_string()))?;
        if !status.is_success() {
            return Err(ExchangeError::Api(friendly_http_error(status, &text)));
        }
        serde_json::from_str(&text).map_err(|e| ExchangeError::Api(format!("{e}: {text}")))
    }

    async fn signed_trade(
        &self,
        method: &str,
        path: &str,
        compact: rmpv::Value,
        op_json: Value,
    ) -> ExchangeResult<Value> {
        let proxy_sk = self.proxy_sk.as_ref().ok_or(ExchangeError::NotConnected)?;
        let ts = self.server_time_ms().await.unwrap_or_else(|_| Self::now_ms());
        let mut salt_bytes = [0u8; 8];
        rand::thread_rng().fill_bytes(&mut salt_bytes);
        let salt = u64::from_be_bytes(salt_bytes);
        let sig = sign_op(proxy_sk, &compact, salt, ts)?;
        let body = json!({
            "op": op_json,
            "sig": sig,
            "salt": salt,
            "ts": ts,
        });
        let url = format!("{}{path}", self.base_url);
        let req = match method {
            "POST" => self.client.post(&url),
            "DELETE" => self.client.delete(&url),
            "PATCH" => self.client.patch(&url),
            other => {
                return Err(ExchangeError::Other(format!("unsupported method {other}")));
            }
        };
        let res = req
            .json(&body)
            .send()
            .await
            .map_err(|e| ExchangeError::Api(e.to_string()))?;
        let status = res.status();
        let text = res
            .text()
            .await
            .map_err(|e| ExchangeError::Api(e.to_string()))?;
        if !status.is_success() {
            return Err(ExchangeError::Api(friendly_http_error(status, &text)));
        }
        serde_json::from_str(&text).map_err(|e| ExchangeError::Api(format!("{e}: {text}")))
    }

    fn round_px(price: Decimal, decimals: u32) -> Decimal {
        round_with_sig_figs(price, decimals, 5)
    }

    fn round_qty(qty: Decimal, decimals: u32) -> Decimal {
        round_with_sig_figs(qty, decimals, 5)
    }

    /// Truncate size toward zero so reduce-only closes never exceed the open position.
    fn floor_qty(qty: Decimal, decimals: u32) -> Decimal {
        qty.round_dp_with_strategy(decimals, RoundingStrategy::ToZero)
    }

    fn decimal_to_wire(d: Decimal) -> String {
        let s = format!("{d}");
        if s.contains('.') {
            let trimmed = s.trim_end_matches('0').trim_end_matches('.');
            if trimmed.is_empty() || trimmed == "-" {
                "0".into()
            } else {
                trimmed.to_string()
            }
        } else {
            s
        }
    }

    fn make_coid(intent: &OrderIntent) -> String {
        if let Some(cloid) = intent.cloid.as_ref().filter(|c| !c.is_empty()) {
            let mut hex: String = cloid
                .chars()
                .filter(|c| c.is_ascii_hexdigit())
                .map(|c| c.to_ascii_lowercase())
                .collect();
            while hex.len() < 32 {
                hex.push('0');
            }
            return hex.chars().take(32).collect();
        }
        let u = uuid::Uuid::new_v4().simple().to_string();
        u.chars().take(32).collect()
    }

    fn build_create_order_wire(
        &self,
        intent: &OrderIntent,
    ) -> ExchangeResult<(rmpv::Value, Value, Decimal, Decimal, String)> {
        let meta = self.resolve_instrument(&intent.symbol)?;
        let is_buy = matches!(intent.side, Side::Buy);
        let px = Self::round_px(intent.price, meta.price_decimals);
        // Reduce-only must not exceed position; truncate toward zero instead of round-half-up.
        let qty = if intent.reduce_only {
            Self::floor_qty(intent.size.abs(), meta.quantity_decimals)
        } else {
            Self::round_qty(intent.size, meta.quantity_decimals)
        };
        let notional = px * qty;
        if notional < meta.min_notional {
            return Err(ExchangeError::Other(format!(
                "订单名义约 {notional} pUSD，低于 Polymarket 最低 ${}。请提高总投入或减少网格数量。",
                meta.min_notional
            )));
        }
        let (tif, po) = match intent.tif {
            grid_engine::TimeInForce::Gtc => ("gtc", false),
            grid_engine::TimeInForce::Ioc => ("ioc", false),
            grid_engine::TimeInForce::Alo => ("gtc", true),
        };
        let coid = Self::make_coid(intent);
        let p_s = Self::decimal_to_wire(px);
        let q_s = Self::decimal_to_wire(qty);
        let ro = intent.reduce_only;

        // Compact must match official SDK `sa()` + `Rs()` filter:
        // [iid, buy, p?, qty, tif?, po, ro?, c?, tr?] with undefined entries omitted.
        // Critically: `ro` is only present when true — a literal `false` breaks the hash.
        let mut compact_order = vec![
            rmpv::Value::Integer(meta.instrument_id.into()),
            rmpv::Value::Boolean(is_buy),
            rmpv::Value::String(p_s.as_str().into()),
            rmpv::Value::String(q_s.as_str().into()),
            rmpv::Value::String(tif.into()),
            rmpv::Value::Boolean(po),
        ];
        if ro {
            compact_order.push(rmpv::Value::Boolean(true));
        }
        compact_order.push(rmpv::Value::String(coid.as_str().into()));

        let mut args_obj = json!({
            "iid": meta.instrument_id,
            "buy": is_buy,
            "p": p_s,
            "qty": q_s,
            "tif": tif,
            "po": po,
            "c": coid,
        });
        if ro {
            args_obj["ro"] = json!(true);
        }

        Ok((
            rmpv::Value::Array(compact_order),
            args_obj,
            px,
            qty,
            coid,
        ))
    }

    /// Official close path: IOC reduce-only with **no price** (market-style).
    /// See <https://docs.polymarket.com/perps/trading#close-a-position>.
    async fn place_reduce_only_market_ioc(
        &mut self,
        symbol: &str,
        is_buy: bool,
        size: Decimal,
    ) -> ExchangeResult<()> {
        let meta = self.resolve_instrument(symbol)?;
        let qty = Self::floor_qty(size.abs(), meta.quantity_decimals);
        if qty <= Decimal::ZERO {
            return Err(ExchangeError::Other(format!(
                "position size {size} for {symbol} is below tradable size precision"
            )));
        }
        if let Ok(mid) = self.get_mid(symbol).await {
            let notional = mid * qty;
            if notional < meta.min_notional {
                return Err(ExchangeError::Other(format!(
                    "close notional ~{notional} pUSD below min ${} for {symbol} (qty {qty})",
                    meta.min_notional
                )));
            }
        }
        let coid = {
            let u = uuid::Uuid::new_v4().simple().to_string();
            u.chars().take(32).collect::<String>()
        };
        let q_s = Self::decimal_to_wire(qty);
        // Compact: [iid, buy, qty, tif, po, ro, c] — price omitted (market IOC).
        let compact_order = vec![
            rmpv::Value::Integer(meta.instrument_id.into()),
            rmpv::Value::Boolean(is_buy),
            rmpv::Value::String(q_s.as_str().into()),
            rmpv::Value::String("ioc".into()),
            rmpv::Value::Boolean(false),
            rmpv::Value::Boolean(true),
            rmpv::Value::String(coid.as_str().into()),
        ];
        let args_obj = json!({
            "iid": meta.instrument_id,
            "buy": is_buy,
            "qty": q_s,
            "tif": "ioc",
            "po": false,
            "ro": true,
            "c": coid,
        });
        let compact = rmpv::Value::Array(vec![
            rmpv::Value::String("createOrders".into()),
            rmpv::Value::Array(vec![rmpv::Value::Array(compact_order)]),
        ]);
        let op_json = json!({
            "type": "createOrders",
            "args": [args_obj]
        });
        let resp = self
            .signed_trade("POST", "/v1/trade/orders", compact, op_json)
            .await?;
        let results = parse_order_acks(&resp, 1)?;
        match results.into_iter().next() {
            Some(Ok(ack)) => {
                info!(
                    "market close {} {} qty={} oid={} filled={}",
                    if is_buy { "buy" } else { "sell" },
                    meta.base,
                    qty,
                    ack.oid,
                    ack.immediately_filled
                );
                Ok(())
            }
            Some(Err(e)) => Err(e),
            None => Err(ExchangeError::Api("empty market-close response".into())),
        }
    }
}

#[async_trait]
impl Exchange for PolymarketExchange {
    fn mode(&self) -> RunMode {
        self.mode
    }

    async fn connect(&mut self) -> ExchangeResult<()> {
        self.refresh_instruments().await?;
        if self.private_key.is_some() {
            self.ensure_session().await?;
        }
        Ok(())
    }

    async fn get_mid(&self, symbol: &str) -> ExchangeResult<Decimal> {
        let meta = if self.instruments.is_empty() {
            // Public mid without prior connect: resolve from live instruments list.
            let url = format!("{}/v1/info/instruments", self.base_url);
            let raw = self.get_public(&url).await?;
            let arr = raw
                .as_array()
                .ok_or_else(|| ExchangeError::Api("instruments not array".into()))?;
            let want = symbol.trim().to_ascii_uppercase();
            let want_base = base_from_full(&want).to_ascii_uppercase();
            let found = arr.iter().find(|item| {
                let full = item
                    .get("symbol")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_ascii_uppercase();
                let base = item
                    .get("base_asset")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_ascii_uppercase();
                full == want
                    || base == want
                    || full == want_base
                    || base == want_base
                    || full == format!("{want}-USD")
            });
            let item = found.ok_or_else(|| {
                ExchangeError::Other(format!("mid not found for {symbol}"))
            })?;
            InstrumentMeta {
                instrument_id: item
                    .get("instrument_id")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                base: item
                    .get("base_asset")
                    .and_then(|s| s.as_str())
                    .unwrap_or(symbol)
                    .to_string(),
                full_symbol: item
                    .get("symbol")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                quantity_decimals: 0,
                price_decimals: 0,
                min_notional: Decimal::ONE,
                max_leverage: 20,
                isolated_only: false,
            }
        } else {
            self.resolve_instrument(symbol)?
        };
        let url = format!("{}/v1/info/tickers", self.base_url);
        let tickers = self.get_public(&url).await?;
        let arr = tickers
            .as_array()
            .ok_or_else(|| ExchangeError::Api("tickers not array".into()))?;
        for t in arr {
            let iid = t.get("instrument_id").and_then(|v| v.as_u64()).unwrap_or(0);
            let sym = t.get("symbol").and_then(|s| s.as_str()).unwrap_or("");
            if iid == meta.instrument_id
                || sym.eq_ignore_ascii_case(&meta.full_symbol)
                || sym.eq_ignore_ascii_case(symbol)
            {
                if let Some(mid) = json_decimal(t.get("mid_price"))
                    .or_else(|| json_decimal(t.get("mark_price")))
                    .filter(|m| *m > Decimal::ZERO)
                {
                    return Ok(mid);
                }
            }
        }
        Err(ExchangeError::Other(format!("mid not found for {symbol}")))
    }

    async fn get_balances(&self) -> ExchangeResult<Vec<Balance>> {
        if self.secret.is_none() {
            return Err(ExchangeError::NotConnected);
        }
        let mut out = Vec::new();

        if let Ok(bals) = self.get_private_json("/v1/account/balances").await {
            if let Some(arr) = bals.as_array() {
                for b in arr {
                    let asset = b
                        .get("asset")
                        .and_then(|a| a.as_str())
                        .unwrap_or("pUSD")
                        .to_string();
                    let total = json_decimal(b.get("balance"))
                        .or_else(|| json_decimal(b.get("value")))
                        .unwrap_or(Decimal::ZERO);
                    if total == Decimal::ZERO {
                        continue;
                    }
                    out.push(Balance {
                        asset,
                        total,
                        available: total,
                        kind: "perp".into(),
                    });
                }
            }
        }

        if let Ok(portfolio) = self.get_private_json("/v1/account/portfolio").await {
            let equity = portfolio
                .get("margin")
                .and_then(|m| json_decimal(m.get("total_account_value")))
                .unwrap_or(Decimal::ZERO);
            let free = json_decimal(portfolio.get("withdrawable")).unwrap_or(equity);
            if equity != Decimal::ZERO
                && !out
                    .iter()
                    .any(|b| b.asset.eq_ignore_ascii_case("pUSD") || b.asset.eq_ignore_ascii_case("USDC"))
            {
                out.push(Balance {
                    asset: "pUSD".into(),
                    total: equity,
                    available: free,
                    kind: "unified".into(),
                });
            } else if let Some(row) = out.iter_mut().find(|b| {
                b.asset.eq_ignore_ascii_case("pUSD") || b.asset.eq_ignore_ascii_case("USDC")
            }) {
                row.total = row.total.max(equity);
                row.available = row.available.max(free);
                if row.kind == "perp" {
                    row.kind = "unified".into();
                }
            }
            if let Some(positions) = portfolio.get("positions").and_then(|a| a.as_array()) {
                for p in positions {
                    let size = json_decimal(p.get("size")).unwrap_or(Decimal::ZERO);
                    if size == Decimal::ZERO {
                        continue;
                    }
                    let sym = p
                        .get("symbol")
                        .and_then(|s| s.as_str())
                        .map(base_from_full)
                        .unwrap_or_else(|| "POS".into());
                    out.push(Balance {
                        asset: sym,
                        total: size,
                        available: size,
                        kind: "position".into(),
                    });
                }
            }
        }

        if out.is_empty() {
            out.push(Balance {
                asset: "pUSD".into(),
                total: Decimal::ZERO,
                available: Decimal::ZERO,
                kind: "perp".into(),
            });
        }
        Ok(out)
    }

    async fn place_order(&mut self, intent: OrderIntent) -> ExchangeResult<LiveOrder> {
        let mut orders = self.place_orders(vec![intent]).await?;
        orders
            .pop()
            .ok_or_else(|| ExchangeError::Other("empty place_orders result".into()))
    }

    async fn place_orders(&mut self, intents: Vec<OrderIntent>) -> ExchangeResult<Vec<LiveOrder>> {
        if intents.is_empty() {
            return Ok(vec![]);
        }
        self.ensure_session().await?;
        if self.instruments.is_empty() {
            self.refresh_instruments().await?;
        }

        const CHUNK: usize = 40;
        let mut placed = Vec::with_capacity(intents.len());
        for chunk in intents.chunks(CHUNK) {
            let mut compact_orders = Vec::with_capacity(chunk.len());
            let mut json_orders = Vec::with_capacity(chunk.len());
            let mut prepared = Vec::with_capacity(chunk.len());
            for intent in chunk {
                let (compact_ord, json_ord, px, qty, coid) =
                    self.build_create_order_wire(intent)?;
                compact_orders.push(compact_ord);
                json_orders.push(json_ord);
                prepared.push((intent, px, qty, coid));
            }
            let compact = rmpv::Value::Array(vec![
                rmpv::Value::String("createOrders".into()),
                rmpv::Value::Array(compact_orders),
            ]);
            let op_json = json!({
                "type": "createOrders",
                "args": json_orders
            });
            let resp = self
                .signed_trade("POST", "/v1/trade/orders", compact, op_json)
                .await?;
            let results = parse_order_acks(&resp, prepared.len())?;
            let mut errors = Vec::new();
            for ((intent, px, qty, coid), result) in prepared.into_iter().zip(results) {
                match result {
                    Ok(ack) => {
                        let mut order =
                            LiveOrder::from_intent(intent, Some(ack.oid.to_string()));
                        order.price = px;
                        order.size = qty;
                        order.orig_size = qty;
                        order.cloid = Some(coid);
                        if ack.immediately_filled {
                            warn!(
                                "order immediately filled oid={} {} {} @ {}",
                                ack.oid, intent.symbol, qty, px
                            );
                            self.pending_immediate_fills.push(FillEvent {
                                client_id: intent.client_id.clone(),
                                symbol: intent.symbol.clone(),
                                side: intent.side,
                                price: px,
                                size: qty,
                                level_index: intent.level_index,
                                fee: Decimal::ZERO,
                                fee_token: None,
                                exchange_tid: None,
                                exchange_oid: Some(ack.oid.to_string()),
                                cloid: order.cloid.clone(),
                                exchange_time_ms: Some(Self::now_ms() as i64),
                                crossed: true,
                                dir: None,
                                closed_pnl: None,
                            });
                        } else {
                            self.open_orders
                                .insert(order.client_id.clone(), order.clone());
                            placed.push(order);
                        }
                    }
                    Err(e) => errors.push(e.to_string()),
                }
            }
            if !errors.is_empty() {
                let n = placed.len();
                let ids: Vec<String> = placed.iter().map(|o| o.client_id.clone()).collect();
                for id in ids {
                    let _ = self.cancel_order(&id).await;
                }
                placed.clear();
                return Err(ExchangeError::Api(summarize_batch_place_errors(n, &errors)));
            }
        }
        Ok(placed)
    }

    async fn cancel_order(&mut self, client_id: &str) -> ExchangeResult<()> {
        self.ensure_session().await?;
        if let Some(order) = self.open_orders.get(client_id).cloned() {
            if let Some(oid_str) = &order.exchange_id {
                if let Ok(oid) = oid_str.parse::<u64>() {
                    let compact = rmpv::Value::Array(vec![
                        rmpv::Value::String("cancelOrders".into()),
                        rmpv::Value::Array(vec![rmpv::Value::Integer(oid.into())]),
                    ]);
                    let op_json = json!({
                        "type": "cancelOrders",
                        "args": [oid]
                    });
                    let resp = self
                        .signed_trade("DELETE", "/v1/trade/orders", compact, op_json)
                        .await?;
                    if resp_has_error(&resp) {
                        // Fallback: cancel by client order id.
                        if let Some(coid) = order.cloid.as_ref() {
                            let compact = rmpv::Value::Array(vec![
                                rmpv::Value::String("cancelOrdersCOID".into()),
                                rmpv::Value::Array(vec![rmpv::Value::String(
                                    coid.as_str().into(),
                                )]),
                            ]);
                            let op_json = json!({
                                "type": "cancelOrdersCOID",
                                "args": [coid]
                            });
                            let _ = self
                                .signed_trade(
                                    "DELETE",
                                    "/v1/trade/orders-coid",
                                    compact,
                                    op_json,
                                )
                                .await;
                        }
                    }
                }
            } else if let Some(coid) = order.cloid.as_ref() {
                let compact = rmpv::Value::Array(vec![
                    rmpv::Value::String("cancelOrdersCOID".into()),
                    rmpv::Value::Array(vec![rmpv::Value::String(coid.as_str().into())]),
                ]);
                let op_json = json!({
                    "type": "cancelOrdersCOID",
                    "args": [coid]
                });
                let _ = self
                    .signed_trade("DELETE", "/v1/trade/orders-coid", compact, op_json)
                    .await;
            }
        }
        self.open_orders.remove(client_id);
        Ok(())
    }

    async fn cancel_all(&mut self, symbol: &str) -> ExchangeResult<()> {
        self.ensure_session().await?;
        if self.instruments.is_empty() {
            self.refresh_instruments().await?;
        }
        // Compact op name is `cancelAll` (SDK method is cancelAllOrders). Wrong name
        // fails EIP-712 verify and the API often returns "account not found for proxy".
        let (compact, op_json) = if symbol.is_empty() {
            (
                rmpv::Value::Array(vec![
                    rmpv::Value::String("cancelAll".into()),
                    rmpv::Value::Array(vec![]),
                ]),
                json!({"type": "cancelAll", "args": {}}),
            )
        } else {
            let meta = self.resolve_instrument(symbol)?;
            (
                rmpv::Value::Array(vec![
                    rmpv::Value::String("cancelAll".into()),
                    rmpv::Value::Array(vec![rmpv::Value::Integer(meta.instrument_id.into())]),
                ]),
                json!({"type": "cancelAll", "args": {"iid": meta.instrument_id}}),
            )
        };
        let resp = self
            .signed_trade("DELETE", "/v1/trade/orders/all", compact, op_json)
            .await;
        if let Err(e) = resp {
            warn!("cancelAll via /orders/all failed: {e}; falling back to cancel by oid");
            // Fallback: cancel listed open orders individually.
            let open = if symbol.is_empty() {
                self.get_private_json("/v1/account/open-orders")
                    .await
                    .unwrap_or(json!([]))
            } else {
                let meta = self.resolve_instrument(symbol)?;
                self.get_private_json(&format!(
                    "/v1/account/open-orders?instrument_id={}",
                    meta.instrument_id
                ))
                .await
                .unwrap_or(json!([]))
            };
            if let Some(arr) = open_orders_array(&open) {
                let oids: Vec<u64> = arr.iter().filter_map(|o| json_oid(o)).collect();
                for chunk in oids.chunks(40) {
                    if chunk.is_empty() {
                        continue;
                    }
                    let compact = rmpv::Value::Array(vec![
                        rmpv::Value::String("cancelOrders".into()),
                        rmpv::Value::Array(
                            chunk
                                .iter()
                                .map(|o| rmpv::Value::Integer((*o).into()))
                                .collect(),
                        ),
                    ]);
                    let op_json = json!({"type": "cancelOrders", "args": chunk});
                    let _ = self
                        .signed_trade("DELETE", "/v1/trade/orders", compact, op_json)
                        .await;
                }
            }
        }
        if symbol.is_empty() {
            self.open_orders.clear();
        } else {
            let key = self.normalize_symbol(symbol);
            let drop_ids: Vec<String> = self
                .open_orders
                .iter()
                .filter(|(_, o)| {
                    let ok = self.normalize_symbol(&o.symbol);
                    ok.eq_ignore_ascii_case(&key) || o.symbol.eq_ignore_ascii_case(symbol)
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in drop_ids {
                self.open_orders.remove(&id);
            }
        }
        Ok(())
    }

    async fn close_position(&mut self, symbol: &str) -> ExchangeResult<()> {
        self.ensure_session().await?;
        if self.instruments.is_empty() {
            self.refresh_instruments().await?;
        }
        // Prefer official market-style IOC (no price); fall back to aggressive limits.
        const MAX_ATTEMPTS: usize = 6;
        let mut last_err: Option<String> = None;
        for attempt in 0..MAX_ATTEMPTS {
            let (size, _, _, _) = self.get_perp_position(symbol).await?;
            if size == Decimal::ZERO {
                return Ok(());
            }
            let meta = self.resolve_instrument(symbol)?;
            let abs_sz = Self::floor_qty(size.abs(), meta.quantity_decimals);
            if abs_sz <= Decimal::ZERO {
                // Dust below lot size — nothing tradable left.
                warn!(
                    "close_position: {symbol} residual {size} below qty precision; treating as flat"
                );
                return Ok(());
            }
            let is_buy = size < Decimal::ZERO;
            let use_limit_fallback = attempt >= 3;
            let result = if use_limit_fallback {
                let mid = self.get_mid(symbol).await?;
                // Wider slip than before (5% → 15%/25%) so thin books still take.
                let slip = if attempt >= 5 {
                    dec!(0.75)
                } else {
                    dec!(0.85)
                };
                let raw_px = if is_buy {
                    mid * (Decimal::ONE + (Decimal::ONE - slip))
                } else {
                    mid * slip
                };
                let px = Self::round_px(raw_px, meta.price_decimals);
                let intent = OrderIntent {
                    client_id: format!("close-{}", uuid::Uuid::new_v4()),
                    symbol: meta.base.clone(),
                    side: if is_buy { Side::Buy } else { Side::Sell },
                    price: px,
                    size: abs_sz,
                    level_index: 0,
                    reduce_only: true,
                    tif: grid_engine::TimeInForce::Ioc,
                    cloid: None,
                };
                self.place_orders(vec![intent]).await.map(|_| ())
            } else {
                self.place_reduce_only_market_ioc(&meta.base, is_buy, abs_sz)
                    .await
            };
            match result {
                Ok(()) => {}
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("immediately") || msg.contains("empty place") {
                        // Soft: ACK quirks after fill.
                    } else {
                        warn!(
                            "close_position attempt {}/{}: {e}",
                            attempt + 1,
                            MAX_ATTEMPTS
                        );
                        last_err = Some(msg);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(300 + attempt as u64 * 150)).await;
        }
        let (remaining, _, _, _) = self.get_perp_position(symbol).await?;
        if remaining != Decimal::ZERO {
            let detail = last_err
                .map(|e| format!("; last error: {e}"))
                .unwrap_or_default();
            return Err(ExchangeError::Other(format!(
                "failed to fully close {symbol}; remaining position {remaining}{detail}"
            )));
        }
        Ok(())
    }

    async fn flatten(&mut self) -> ExchangeResult<()> {
        self.ensure_session().await?;
        if self.instruments.is_empty() {
            self.refresh_instruments().await?;
        }
        let _ = self.cancel_all("").await;
        let portfolio = self
            .get_private_json("/v1/account/portfolio")
            .await
            .unwrap_or(json!({}));
        let mut close_syms: Vec<(String, Decimal)> = Vec::new();
        if let Some(positions) = portfolio.get("positions").and_then(|a| a.as_array()) {
            for p in positions {
                let size = json_decimal(p.get("size")).unwrap_or(Decimal::ZERO);
                if size == Decimal::ZERO {
                    continue;
                }
                let sym = p
                    .get("symbol")
                    .and_then(|s| s.as_str())
                    .map(base_from_full)
                    .or_else(|| {
                        p.get("instrument_id")
                            .and_then(|v| v.as_u64())
                            .and_then(|id| self.id_to_base.get(&id).cloned())
                    })
                    .unwrap_or_default();
                if sym.is_empty() {
                    continue;
                }
                if !close_syms.iter().any(|(s, _)| s == &sym) {
                    close_syms.push((sym, size));
                }
            }
        }
        if close_syms.is_empty() {
            info!("flatten: nothing to cancel or close");
            self.open_orders.clear();
            return Ok(());
        }
        for (sym, _) in close_syms {
            if let Err(e) = self.close_position(&sym).await {
                warn!("flatten close {sym}: {e}");
            }
        }
        self.open_orders.clear();
        Ok(())
    }

    async fn drain_fills(&mut self) -> ExchangeResult<Vec<FillEvent>> {
        let mut out = std::mem::take(&mut self.pending_immediate_fills);
        if self.secret.is_none() {
            return Ok(out);
        }
        if !self.fills_primed {
            self.prime_seen_fills().await?;
        }
        let fills = self
            .get_private_json("/v1/account/fills")
            .await
            .unwrap_or(json!({}));
        let arr = fills_array(&fills);
        let min_fill_ms = self.session_start_ms.saturating_sub(3_000);
        for f in arr.iter().take(200) {
            let tid = fill_tid(f);
            if self.last_seen_fills.contains(&tid) {
                continue;
            }
            let fill_ms = f
                .get("timestamp")
                .or_else(|| f.get("ts"))
                .and_then(|t| t.as_u64().or_else(|| t.as_i64().map(|i| i as u64)))
                .unwrap_or(0);
            if fill_ms > 0 && fill_ms < min_fill_ms {
                self.last_seen_fills.push(tid);
                if self.last_seen_fills.len() > 500 {
                    self.last_seen_fills.drain(0..100);
                }
                continue;
            }

            let iid = f
                .get("instrument_id")
                .or_else(|| f.get("iid"))
                .and_then(|v| v.as_u64());
            let coin = iid
                .and_then(|id| self.id_to_base.get(&id).cloned())
                .unwrap_or_default();
            let side = match f.get("side").and_then(|s| s.as_str()) {
                Some("long") | Some("buy") | Some("B") | Some("Buy") => Side::Buy,
                _ => Side::Sell,
            };
            let px = json_decimal(f.get("price"))
                .or_else(|| json_decimal(f.get("p")))
                .unwrap_or(Decimal::ZERO);
            let sz = json_decimal(f.get("quantity"))
                .or_else(|| json_decimal(f.get("qty")))
                .unwrap_or(Decimal::ZERO);
            let fee = json_decimal(f.get("fee")).unwrap_or(Decimal::ZERO);
            let fee_token = f
                .get("fee_asset")
                .or_else(|| f.get("fea"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let oid = json_oid(f).map(|o| o.to_string());
            let crossed = f.get("taker").and_then(|v| v.as_bool()).unwrap_or(false);
            let closed_pnl = json_decimal(f.get("pnl"));

            let matched = self.open_orders.values().find(|o| match (&oid, &o.exchange_id) {
                (Some(fill_oid), Some(ex_id)) if fill_oid == ex_id => true,
                _ => false,
            });
            let Some(order) = matched.cloned() else {
                if !self.open_orders.is_empty() {
                    self.last_seen_fills.push(tid.clone());
                    if self.last_seen_fills.len() > 500 {
                        self.last_seen_fills.drain(0..100);
                    }
                }
                continue;
            };

            self.last_seen_fills.push(tid.clone());
            if self.last_seen_fills.len() > 500 {
                self.last_seen_fills.drain(0..100);
            }

            let client_id = order.client_id.clone();
            let level_index = order.level_index;
            if let Some(tracked) = self.open_orders.get_mut(&client_id) {
                let remaining = tracked.size - sz;
                if remaining.abs() <= Decimal::new(1, 8) || remaining <= Decimal::ZERO {
                    self.open_orders.remove(&client_id);
                } else {
                    tracked.size = remaining;
                }
            }
            out.push(FillEvent {
                client_id,
                symbol: if coin.is_empty() {
                    order.symbol.clone()
                } else {
                    coin
                },
                side,
                price: px,
                size: sz,
                level_index,
                fee,
                fee_token,
                exchange_tid: Some(tid),
                exchange_oid: oid,
                cloid: order.cloid.clone(),
                exchange_time_ms: if fill_ms > 0 {
                    Some(fill_ms as i64)
                } else {
                    None
                },
                crossed,
                dir: None,
                closed_pnl,
            });
        }
        Ok(out)
    }

    async fn list_open_orders(&self, symbol: &str) -> ExchangeResult<Vec<LiveOrder>> {
        let key = self.normalize_symbol(symbol);
        Ok(self
            .open_orders
            .values()
            .filter(|o| {
                self.normalize_symbol(&o.symbol)
                    .eq_ignore_ascii_case(&key)
                    || o.symbol.eq_ignore_ascii_case(symbol)
            })
            .cloned()
            .collect())
    }

    async fn list_exchange_open_orders(&self, symbol: &str) -> ExchangeResult<Vec<LiveOrder>> {
        if self.secret.is_none() {
            return Ok(vec![]);
        }
        let path = if symbol.is_empty() {
            "/v1/account/open-orders".to_string()
        } else {
            let meta = self.resolve_instrument(symbol)?;
            format!(
                "/v1/account/open-orders?instrument_id={}",
                meta.instrument_id
            )
        };
        let open = self.get_private_json(&path).await.unwrap_or(json!([]));
        let mut out = Vec::new();
        if let Some(arr) = open_orders_array(&open) {
            for o in arr {
                let oid = json_oid(o).map(|v| v.to_string());
                let side = if o.get("buy").and_then(|b| b.as_bool()).unwrap_or(false) {
                    Side::Buy
                } else {
                    Side::Sell
                };
                let px = json_decimal(o.get("price"))
                    .or_else(|| json_decimal(o.get("p")))
                    .unwrap_or(Decimal::ZERO);
                let sz = json_decimal(o.get("resting_quantity"))
                    .or_else(|| json_decimal(o.get("rest")))
                    .or_else(|| json_decimal(o.get("quantity")))
                    .or_else(|| json_decimal(o.get("qty")))
                    .unwrap_or(Decimal::ZERO);
                let cloid = o
                    .get("client_order_id")
                    .or_else(|| o.get("c"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let iid = o
                    .get("instrument_id")
                    .or_else(|| o.get("iid"))
                    .and_then(|v| v.as_u64());
                let coin = iid
                    .and_then(|id| self.id_to_base.get(&id).cloned())
                    .unwrap_or_else(|| symbol.to_string());
                let local = self.open_orders.values().find(|lo| {
                    lo.exchange_id.as_ref() == oid.as_ref()
                        || (cloid.is_some() && lo.cloid == cloid)
                });
                let client_id = local
                    .map(|l| l.client_id.clone())
                    .unwrap_or_else(|| format!("ex-{}", oid.clone().unwrap_or_default()));
                out.push(LiveOrder {
                    client_id,
                    exchange_id: oid,
                    symbol: coin,
                    side,
                    price: px,
                    size: sz,
                    orig_size: local.map(|l| l.orig_size).unwrap_or(sz),
                    level_index: local.map(|l| l.level_index).unwrap_or(0),
                    reduce_only: o
                        .get("ro")
                        .or_else(|| o.get("reduce_only"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    cloid,
                });
            }
        }
        Ok(out)
    }

    async fn get_position(&self, symbol: &str) -> ExchangeResult<crate::traits::PositionSnapshot> {
        let (size, entry, upnl, liq) = self.get_perp_position(symbol).await?;
        Ok(crate::traits::PositionSnapshot {
            symbol: symbol.to_string(),
            size,
            entry_price: entry,
            unrealized_pnl: upnl,
            liquidation_price: liq,
        })
    }

    async fn cancel_all_confirmed(
        &mut self,
        symbol: &str,
        max_attempts: u32,
    ) -> ExchangeResult<crate::traits::CancelReport> {
        use crate::traits::CancelReport;
        let mut last_remaining = Vec::new();
        let attempts = max_attempts.max(1);
        for attempt in 0..attempts {
            self.cancel_all(symbol).await?;
            tokio::time::sleep(Duration::from_millis(200 * (attempt as u64 + 1))).await;
            let still = self.has_open_orders(symbol).await?;
            if !still {
                return Ok(CancelReport {
                    canceled: 0,
                    remaining_oids: vec![],
                    confirmed_flat: true,
                });
            }
            let open = self
                .list_exchange_open_orders(symbol)
                .await
                .unwrap_or_default();
            last_remaining = open.into_iter().filter_map(|o| o.exchange_id).collect();
        }
        Ok(CancelReport {
            canceled: 0,
            remaining_oids: last_remaining,
            confirmed_flat: false,
        })
    }

    async fn list_spot_symbols(&self) -> ExchangeResult<Vec<String>> {
        let markets = self.list_markets().await?;
        Ok(markets.into_iter().map(|m| m.symbol).collect())
    }

    async fn list_markets(&self) -> ExchangeResult<Vec<MarketInfo>> {
        list_live_markets(self.mode).await
    }
}

#[derive(Debug, Clone)]
struct PlacedOrderAck {
    oid: u64,
    immediately_filled: bool,
}

fn parse_order_acks(
    resp: &Value,
    expected: usize,
) -> ExchangeResult<Vec<ExchangeResult<PlacedOrderAck>>> {
    let statuses = resp
        .as_array()
        .cloned()
        .or_else(|| {
            resp.get("data")
                .and_then(|d| d.as_array())
                .cloned()
        })
        .ok_or_else(|| ExchangeError::Api(format!("unexpected order response: {resp}")))?;
    if statuses.len() != expected {
        // Some gateways wrap a single ack as object.
        if expected == 1 && !resp.is_array() {
            return Ok(vec![parse_one_ack(resp)]);
        }
        return Err(ExchangeError::Api(format!(
            "expected {expected} order statuses, got {}",
            statuses.len()
        )));
    }
    Ok(statuses.iter().map(parse_one_ack).collect())
}

fn parse_one_ack(status: &Value) -> ExchangeResult<PlacedOrderAck> {
    if status.get("status").and_then(|s| s.as_str()) == Some("err") {
        return Err(ExchangeError::Api(
            status
                .get("error")
                .map(|e| e.to_string())
                .unwrap_or_else(|| status.to_string()),
        ));
    }
    if let Some(err) = status.get("error").and_then(|e| e.as_str()) {
        return Err(ExchangeError::Api(err.to_string()));
    }
    let oid = status
        .get("oid")
        .or_else(|| status.get("order_id"))
        .and_then(|o| o.as_u64().or_else(|| o.as_i64().map(|i| i as u64)))
        .ok_or_else(|| ExchangeError::Api(format!("order ack missing oid: {status}")))?;
    Ok(PlacedOrderAck {
        oid,
        immediately_filled: false,
    })
}

fn resp_has_error(resp: &Value) -> bool {
    if resp.get("status").and_then(|s| s.as_str()) == Some("err") {
        return true;
    }
    if let Some(arr) = resp.as_array() {
        return arr.iter().any(|x| {
            x.get("status").and_then(|s| s.as_str()) == Some("err") || x.get("error").is_some()
        });
    }
    false
}

fn summarize_batch_place_errors(placed: usize, errors: &[String]) -> String {
    let joined = errors.join("; ");
    let lower = joined.to_ascii_lowercase();
    if lower.contains("insufficient") || lower.contains("margin") {
        return format!(
            "保证金不足，无法挂完全部网格单（已撤销本次成功的 {placed} 笔）。\
请降低「总名义投入」或网格数量、提高可用 pUSD 保证金后重试。"
        );
    }
    let mut uniq = Vec::new();
    for e in errors {
        if !uniq.iter().any(|u: &String| u == e) {
            uniq.push(e.clone());
        }
        if uniq.len() >= 3 {
            break;
        }
    }
    format!(
        "批量挂单部分失败（已撤销本次成功的 {placed} 笔）: {}",
        uniq.join("；")
    )
}

fn friendly_pm_error(resp: &Value) -> String {
    let text = resp.to_string();
    if text.to_ascii_lowercase().contains("unauthorized")
        || text.to_ascii_lowercase().contains("auth")
    {
        return format!(
            "Polymarket 鉴权失败。请确认私钥对应 EOA 已在 https://polymarket.com 入金 pUSD，\
并重试以重建代理会话。原始错误: {text}"
        );
    }
    text
}

fn friendly_http_error(status: reqwest::StatusCode, text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if status.as_u16() == 401 || lower.contains("unauthorized") {
        return format!(
            "Polymarket 账户未授权或代理会话失效。请确认已在 https://polymarket.com 充值 pUSD，\
并用同一 EOA 私钥重试。HTTP {status}: {text}"
        );
    }
    format!("{status}: {text}")
}

fn fill_tid(f: &Value) -> String {
    f.get("trade_id")
        .or_else(|| f.get("tid"))
        .map(|t| t.to_string())
        .unwrap_or_else(|| f.to_string())
}

fn json_oid(v: &Value) -> Option<u64> {
    v.get("order_id")
        .or_else(|| v.get("oid"))
        .and_then(|o| o.as_u64().or_else(|| o.as_i64().map(|i| i as u64)))
}

fn open_orders_array(open: &Value) -> Option<&Vec<Value>> {
    open.as_array().or_else(|| open.get("data").and_then(|d| d.as_array()))
}

fn fills_array(fills: &Value) -> Vec<Value> {
    fills
        .as_array()
        .cloned()
        .or_else(|| fills.get("data").and_then(|d| d.as_array()).cloned())
        .unwrap_or_default()
}

fn base_from_full(symbol: &str) -> String {
    let s = symbol.trim();
    if let Some((base, _)) = s.rsplit_once('-') {
        base.to_string()
    } else if let Some((base, _)) = s.rsplit_once('/') {
        base.to_string()
    } else {
        s.to_string()
    }
}

fn json_decimal(v: Option<&Value>) -> Option<Decimal> {
    let v = v?;
    if let Some(s) = v.as_str() {
        return Decimal::from_str(s).ok();
    }
    if let Some(n) = v.as_f64() {
        return Decimal::from_str(&n.to_string()).ok();
    }
    if let Some(n) = v.as_i64() {
        return Some(Decimal::from(n));
    }
    if let Some(n) = v.as_u64() {
        return Some(Decimal::from(n));
    }
    None
}

fn keccak(data: &[u8]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(data);
    let out = hasher.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
}

fn u256_bytes(v: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[24..].copy_from_slice(&v.to_be_bytes());
    out
}

fn address_from_verifying_key(signing_key: &SigningKey) -> String {
    let verifying = signing_key.verifying_key();
    let point = verifying.to_encoded_point(false);
    let hash = Keccak256::digest(&point.as_bytes()[1..]);
    format!("0x{}", hex::encode(&hash[12..]))
}

fn parse_address20(addr: &str) -> ExchangeResult<[u8; 20]> {
    let hex = addr.trim().trim_start_matches("0x");
    let bytes = hex::decode(hex).map_err(|e| ExchangeError::Other(e.to_string()))?;
    if bytes.len() != 20 {
        return Err(ExchangeError::Other("address must be 20 bytes".into()));
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn encode_address_word(addr: &str) -> ExchangeResult<[u8; 32]> {
    let a = parse_address20(addr)?;
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(&a);
    Ok(out)
}

fn polymarket_domain_separator() -> [u8; 32] {
    // EIP712Domain(string name,string version,uint256 chainId) — no verifyingContract
    let domain_type_hash =
        keccak(b"EIP712Domain(string name,string version,uint256 chainId)");
    let name_hash = keccak(b"Polymarket");
    let version_hash = keccak(b"1");
    let mut domain = Vec::new();
    domain.extend_from_slice(&domain_type_hash);
    domain.extend_from_slice(&name_hash);
    domain.extend_from_slice(&version_hash);
    domain.extend_from_slice(&u256_bytes(CHAIN_ID));
    keccak(&domain)
}

fn sign_digest(sk_hex: &str, digest: &[u8; 32]) -> ExchangeResult<String> {
    let signing_key = SigningKey::from_bytes((&hex::decode(sk_hex).unwrap()[..]).into())
        .map_err(|e| ExchangeError::InvalidKey(e.to_string()))?;
    let recoverable = signing_key
        .sign_prehash_recoverable(digest.as_slice())
        .map_err(|e| ExchangeError::Other(e.to_string()))?;
    let (sig, recid): (Signature, RecoveryId) = recoverable;
    let sig_bytes = sig.to_bytes();
    let mut out = Vec::with_capacity(65);
    out.extend_from_slice(&sig_bytes[..32]);
    out.extend_from_slice(&sig_bytes[32..64]);
    out.push(27 + recid.to_byte());
    Ok(format!("0x{}", hex::encode(out)))
}

fn sign_create_proxy(
    eoa_sk: &str,
    proxy_addr: &str,
    exp: u64,
    salt: u64,
    ts: u64,
) -> ExchangeResult<String> {
    let domain_separator = polymarket_domain_separator();
    let type_hash =
        keccak(b"CreateProxy(address addr,uint64 exp,uint64 salt,uint64 ts)");
    let mut msg = Vec::new();
    msg.extend_from_slice(&type_hash);
    msg.extend_from_slice(&encode_address_word(proxy_addr)?);
    msg.extend_from_slice(&u256_bytes(exp));
    msg.extend_from_slice(&u256_bytes(salt));
    msg.extend_from_slice(&u256_bytes(ts));
    let struct_hash = keccak(&msg);
    let mut digest_input = Vec::with_capacity(66);
    digest_input.extend_from_slice(&[0x19, 0x01]);
    digest_input.extend_from_slice(&domain_separator);
    digest_input.extend_from_slice(&struct_hash);
    let digest = keccak(&digest_input);
    sign_digest(eoa_sk, &digest)
}

fn pack_compact(op: &rmpv::Value) -> ExchangeResult<Vec<u8>> {
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, op)
        .map_err(|e| ExchangeError::Other(format!("msgpack encode: {e}")))?;
    Ok(buf)
}

fn sign_op(proxy_sk: &str, compact: &rmpv::Value, salt: u64, ts: u64) -> ExchangeResult<String> {
    let packed = pack_compact(compact)?;
    let data = keccak(&packed);
    let domain_separator = polymarket_domain_separator();
    let type_hash = keccak(b"Op(bytes32 data,uint64 salt,uint64 ts)");
    let mut msg = Vec::new();
    msg.extend_from_slice(&type_hash);
    msg.extend_from_slice(&data);
    msg.extend_from_slice(&u256_bytes(salt));
    msg.extend_from_slice(&u256_bytes(ts));
    let struct_hash = keccak(&msg);
    let mut digest_input = Vec::with_capacity(66);
    digest_input.extend_from_slice(&[0x19, 0x01]);
    digest_input.extend_from_slice(&domain_separator);
    digest_input.extend_from_slice(&struct_hash);
    let digest = keccak(&digest_input);
    sign_digest(proxy_sk, &digest)
}

/// Candlestick interval labels used by UI / ATR (mapped to Polymarket klines).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CandleInterval {
    #[serde(rename = "1m")]
    M1,
    #[serde(rename = "3m")]
    M3,
    #[serde(rename = "5m")]
    M5,
    #[serde(rename = "15m")]
    M15,
    #[serde(rename = "30m")]
    M30,
    #[serde(rename = "1h")]
    H1,
    #[serde(rename = "2h")]
    H2,
    #[serde(rename = "4h")]
    H4,
    #[serde(rename = "8h")]
    H8,
    #[serde(rename = "12h")]
    H12,
    #[serde(rename = "1d")]
    D1,
    #[serde(rename = "3d")]
    D3,
    #[serde(rename = "1w")]
    W1,
    #[serde(rename = "1M")]
    Mo1,
}

impl CandleInterval {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::M1 => "1m",
            Self::M3 => "3m",
            Self::M5 => "5m",
            Self::M15 => "15m",
            Self::M30 => "30m",
            Self::H1 => "1h",
            Self::H2 => "2h",
            Self::H4 => "4h",
            Self::H8 => "8h",
            Self::H12 => "12h",
            Self::D1 => "1d",
            Self::D3 => "3d",
            Self::W1 => "1w",
            Self::Mo1 => "1M",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "1m" => Some(Self::M1),
            "3m" => Some(Self::M3),
            "5m" => Some(Self::M5),
            "15m" => Some(Self::M15),
            "30m" => Some(Self::M30),
            "1h" => Some(Self::H1),
            "2h" => Some(Self::H2),
            "4h" => Some(Self::H4),
            "8h" => Some(Self::H8),
            "12h" => Some(Self::H12),
            "1d" => Some(Self::D1),
            "3d" => Some(Self::D3),
            "1w" => Some(Self::W1),
            "1M" => Some(Self::Mo1),
            _ => None,
        }
    }

    pub fn duration_ms(self) -> i64 {
        match self {
            Self::M1 => 60_000,
            Self::M3 => 180_000,
            Self::M5 => 300_000,
            Self::M15 => 900_000,
            Self::M30 => 1_800_000,
            Self::H1 => 3_600_000,
            Self::H2 => 7_200_000,
            Self::H4 => 14_400_000,
            Self::H8 => 28_800_000,
            Self::H12 => 43_200_000,
            Self::D1 => 86_400_000,
            Self::D3 => 259_200_000,
            Self::W1 => 604_800_000,
            Self::Mo1 => 2_592_000_000,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Candle {
    /// Candle open time (unix seconds).
    pub time: i64,
    pub open: String,
    pub high: String,
    pub low: String,
    pub close: String,
    pub volume: String,
}

pub async fn fetch_live_mid(mode: RunMode, symbol: &str) -> ExchangeResult<Decimal> {
    let _ = mode; // Simulation callers also use the live Polymarket Perps URL for mids.
    let ex = PolymarketExchange::new(RunMode::Mainnet);
    ex.get_mid(symbol).await
}

pub async fn fetch_candles(
    mode: RunMode,
    symbol: &str,
    interval: CandleInterval,
    limit: usize,
) -> ExchangeResult<Vec<Candle>> {
    let _ = mode;
    let mut ex = PolymarketExchange::new(RunMode::Mainnet);
    ex.refresh_instruments().await?;
    let meta = ex.resolve_instrument(symbol)?;
    let bars = limit.clamp(1, 5000);
    let now_ms = PolymarketExchange::now_ms() as i64;
    let start_ms = now_ms.saturating_sub(interval.duration_ms().saturating_mul(bars as i64));
    let url = format!(
        "{}/v1/info/klines?instrument_id={}&interval={}&start_timestamp={}&end_timestamp={}",
        ex.base_url,
        meta.instrument_id,
        interval.as_str(),
        start_ms,
        now_ms
    );
    let raw = ex.get_public(&url).await?;
    let arr = raw
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| ExchangeError::Api("klines data not array".into()))?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let row = match item.as_array() {
            Some(r) if r.len() >= 6 => r,
            _ => continue,
        };
        let t_ms = row[0]
            .as_i64()
            .or_else(|| row[0].as_u64().map(|u| u as i64))
            .unwrap_or(0);
        if t_ms <= 0 {
            continue;
        }
        let num = |v: &Value| -> String {
            if let Some(s) = v.as_str() {
                s.to_string()
            } else if let Some(n) = v.as_f64() {
                n.to_string()
            } else {
                v.to_string()
            }
        };
        out.push(Candle {
            time: t_ms / 1000,
            open: num(&row[1]),
            high: num(&row[2]),
            low: num(&row[3]),
            close: num(&row[4]),
            volume: num(&row[5]),
        });
    }
    out.sort_by_key(|c| c.time);
    out.dedup_by_key(|c| c.time);
    if out.len() > bars {
        let skip = out.len() - bars;
        out = out.split_off(skip);
    }
    Ok(out)
}

pub async fn list_live_mids(mode: RunMode) -> ExchangeResult<HashMap<String, Decimal>> {
    let _ = mode;
    let ex = PolymarketExchange::new(RunMode::Mainnet);
    let url = format!("{}/v1/info/tickers", ex.base_url);
    let tickers = ex.get_public(&url).await?;
    let mut out = HashMap::new();
    if let Some(arr) = tickers.as_array() {
        for t in arr {
            let full = t.get("symbol").and_then(|s| s.as_str()).unwrap_or("");
            if full.is_empty() {
                continue;
            }
            let mid = json_decimal(t.get("mid_price"))
                .or_else(|| json_decimal(t.get("mark_price")))
                .filter(|m| *m > Decimal::ZERO);
            let Some(mid) = mid else { continue };
            let base = base_from_full(full);
            out.insert(base, mid);
            out.insert(full.to_string(), mid);
        }
    }
    Ok(out)
}

struct MarketsCacheEntry {
    fetched_at: std::time::Instant,
    markets: Vec<MarketInfo>,
}

fn markets_cache() -> &'static std::sync::Mutex<Option<MarketsCacheEntry>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<MarketsCacheEntry>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

const MARKETS_TTL: Duration = Duration::from_secs(90);

pub async fn list_live_markets(mode: RunMode) -> ExchangeResult<Vec<MarketInfo>> {
    let _ = mode;
    if let Ok(guard) = markets_cache().lock() {
        if let Some(entry) = guard.as_ref() {
            if entry.fetched_at.elapsed() <= MARKETS_TTL {
                return Ok(entry.markets.clone());
            }
        }
    }

    let ex = PolymarketExchange::new(RunMode::Mainnet);
    let instruments = ex
        .get_public(&format!("{}/v1/info/instruments", ex.base_url))
        .await?;
    let tickers = ex
        .get_public(&format!("{}/v1/info/tickers", ex.base_url))
        .await
        .unwrap_or(json!([]));
    let stats = ex
        .get_public(&format!("{}/v1/info/statistics", ex.base_url))
        .await
        .unwrap_or(json!([]));

    let mut mid_by_id: HashMap<u64, Decimal> = HashMap::new();
    let mut funding_by_id: HashMap<u64, Decimal> = HashMap::new();
    if let Some(arr) = tickers.as_array() {
        for t in arr {
            let iid = t.get("instrument_id").and_then(|v| v.as_u64()).unwrap_or(0);
            if let Some(mid) = json_decimal(t.get("mid_price"))
                .or_else(|| json_decimal(t.get("mark_price")))
                .filter(|m| *m > Decimal::ZERO)
            {
                mid_by_id.insert(iid, mid);
            }
            if let Some(fr) = json_decimal(t.get("funding_rate")) {
                funding_by_id.insert(iid, fr);
            }
        }
    }
    let mut vol_by_id: HashMap<u64, Decimal> = HashMap::new();
    let mut open_by_id: HashMap<u64, Decimal> = HashMap::new();
    if let Some(arr) = stats.as_array() {
        for s in arr {
            let iid = s.get("instrument_id").and_then(|v| v.as_u64()).unwrap_or(0);
            if let Some(v) = json_decimal(s.get("volume")) {
                vol_by_id.insert(iid, v);
            }
            if let Some(v) = json_decimal(s.get("open_price")) {
                open_by_id.insert(iid, v);
            }
        }
    }

    let mut markets = Vec::new();
    if let Some(arr) = instruments.as_array() {
        for item in arr {
            let iid = item
                .get("instrument_id")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let full = item.get("symbol").and_then(|s| s.as_str()).unwrap_or("");
            if iid == 0 || full.is_empty() {
                continue;
            }
            let base = item
                .get("base_asset")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| base_from_full(full));
            let Some(mid) = mid_by_id.get(&iid).copied() else {
                continue;
            };
            markets.push(MarketInfo {
                symbol: base,
                label: format!("{full} (perp)"),
                kind: "perp".into(),
                mid,
                funding_rate: funding_by_id.get(&iid).copied(),
                day_ntl_vlm: vol_by_id.get(&iid).copied(),
                prev_day_px: open_by_id.get(&iid).copied(),
                min_leverage: 1,
                max_leverage: item
                    .get("max_leverage")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(20) as u32,
                only_isolated: item
                    .get("isolated_only")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            });
        }
    }
    markets.sort_by(|a, b| {
        b.day_ntl_vlm
            .unwrap_or(Decimal::ZERO)
            .cmp(&a.day_ntl_vlm.unwrap_or(Decimal::ZERO))
    });

    if let Ok(mut guard) = markets_cache().lock() {
        *guard = Some(MarketsCacheEntry {
            fetched_at: std::time::Instant::now(),
            markets: markets.clone(),
        });
    }
    Ok(markets)
}

/// Result of `GET https://polymarket.com/api/geoblock` (official geo check).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeoblockStatus {
    pub blocked: bool,
    pub ip: String,
    pub country: String,
    pub region: String,
}

/// Check whether the current public IP is blocked from placing Polymarket orders.
/// See <https://docs.polymarket.com/api-reference/geoblock>.
pub async fn check_geoblock() -> ExchangeResult<GeoblockStatus> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| ExchangeError::Api(e.to_string()))?;
    let res = client
        .get("https://polymarket.com/api/geoblock")
        .send()
        .await
        .map_err(|e| ExchangeError::Api(e.to_string()))?;
    let status = res.status();
    let text = res
        .text()
        .await
        .map_err(|e| ExchangeError::Api(e.to_string()))?;
    if !status.is_success() {
        return Err(ExchangeError::Api(friendly_http_error(status, &text)));
    }
    let v: Value =
        serde_json::from_str(&text).map_err(|e| ExchangeError::Api(format!("{e}: {text}")))?;
    Ok(GeoblockStatus {
        blocked: v
            .get("blocked")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        ip: v
            .get("ip")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        country: v
            .get("country")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        region: v
            .get("region")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// Polymarket rejects prices/qty with more than 5 significant figures, and also
/// enforces per-instrument decimal places. Apply sig-fig rounding first, then decimals.
fn round_with_sig_figs(value: Decimal, decimals: u32, sig_figs: u32) -> Decimal {
    if value == Decimal::ZERO {
        return value;
    }
    let sig = round_to_sig_figs(value, sig_figs);
    sig.round_dp(decimals)
}

fn round_to_sig_figs(value: Decimal, sig_figs: u32) -> Decimal {
    if value == Decimal::ZERO || sig_figs == 0 {
        return value;
    }
    let abs = value.abs();
    let f = abs.to_string().parse::<f64>().unwrap_or(0.0);
    if f <= 0.0 {
        return value;
    }
    let exp = f.log10().floor() as i32;
    let scale = (sig_figs as i32 - 1) - exp;
    if scale >= 0 {
        let factor = Decimal::from(10u64.pow(scale as u32));
        (value * factor).round() / factor
    } else {
        let factor = Decimal::from(10u64.pow((-scale) as u32));
        (value / factor).round() * factor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_symbol_strips_usd() {
        assert_eq!(base_from_full("BTC-USD"), "BTC");
        assert_eq!(base_from_full("BTC"), "BTC");
    }

    #[test]
    fn domain_separator_stable() {
        let a = polymarket_domain_separator();
        let b = polymarket_domain_separator();
        assert_eq!(a, b);
    }

    #[test]
    fn price_respects_five_sig_figs_and_decimals() {
        // BTC-like: 1 price decimal; 65561.5 has 6 sig figs → 65562
        assert_eq!(round_with_sig_figs(dec!(65561.5), 1, 5), dec!(65562));
        // ETH-like: 2 price decimals; 1927.15 → 1927.2
        assert_eq!(round_with_sig_figs(dec!(1927.15), 2, 5), dec!(1927.2));
        // Already valid
        assert_eq!(round_with_sig_figs(dec!(1927.2), 2, 5), dec!(1927.2));
        // High price with 1 decimal that would keep 6 sig figs after dp round
        assert_eq!(round_with_sig_figs(dec!(97000.55), 1, 5), dec!(97001));
    }

    #[test]
    fn qty_respects_five_sig_figs() {
        assert_eq!(round_with_sig_figs(dec!(1.23456), 5, 5), dec!(1.2346));
        assert_eq!(round_with_sig_figs(dec!(0.0123456), 5, 5), dec!(0.01235));
    }

    #[test]
    fn wire_strips_trailing_zeros() {
        assert_eq!(
            PolymarketExchange::decimal_to_wire(dec!(97001.0)),
            "97001"
        );
        assert_eq!(
            PolymarketExchange::decimal_to_wire(dec!(1927.20)),
            "1927.2"
        );
    }
}
