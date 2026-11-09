use alloy::primitives::{utils::parse_ether, Address, U256};

/// Converts asset USD price and ETH USD price to asset/ETH ratio in 1e18 wei.
/// Returns None if inputs are non-finite or <= 0, or if parsing fails.
pub fn to_eth_wei(asset_usd: f64, eth_usd: f64) -> Option<U256> {
    if !asset_usd.is_finite() || asset_usd <= 0.0 || !eth_usd.is_finite() || eth_usd <= 0.0 {
        return None;
    }
    let ratio = asset_usd / eth_usd;
    if !ratio.is_finite() || ratio <= 0.0 {
        return None;
    }
    parse_ether(&format!("{:.18}", ratio)).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnChain {
    pub enabled: bool,
    pub price: U256,
    pub updated_at: u64,
}

/// Checks if an asset price push is due based on on-chain status, deviation, and heartbeat.
pub fn is_due(
    on: &OnChain,
    new_price: U256,
    now_secs: u64,
    deviation_bps: u64,
    heartbeat_secs: u64,
) -> bool {
    if on.price == U256::ZERO {
        return true;
    }
    let diff = if new_price > on.price {
        new_price - on.price
    } else {
        on.price - new_price
    };
    if diff * U256::from(10_000) >= on.price * U256::from(deviation_bps) {
        return true;
    }
    if now_secs.saturating_sub(on.updated_at) >= heartbeat_secs {
        return true;
    }
    false
}

/// Selects assets due for pushing.
/// Skips assets if disabled, stale (price is None), or if ETH price is missing/stale.
pub fn select_pushes(
    assets: &[(Address, Option<f64>, OnChain)],
    eth_usd: Option<f64>,
    now_secs: u64,
    deviation_bps: u64,
    heartbeat_secs: u64,
) -> Vec<(Address, U256)> {
    let eth = match eth_usd {
        Some(e) => e,
        None => return Vec::new(),
    };
    let mut due = Vec::new();
    for (token, price_opt, on) in assets {
        if !on.enabled {
            continue;
        }
        let price = match price_opt {
            Some(p) => *p,
            None => continue,
        };
        let new_wei = match to_eth_wei(price, eth) {
            Some(w) => w,
            None => continue,
        };
        if is_due(on, new_wei, now_secs, deviation_bps, heartbeat_secs) {
            due.push((*token, new_wei));
        }
    }
    due
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::str::FromStr;

    #[test]
    fn to_eth_wei_converts() {
        // (100_000.0, 2_500.0) -> 40e18
        let wbtc = to_eth_wei(100_000.0, 2_500.0).expect("conversion succeeds");
        assert_eq!(wbtc, U256::from(40) * U256::from(10).pow(U256::from(18)));

        // (1.0, 2_500.0) -> 4e14
        let usdt = to_eth_wei(1.0, 2_500.0).expect("conversion succeeds");
        assert_eq!(usdt, U256::from(400_000_000_000_000u64));
    }

    #[test]
    fn to_eth_wei_rejects_bad_inputs() {
        assert_eq!(to_eth_wei(100.0, 0.0), None);
        assert_eq!(to_eth_wei(100.0, -1.0), None);
        assert_eq!(to_eth_wei(100.0, f64::NAN), None);
        assert_eq!(to_eth_wei(100.0, f64::INFINITY), None);
        assert_eq!(to_eth_wei(0.0, 2500.0), None);
        assert_eq!(to_eth_wei(-10.0, 2500.0), None);
        assert_eq!(to_eth_wei(f64::NAN, 2500.0), None);
        assert_eq!(to_eth_wei(f64::INFINITY, 2500.0), None);
    }

    #[test]
    fn is_due_on_deviation() {
        let one_ether = U256::from(10).pow(U256::from(18));
        let on = OnChain {
            enabled: true,
            price: one_ether,
            updated_at: 1000,
        };
        // 50 bps = 0.5%
        // new = 1.005e18 -> diff = 0.005e18 -> exactly 50 bps -> true
        let price_50bps = one_ether + (one_ether * U256::from(50) / U256::from(10_000));
        assert!(is_due(&on, price_50bps, 1010, 50, 300));

        // new = 1.004e18 -> 40 bps -> false
        let price_40bps = one_ether + (one_ether * U256::from(40) / U256::from(10_000));
        assert!(!is_due(&on, price_40bps, 1010, 50, 300));
    }

    #[test]
    fn is_due_on_heartbeat() {
        let one_ether = U256::from(10).pow(U256::from(18));
        let on = OnChain {
            enabled: true,
            price: one_ether,
            updated_at: 1000,
        };
        // same price, now - updated_at == 300 -> true
        assert!(is_due(&on, one_ether, 1300, 50, 300));
        // same price, now - updated_at == 299 -> false
        assert!(!is_due(&on, one_ether, 1299, 50, 300));
    }

    #[test]
    fn is_due_when_never_pushed() {
        let on = OnChain {
            enabled: true,
            price: U256::ZERO,
            updated_at: 0,
        };
        assert!(is_due(&on, U256::from(1000), 100, 50, 300));
    }

    #[test]
    fn select_pushes_skips_all_when_eth_stale() {
        let addr = Address::from_str("0x29f2D40B0605204364af54EC677bD022dA425d03").unwrap();
        let on = OnChain {
            enabled: true,
            price: U256::ZERO,
            updated_at: 0,
        };
        let assets = vec![(addr, Some(100_000.0), on)];
        let res = select_pushes(&assets, None, 100, 50, 300);
        assert!(res.is_empty());
    }

    #[test]
    fn select_pushes_skips_disabled_assets() {
        let addr = Address::from_str("0x29f2D40B0605204364af54EC677bD022dA425d03").unwrap();
        let on = OnChain {
            enabled: false,
            price: U256::ZERO,
            updated_at: 0,
        };
        let assets = vec![(addr, Some(100_000.0), on)];
        let res = select_pushes(&assets, Some(2500.0), 100, 50, 300);
        assert!(res.is_empty());
    }

    #[test]
    fn select_pushes_skips_stale_asset() {
        let addr1 = Address::from_str("0x29f2D40B0605204364af54EC677bD022dA425d03").unwrap();
        let addr2 = Address::from_str("0xf8Fb3713D459D7C1018BD0A49D19b4C44290EBE5").unwrap();
        let on1 = OnChain {
            enabled: true,
            price: U256::ZERO,
            updated_at: 0,
        };
        let on2 = OnChain {
            enabled: true,
            price: U256::ZERO,
            updated_at: 0,
        };
        let assets = vec![(addr1, None, on1), (addr2, Some(20.0), on2)];
        let res = select_pushes(&assets, Some(2500.0), 100, 50, 300);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].0, addr2);
    }

    #[test]
    fn pusher_config_validates() {
        // Good section
        let valid_toml = r#"
        [[assets]]
        symbol = "ETH/USD"
        source = "fixed"
        id = "2500"

        [[assets]]
        symbol = "BTC/USD"
        source = "fixed"
        id = "100000"

        [pusher]
        contract = "0x0000000000000000000000000000000000000001"
        eth_symbol = "ETH/USD"
        tick_secs = 10
        deviation_bps = 50
        heartbeat_secs = 300
        low_balance_wei = "50000000000000000"
        assets = [
          { symbol = "BTC/USD", token = "0x29f2D40B0605204364af54EC677bD022dA425d03" }
        ]
        "#;
        assert!(Config::parse(valid_toml).is_ok());

        // Bad contract address
        let bad_contract = valid_toml.replace(
            "contract = \"0x0000000000000000000000000000000000000001\"",
            "contract = \"0xinvalid\"",
        );
        assert!(Config::parse(&bad_contract).is_err());

        // Unknown eth symbol
        let bad_eth_symbol = valid_toml.replace("eth_symbol = \"ETH/USD\"", "eth_symbol = \"UNKNOWN/USD\"");
        assert!(Config::parse(&bad_eth_symbol).is_err());

        // Unknown asset symbol
        let bad_asset_symbol = valid_toml.replace("symbol = \"BTC/USD\", token", "symbol = \"UNKNOWN/USD\", token");
        assert!(Config::parse(&bad_asset_symbol).is_err());

        // Heartbeat 0
        let bad_heartbeat = valid_toml.replace("heartbeat_secs = 300", "heartbeat_secs = 0");
        assert!(Config::parse(&bad_heartbeat).is_err());

        // Deviation > 10000
        let bad_deviation = valid_toml.replace("deviation_bps = 50", "deviation_bps = 10001");
        assert!(Config::parse(&bad_deviation).is_err());
    }
}
