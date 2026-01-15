# Aggregator Integration Guide

This document provides a complete, step-by-step guide for integrating new DEX aggregators into the `solana-dex-superagg` library.

## Overview

The library currently supports Jupiter (HTTP-based) and Titan (WebSocket-based). This guide will help you add additional aggregators following the same architectural patterns.

## Architecture Context

### DexAggregator Trait
All aggregators implement the `DexAggregator` async trait with two core methods:
- `swap()` - Execute a swap and return transaction signature + output amount
- `simulate()` - Quote a swap without executing (for price comparison)

### Key Components
1. **Aggregator Implementation** - Your custom aggregator struct that implements `DexAggregator`
2. **Configuration** - Aggregator-specific settings (API URLs, keys, etc.)
3. **Client Integration** - Wiring your aggregator into the routing system
4. **Validation** - Testing connectivity and configuration

---

## Step-by-Step Integration Guide

### Step 1: Create Aggregator Implementation

**File:** `src/aggregators/{your_aggregator}/mod.rs` (create new directory and file)

**1.1 Define the Aggregator Struct**
```rust
use crate::aggregators::{DexAggregator, QuoteMetadata, SimulateResult, SwapResult};
use crate::config::{Aggregator, ClientConfig};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentLevel,
    signature::{Keypair, Signer},
};
use std::sync::Arc;
use std::time::Instant;

pub struct YourAggregator {
    // Your API client (HTTP or WebSocket)
    client: YourApiClient,

    // Always needed for Solana RPC calls
    rpc_client: RpcClient,

    // Wallet for signing transactions
    signer: Arc<Keypair>,

    // Additional fields (compute unit price, fees, etc.)
    compute_unit_price_micro_lamports: u64,
}
```

**1.2 Add Initialization Methods**
```rust
impl YourAggregator {
    /// Create from ClientConfig (standard initialization)
    pub async fn new(config: &ClientConfig, signer: Arc<Keypair>) -> Result<Self> {
        let your_config = config
            .your_aggregator
            .as_ref()
            .ok_or_else(|| anyhow!("YourAggregator not configured"))?;

        let rpc_client = RpcClient::new_with_commitment(
            &config.shared.rpc_url,
            solana_sdk::commitment_config::CommitmentConfig::confirmed(),
        );

        // Initialize your API client
        let client = YourApiClient::new(&your_config.api_url, your_config.api_key.clone())?;

        // If WebSocket, call connect() here
        // client.connect().await?;

        Ok(Self {
            client,
            rpc_client,
            signer,
            compute_unit_price_micro_lamports: config.shared.compute_unit_price_micro_lamports,
        })
    }

    /// Optional: Create with custom compute unit price
    pub fn new_with_compute_price(
        config: &ClientConfig,
        signer: Arc<Keypair>,
        compute_unit_price_micro_lamports: u64,
    ) -> Result<Self> {
        // Similar to new() but override compute unit price
    }

    /// Optional: Cleanup method if using persistent connections (like WebSocket)
    pub async fn close(&self) -> Result<()> {
        // Close connections if needed
        // self.client.close().await?;
        Ok(())
    }
}
```

