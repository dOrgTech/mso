use std::str::FromStr;
use std::time::Duration;
use tracing::{error, info, warn};

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use reqwest::Url;

use super::logic::{select_pushes, OnChain};
use crate::config::Pusher;
use crate::feed::{now_ms, FeedCoordinator};

sol! {
    #[sol(rpc)]
    interface IStagingMockOracle {
        function testModeEnabled(address) external view returns (bool);
        function manualPrices(address) external view returns (uint256);
        function updatedAt(address) external view returns (uint256);
        function setAssetPrices(address[] assets, uint256[] prices) external;
    }
}

pub async fn run(coord: FeedCoordinator, cfg: &'static Pusher) {
    let rpc_url = match std::env::var("MSO_PUSHER_RPC_URL") {
        Ok(u) => u,
        Err(_) => {
            error!("MSO_PUSHER_RPC_URL must be set when [pusher] is configured");
            return;
        }
    };
    let private_key = match std::env::var("MSO_PUSHER_PRIVATE_KEY") {
        Ok(k) => k,
        Err(_) => {
            error!("MSO_PUSHER_PRIVATE_KEY must be set when [pusher] is configured");
            return;
        }
    };

    let signer = match private_key.trim().parse::<PrivateKeySigner>() {
        Ok(s) => s,
        Err(e) => {
            error!("failed to parse MSO_PUSHER_PRIVATE_KEY: {e}");
            return;
        }
    };
    let wallet_addr = signer.address();
    let wallet = EthereumWallet::from(signer);

    let url = match rpc_url.parse::<Url>() {
        Ok(u) => u,
        Err(e) => {
            error!("invalid MSO_PUSHER_RPC_URL '{rpc_url}': {e}");
            return;
        }
    };

    let contract_addr = match Address::from_str(&cfg.contract) {
        Ok(a) => a,
        Err(e) => {
            error!("invalid pusher contract address '{}': {e}", cfg.contract);
            return;
        }
    };

    let low_balance_wei = cfg
        .low_balance_wei
        .trim()
        .parse::<U256>()
        .unwrap_or(U256::ZERO);

    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(url);

    let oracle = IStagingMockOracle::new(contract_addr, &provider);
    info!(
        "Pusher started: wallet={wallet_addr}, contract={contract_addr}, tick={}s, deviation={}bps, heartbeat={}s",
        cfg.tick_secs, cfg.deviation_bps, cfg.heartbeat_secs
    );

    // Initial balance check
    match provider.get_balance(wallet_addr).await {
        Ok(bal) => {
            info!("Pusher wallet initial balance: {bal} wei");
            if bal < low_balance_wei {
                error!("Pusher wallet balance {bal} wei is below threshold {low_balance_wei} wei!");
            }
        }
        Err(e) => warn!("Failed to query initial pusher balance: {e}"),
    }

    let mut interval = tokio::time::interval(Duration::from_secs(cfg.tick_secs));
    let mut last_push_time: Option<u64> = None;

    loop {
        interval.tick().await;

        let now_s = (now_ms() / 1000) as u64;

        // 1. Read on-chain status for each asset in cfg.assets
        let mut onchain_assets: Vec<(Address, Option<f64>, OnChain)> = Vec::new();
        let mut any_enabled = false;

        for asset_cfg in &cfg.assets {
            let token_addr = match Address::from_str(&asset_cfg.token) {
                Ok(a) => a,
                Err(_) => continue,
            };

            let enabled = match oracle.testModeEnabled(token_addr).call().await {
                Ok(res) => res,
                Err(e) => {
                    error!("RPC error calling testModeEnabled for {}: {e}", asset_cfg.symbol);
                    continue;
                }
            };

            if enabled {
                any_enabled = true;
            }

            let price = match oracle.manualPrices(token_addr).call().await {
                Ok(p) => p,
                Err(e) => {
                    error!("RPC error calling manualPrices for {}: {e}", asset_cfg.symbol);
                    continue;
                }
            };

            let updated_at = match oracle.updatedAt(token_addr).call().await {
                Ok(t) => t.to::<u64>(),
                Err(e) => {
                    error!("RPC error calling updatedAt for {}: {e}", asset_cfg.symbol);
                    continue;
                }
            };

            // Read price from MSO feed
            let mso_price = if coord.is_stale(&asset_cfg.symbol, now_ms()) {
                None
            } else {
                coord.l1_cache.get(&asset_cfg.symbol).map(|e| e.price)
            };

            onchain_assets.push((
                token_addr,
                mso_price,
                OnChain {
                    enabled,
                    price,
                    updated_at,
                },
            ));
        }

        // Check heartbeat alert if some asset is enabled but no push for longer than heartbeat_secs
        if any_enabled {
            if let Some(last_push) = last_push_time {
                if now_s.saturating_sub(last_push) > cfg.heartbeat_secs {
                    error!(
                        "Pusher heartbeat alert: no successful push for {}s (> heartbeat {}s) with active assets!",
                        now_s.saturating_sub(last_push),
                        cfg.heartbeat_secs
                    );
                }
            }
        }

        // 2. Read ETH price from MSO feed
        let eth_price = if coord.is_stale(&cfg.eth_symbol, now_ms()) {
            None
        } else {
            coord.l1_cache.get(&cfg.eth_symbol).map(|e| e.price)
        };

        // 3. Select pushes
        let pushes = select_pushes(
            &onchain_assets,
            eth_price,
            now_s,
            cfg.deviation_bps,
            cfg.heartbeat_secs,
        );

        if pushes.is_empty() {
            continue;
        }

        let (tokens, prices): (Vec<Address>, Vec<U256>) = pushes.into_iter().unzip();
        info!(
            "Pushing {} asset prices: tokens={:?}, prices={:?}",
            tokens.len(),
            tokens,
            prices
        );

        let call = oracle.setAssetPrices(tokens.clone(), prices.clone());
        let push_future = async {
            let pending_tx = call.send().await?;
            let tx_hash = *pending_tx.tx_hash();
            let receipt = pending_tx.get_receipt().await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>((tx_hash, receipt))
        };

        match tokio::time::timeout(Duration::from_secs(120), push_future).await {
            Ok(Ok((tx_hash, receipt))) => {
                if receipt.status() {
                    info!(
                        "Pushed {} prices successfully: tx_hash={tx_hash:?}",
                        tokens.len()
                    );
                    last_push_time = Some(now_s);
                } else {
                    error!(
                        "Price push transaction reverted! tx_hash={tx_hash:?}, tokens={:?}, prices={:?}",
                        tokens, prices
                    );
                }
            }
            Ok(Err(e)) => {
                error!("Failed to send/confirm setAssetPrices transaction: {e}");
            }
            Err(_) => {
                error!("Timed out after 120s waiting for setAssetPrices receipt!");
            }
        }

        // Check wallet balance after each push attempt
        match provider.get_balance(wallet_addr).await {
            Ok(bal) => {
                if bal < low_balance_wei {
                    error!(
                        "Pusher wallet balance {bal} wei is below threshold {low_balance_wei} wei! Please fund {wallet_addr}"
                    );
                }
            }
            Err(e) => warn!("Failed to query pusher balance: {e}"),
        }
    }
}