**1.3 Implement DexAggregator Trait**
```rust
#[async_trait]
impl DexAggregator for YourAggregator {
    async fn swap(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
        commitment_level: CommitmentLevel,
    ) -> Result<SwapResult> {
        let start = Instant::now();

        // 1. Validate inputs
        let input_mint = solana_sdk::pubkey::Pubkey::from_str(input)
            .map_err(|e| anyhow!("Invalid input mint: {}", e))?;
        let output_mint = solana_sdk::pubkey::Pubkey::from_str(output)
            .map_err(|e| anyhow!("Invalid output mint: {}", e))?;

        if input_mint == output_mint {
            return Err(anyhow!("Input and output mints must be different"));
        }

        // 2. Get quote from your API
        let quote = self.client.get_quote(input, output, amount, slippage_bps).await?;

        // 3. Build transaction (method varies by aggregator)
        let transaction = self.client.build_swap_transaction(quote, self.signer.pubkey()).await?;

        // 4. Sign transaction
        let signed_tx = self.sign_transaction(transaction)?;

        // 5. Submit and confirm
        let sig = self.rpc_client.send_and_confirm_transaction_with_spinner_and_config(
            &signed_tx,
            solana_sdk::commitment_config::CommitmentConfig { commitment: commitment_level },
            solana_client::rpc_config::RpcSendTransactionConfig {
                skip_preflight: false,
                preflight_commitment: Some(CommitmentLevel::Processed),
                ..Default::default()
            },
        )?;

        let execution_time = start.elapsed();

        Ok(SwapResult {
            signature: sig.to_string(),
            out_amount: quote.out_amount,
            slippage_bps_used: Some(slippage_bps),
            aggregator_used: Some(Aggregator::YourAggregator),
            execution_time: Some(execution_time),
        })
    }

    async fn simulate(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<SimulateResult> {
        let start = Instant::now();

        // Get quote (same as swap, but don't execute)
        let quote = self.client.get_quote(input, output, amount, slippage_bps).await?;

        let sim_time = start.elapsed();

        Ok(SimulateResult {
            out_amount: quote.out_amount,
            price_impact: quote.price_impact_pct,
            metadata: QuoteMetadata {
                route: quote.route_info,
                fees: quote.fees,
                extra: None,
            },
            sim_time: Some(sim_time),
        })
    }
}
```

---

### Step 2: Add Configuration

**File:** `src/config.rs`

**2.1 Add to Aggregator Enum (lines 12-19)**
```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Aggregator {
    Jupiter,
    Titan,
    YourAggregator,  // ADD THIS
}
```

**2.2 Create Config Struct (after line 255)**
```rust
/// YourAggregator-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct YourAggregatorConfig {
    /// API endpoint URL
    pub api_url: String,
    /// Optional API key
    pub api_key: Option<String>,
    // Add other aggregator-specific settings
}

impl Default for YourAggregatorConfig {
    fn default() -> Self {
        Self {
            api_url: "https://api.youraggregator.com".to_string(),
            api_key: None,
        }
    }
}
```

**2.3 Update ClientConfig (lines 305-325)**
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    #[serde(default)]
    pub shared: SharedConfig,
    #[serde(default)]
    pub jupiter: JupiterConfig,
    #[serde(default)]
    pub titan: Option<TitanConfig>,
    #[serde(default)]
    pub your_aggregator: Option<YourAggregatorConfig>,  // ADD THIS
}
```

**2.4 Update Default Implementation (lines 317-325)**
```rust
impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            shared: SharedConfig::default(),
            jupiter: JupiterConfig::default(),
            titan: None,
            your_aggregator: None,  // ADD THIS
        }
    }
}
```

**2.5 Add Helper Method (after line 356)**
```rust
/// Check if YourAggregator is configured
pub fn is_your_aggregator_configured(&self) -> bool {
    self.your_aggregator
        .as_ref()
        .map(|y| !y.api_url.is_empty())
        .unwrap_or(false)
}
```

**2.6 Add Validation Method (after line 534)**
```rust
/// Validate YourAggregator configuration and connectivity
async fn validate_your_aggregator(&self) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();

    let your_agg = match &self.your_aggregator {
        Some(y) => y,
        None => {
            errors.push("YourAggregator is not configured".to_string());
            return Err(errors);
        }
    };

    if your_agg.api_url.is_empty() {
        errors.push("YourAggregator API URL is required".to_string());
        return Err(errors);
    }

    // Test connectivity (implement validate_your_aggregator_endpoint)
    if let Err(e) = self.validate_your_aggregator_endpoint(your_agg).await {
        errors.push(format!(
            "YourAggregator API validation failed ({}): {}",
            your_agg.api_url, e
        ));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

async fn validate_your_aggregator_endpoint(&self, config: &YourAggregatorConfig) -> anyhow::Result<()> {
    use tokio::time::timeout;
    use std::time::Duration;

    // Test API connectivity with a simple HTTP request or WebSocket connection
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    timeout(
        Duration::from_secs(5),
        client.get(&config.api_url).send(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("API request timeout"))?
    .map_err(|e| anyhow::anyhow!("Failed to connect: {}", e))?;

    Ok(())
}
```

**2.7 Update validate() Method (lines 392-449)**

Add to BestPrice strategy section (around line 395-406):
```rust
RoutingStrategy::BestPrice => {
    if let Err(errs) = self.validate_jupiter().await {
        errors.extend(errs);
    }
    if self.titan.is_some() {
        if let Err(errs) = self.validate_titan().await {
            errors.extend(errs);
        }
    }
    // ADD THIS:
    if self.your_aggregator.is_some() {
        if let Err(errs) = self.validate_your_aggregator().await {
            errors.extend(errs);
        }
    }
}
```

Add to PreferredAggregator match (around line 407-418):
```rust
RoutingStrategy::PreferredAggregator { aggregator, .. } => match aggregator {
    Aggregator::Jupiter => { /* existing */ }
    Aggregator::Titan => { /* existing */ }
    // ADD THIS:
    Aggregator::YourAggregator => {
        if let Err(errs) = self.validate_your_aggregator().await {
            errors.extend(errs);
        }
    }
}
```

---

### Step 3: Export Aggregator Module

**File:** `src/aggregators/mod.rs` (lines 108-112)

```rust
pub mod jupiter;
pub mod titan;
pub mod your_aggregator;  // ADD THIS

pub use jupiter::JupiterAggregator;
pub use titan::TitanAggregator;
pub use your_aggregator::YourAggregator;  // ADD THIS
```

---

### Step 4: Integrate into DexSuperAggClient

**File:** `src/client.rs`

**4.1 Add Cached Aggregator Field (if persistent connection needed, after line 31)**
```rust
pub struct DexSuperAggClient {
    signer: Arc<Keypair>,
    rpc_client: RpcClient,
    config: ClientConfig,
    titan_aggregator: Arc<Mutex<Option<Arc<TitanAggregator>>>>,
    your_aggregator: Arc<Mutex<Option<Arc<YourAggregator>>>>,  // ADD THIS if needed
}
```

**4.2 Initialize in new() (update line 49)**
```rust
Ok(Self {
    signer: Arc::new(signer),
    rpc_client,
    config,
    titan_aggregator: Arc::new(Mutex::new(None)),
    your_aggregator: Arc::new(Mutex::new(None)),  // ADD THIS if needed
})
```

**4.3 Add Getter Method (after line 92, if persistent connection needed)**
```rust
async fn get_your_aggregator(&self) -> Result<Arc<YourAggregator>> {
    let mut your_agg_opt = self.your_aggregator.lock().await;

    if your_agg_opt.is_none() {
        let your_agg = YourAggregator::new(&self.config, Arc::clone(&self.signer)).await?;
        *your_agg_opt = Some(Arc::new(your_agg));
    }

    Ok(your_agg_opt.as_ref().unwrap().clone())
}
```

**4.4 Update close() Method (lines 96-102)**
```rust
pub async fn close(&self) -> Result<()> {
    let mut titan_opt = self.titan_aggregator.lock().await;
    if let Some(titan) = titan_opt.take() {
        titan.close().await?;
    }
    // ADD THIS:
    let mut your_agg_opt = self.your_aggregator.lock().await;
    if let Some(your_agg) = your_agg_opt.take() {
        your_agg.close().await?;
    }
    Ok(())
}
```

**4.5 Update swap_direct() (lines 314-361)**
```rust
async fn swap_direct(
    &self,
    input: &str,
    output: &str,
    amount: u64,
    slippage_bps: u16,
    aggregator: &Aggregator,
    route_config: &RouteConfig,
) -> Result<SwapResult> {
    let mut result = match aggregator {
        Aggregator::Jupiter => { /* existing */ },
        Aggregator::Titan => { /* existing */ },
        // ADD THIS:
        Aggregator::YourAggregator => {
            let your_agg = YourAggregator::new_with_compute_price(
                &self.config,
                Arc::clone(&self.signer),
                route_config.compute_unit_price_micro_lamports,
            )?;
            // Or if using cached connection:
            // let your_agg = self.get_your_aggregator().await?;
            your_agg
                .swap(
                    input,
                    output,
                    amount,
                    slippage_bps,
                    route_config.commitment_level,
                )
                .await?
        }
    };

    // Ensure aggregator_used is set
    if result.aggregator_used.is_none() {
        result.aggregator_used = Some(*aggregator);
    }

    Ok(result)
}
```

**4.6 Update swap_with_simulation() (lines 364-408)**
```rust
async fn swap_with_simulation(
    &self,
    input: &str,
    output: &str,
    amount: u64,
    slippage_bps: u16,
    aggregator: &Aggregator,
    route_config: &RouteConfig,
) -> Result<SwapSummary> {
    let sim_result = match aggregator {
        Aggregator::Jupiter => { /* existing */ },
        Aggregator::Titan => { /* existing */ },
        // ADD THIS:
        Aggregator::YourAggregator => {
            let your_agg = YourAggregator::new_with_compute_price(
                &self.config,
                Arc::clone(&self.signer),
                route_config.compute_unit_price_micro_lamports,
            )?;
            your_agg.simulate(input, output, amount, slippage_bps).await?
        }
    };

    // Then execute swap
    let swap_result = self.swap_direct(input, output, amount, slippage_bps, aggregator, route_config).await?;

    Ok(SwapSummary {
        swap_result,
        sim_results: vec![(*aggregator, sim_result)],
    })
}
```

**4.7 Update swap_best_price() (lines 525-607)**

Add simulation logic (around lines 549-557):
```rust
// YourAggregator simulation (if configured)
if self.config.is_your_aggregator_configured() {
    if let Ok(your_agg) = YourAggregator::new_with_compute_price(
        &self.config,
        Arc::clone(&self.signer),
        route_config.compute_unit_price_micro_lamports,
    ) {
        if let Ok(sim_result) = your_agg.simulate(input, output, amount, slippage_bps).await {
            sim_results.push((Aggregator::YourAggregator, sim_result.clone()));
            comparison_results.push((Aggregator::YourAggregator, sim_result.out_amount));
        }
    }
}
```

**4.8 Update Logging (lines 467-471, 571-574, 581-584)**
```rust
let agg_name = match agg {
    Aggregator::Jupiter => "Jupiter",
    Aggregator::Titan => "Titan",
    Aggregator::YourAggregator => "YourAggregator",  // ADD THIS
};
```

---

### Step 5: Environment Variables

Users will configure your aggregator via environment variables:
```bash
DEX_SUPERAGG_YOUR_AGGREGATOR__API_URL='https://api.youraggregator.com'
DEX_SUPERAGG_YOUR_AGGREGATOR__API_KEY='your_api_key'
```

Pattern: `DEX_SUPERAGG_{SECTION}__{FIELD}` where `{SECTION}` is uppercase snake_case

---

### Step 6: Add Dependencies (if needed)

**File:** `Cargo.toml`

```toml
[dependencies]
your-aggregator-client = "1.0"
# or from git:
your-aggregator-client = { git = "https://github.com/...", rev = "..." }
```

---

### Step 7: Testing

**Create tests in your aggregator module:**
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_your_aggregator_initialization() {
        // Test config loading and initialization
    }

    #[tokio::test]
    #[ignore]  // Requires API credentials
    async fn test_swap() {
        // Test actual swap execution
    }

    #[tokio::test]
    #[ignore]
    async fn test_simulate() {
        // Test quote/simulation
    }
}
```

---

## Reference: Two Implementation Patterns

### Pattern 1: HTTP-Based (like Jupiter)
- Simple HTTP client
- No persistent connection
- Create new instance per swap or reuse across calls
- Simpler initialization

### Pattern 2: WebSocket-Based (like Titan)
- Persistent WebSocket connection
- Requires async initialization
- Must implement close() for cleanup
- Cache and reuse connection via `Arc<Mutex<Option<Arc<YourAgg>>>>`
- More complex but efficient for multiple swaps

Choose the pattern that matches your aggregator's API design.

---

## Complete Integration Checklist

- [ ] Step 1: Create aggregator implementation in `src/aggregators/your_aggregator/mod.rs`
- [ ] Step 2.1: Add enum variant to `Aggregator` in `src/config.rs`
- [ ] Step 2.2: Create `YourAggregatorConfig` struct in `src/config.rs`
- [ ] Step 2.3: Add field to `ClientConfig` in `src/config.rs`
- [ ] Step 2.4: Update `ClientConfig::default()`
- [ ] Step 2.5: Add `is_your_aggregator_configured()` helper
- [ ] Step 2.6: Add `validate_your_aggregator()` and endpoint validation
- [ ] Step 2.7: Update `validate()` method for routing strategies
- [ ] Step 3: Export module in `src/aggregators/mod.rs`
- [ ] Step 4.1: Add cached aggregator field (if needed)
- [ ] Step 4.2: Initialize in `DexSuperAggClient::new()`
- [ ] Step 4.3: Add getter method (if persistent connection)
- [ ] Step 4.4: Update `close()` method
- [ ] Step 4.5: Update `swap_direct()` with match arm
- [ ] Step 4.6: Update `swap_with_simulation()` with match arm
- [ ] Step 4.7: Update `swap_best_price()` to include in comparison
- [ ] Step 4.8: Update all logging statements with aggregator name
- [ ] Step 5: Document environment variables in README
- [ ] Step 6: Add dependencies to `Cargo.toml`
- [ ] Step 7: Write tests for your aggregator
- [ ] Update `.env` file with test credentials
- [ ] Update `examples/basic_swap.rs` to demonstrate your aggregator
- [ ] Update `CLAUDE.md` to mention new aggregator

---

## Verification

After integration, verify with:

1. **Build**: `cargo build`
2. **Tests**: `cargo test`
3. **Example with your aggregator**:
   ```bash
   source .env
   cargo run --example basic_swap
   ```
4. **Test all routing strategies**:
   - BestPrice (should compare your aggregator too)
   - PreferredAggregator with `YourAggregator`
   - LowestSlippageClimber (includes your aggregator)

---

## Files Modified Summary

Total: **3 main files + 1 new file**

1. **NEW**: `src/aggregators/your_aggregator/mod.rs` - Your implementation
2. `src/config.rs` - Configuration (~8 locations)
3. `src/client.rs` - Client integration (~6 locations)
4. `src/aggregators/mod.rs` - Exports (~2 lines)

Plus optional: `Cargo.toml`, `.env`, `README.md`, `CLAUDE.md`, tests, examples

---

## Example Aggregators to Consider

Popular Solana DEX aggregators you might want to integrate:
- **Raydium** - One of the largest AMMs on Solana
- **Orca** - Popular concentrated liquidity protocol
- **Phoenix** - Order book-based DEX
- **Lifinity** - Proactive market maker
- **Meteora** - Dynamic liquidity marketplace
- **Drift** - Perpetuals and spot trading

---

This guide provides a complete blueprint for integrating any new DEX aggregator into the solana-dex-superagg library.
