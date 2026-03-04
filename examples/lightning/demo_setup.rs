//! Demo Setup — opens a Lightning channel between customer and provider.
//!
//! This is a one-time operation. Once the channel is open, all subsequent
//! Lightning payments between customer and provider are instant.
//!
//! Run this once before the demo to ensure the customer has outbound liquidity:
//!   cargo run --example demo_setup
//!
//! After the channel is usable (~60 min for ~6 confirmations), run:
//!   ANTHROPIC_API_KEY=sk-... cargo run --example demo_provider
//!   cargo run --example demo_customer

use elisym_core::*;
use std::time::{Duration, Instant};

fn ts() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let total_start = Instant::now();

    println!();
    println!("  ╔═══════════════════════════════════════════════════╗");
    println!("  ║      elisym-core Demo: Channel Setup              ║");
    println!("  ╚═══════════════════════════════════════════════════╝");
    println!();

    // ── Step 1: Start both LDK nodes ──
    let step = Instant::now();
    println!("  [{}] [Step 1/4] Starting Lightning nodes...", ts());

    let mut provider_payments = LdkPaymentProvider::new(LdkPaymentConfig {
        storage_dir: "/tmp/elisym-ldk-provider".to_string(),
        network: ldk_node::bitcoin::Network::Testnet,
        esplora_url: "https://mempool.space/testnet/api".to_string(),
        listening_address: Some("0.0.0.0:9735".to_string()),
    });
    provider_payments.start().await?;

    let mut customer_payments = LdkPaymentProvider::new(LdkPaymentConfig {
        storage_dir: "/tmp/elisym-ldk-customer".to_string(),
        network: ldk_node::bitcoin::Network::Testnet,
        esplora_url: "https://mempool.space/testnet/api".to_string(),
        listening_address: Some("0.0.0.0:9736".to_string()),
    });
    customer_payments.start().await?;

    println!("             Provider node: 0.0.0.0:9735");
    println!("             Customer node: 0.0.0.0:9736");
    println!("             Done in {}", fmt_duration(step.elapsed()));
    println!();

    // ── Step 2: Sync and show balances ──
    let step = Instant::now();
    println!("  [{}] [Step 2/4] Syncing with blockchain (30s)...", ts());
    tokio::time::sleep(Duration::from_secs(30)).await;

    let provider_balance = provider_payments.onchain_balance().unwrap_or(0);
    let customer_balance = customer_payments.onchain_balance().unwrap_or(0);
    let provider_node_id = provider_payments.node_id()?;
    let customer_node_id = customer_payments.node_id()?;

    println!("             Provider: {} sats on-chain", provider_balance);
    println!("             Customer: {} sats on-chain", customer_balance);
    println!("             Provider node ID: {}...{}", &provider_node_id[..16], &provider_node_id[provider_node_id.len()-8..]);
    println!("             Customer node ID: {}...{}", &customer_node_id[..16], &customer_node_id[customer_node_id.len()-8..]);
    println!("             Mempool explorer: https://mempool.space/testnet");
    println!("             Done in {}", fmt_duration(step.elapsed()));
    println!();

    // ── Step 3: Check existing channels or open new one ──
    let step = Instant::now();
    println!("  [{}] [Step 3/4] Checking Lightning channels...", ts());

    let channels = customer_payments.list_channels().unwrap_or_default();
    let usable = channels.iter().filter(|c| c.is_usable).count();

    if usable > 0 {
        let outbound: u64 = channels
            .iter()
            .filter(|c| c.is_usable)
            .map(|c| c.outbound_capacity_msat / 1000)
            .sum();
        let funding_txid = channels
            .first()
            .and_then(|c| c.funding_txo.as_ref())
            .and_then(|txo| txo.split(':').next())
            .map(|s| s.to_string());
        println!("             Already have {} usable channel(s)!", usable);
        println!("             Outbound capacity: {} sats", outbound);
        if let Some(txid) = &funding_txid {
            println!("             Funding tx: https://mempool.space/testnet/tx/{}", txid);
        }
        println!("             Done in {}", fmt_duration(step.elapsed()));
        println!();
        println!("  --- Channel Ready! Run the demo: ---");
        println!("  Terminal 1: cargo run --example demo_provider");
        println!("  Terminal 2: cargo run --example demo_customer");
        println!("  Total time: {}", fmt_duration(total_start.elapsed()));
        println!();

        // Clean shutdown
        tokio::task::spawn_blocking(move || {
            provider_payments.stop();
            customer_payments.stop();
        })
        .await
        .ok();

        return Ok(());
    }

    // Open 30,000 sat channel: customer → provider
    let channel_amount = 30_000u64;

    if !channels.is_empty() {
        println!("             Channel exists but not yet usable, waiting for confirmations...");
    } else {
        if customer_balance < channel_amount + 1_000 {
            let customer_addr = customer_payments.new_onchain_address()?;
            println!("             ERROR: Customer needs at least {} sats on-chain", channel_amount + 1_000);
            println!("             Current balance: {} sats", customer_balance);
            println!();
            println!("             Fund the customer wallet with a testnet faucet:");
            println!("             Address: {}", customer_addr);
            println!();
            println!("             After funding, wait for 1 confirmation and re-run this script.");

            tokio::task::spawn_blocking(move || {
                provider_payments.stop();
                customer_payments.stop();
            })
            .await
            .ok();

            return Err(ElisymError::Payment("Insufficient on-chain balance".into()));
        }

        println!("             Opening {} sat channel: customer -> provider...", channel_amount);

        match customer_payments.open_channel(&provider_node_id, "127.0.0.1:9735", channel_amount) {
            Ok(channel_id) => {
                println!("             Channel open initiated! ID: {}", channel_id);
            }
            Err(e) => {
                println!("             Failed to open channel: {}", e);

                tokio::task::spawn_blocking(move || {
                    provider_payments.stop();
                    customer_payments.stop();
                })
                .await
                .ok();

                return Err(e);
            }
        }
    }
    println!("             Done in {}", fmt_duration(step.elapsed()));
    println!();

    // ── Step 4: Wait for channel to become usable ──
    let step = Instant::now();
    println!("  [{}] [Step 4/4] Waiting for channel confirmations...", ts());
    println!("             The funding tx needs ~6 confirmations (~60 min).");
    println!("             This is a one-time setup — once open, payments are instant.");
    println!("             Checking every 30s (up to 80 iterations)...");
    println!();

    let mut ready = false;
    for i in 1..=80 {
        tokio::time::sleep(Duration::from_secs(30)).await;

        let chs = customer_payments.list_channels().unwrap_or_default();
        let any_usable = chs.iter().any(|c| c.is_usable);

        let funding_info = chs
            .first()
            .and_then(|c| c.funding_txo.as_ref())
            .and_then(|txo| txo.split(':').next())
            .map(|s| s.to_string());

        let status = if chs.is_empty() {
            "pending...".to_string()
        } else if any_usable {
            "USABLE".to_string()
        } else {
            "waiting for confirmations...".to_string()
        };

        println!("             [{:>2}/80] Channel status: {} ({})", i, status, fmt_duration(step.elapsed()));
        if i == 1 {
            if let Some(txid) = &funding_info {
                println!("             Funding tx: https://mempool.space/testnet/tx/{}", txid);
            }
        }

        if any_usable {
            ready = true;
            break;
        }
    }

    if !ready {
        println!();
        println!("             Channel did not become usable within timeout.");
        println!("             Run this script again later to check.");

        tokio::task::spawn_blocking(move || {
            provider_payments.stop();
            customer_payments.stop();
        })
        .await
        .ok();

        return Err(ElisymError::Payment(
            "Channel did not become usable within timeout".into(),
        ));
    }

    // Show final channel info
    let channels = customer_payments.list_channels().unwrap_or_default();
    let outbound: u64 = channels
        .iter()
        .filter(|c| c.is_usable)
        .map(|c| c.outbound_capacity_msat / 1000)
        .sum();
    // Extract funding txid (format is "txid:vout", we need just the txid)
    let funding_txid = channels
        .first()
        .and_then(|c| c.funding_txo.as_ref())
        .and_then(|txo| txo.split(':').next())
        .map(|s| s.to_string());

    println!();
    println!("  --- Channel Setup Complete! ---");
    println!("  Capacity: {} sats", channel_amount);
    println!("  Outbound: {} sats (customer -> provider)", outbound);
    println!();
    println!("  Now run the demo:");
    println!("  Terminal 1: cargo run --example demo_provider");
    println!("  Terminal 2: cargo run --example demo_customer");
    println!("  Total time: {}", fmt_duration(total_start.elapsed()));
    if let Some(txid) = &funding_txid {
        println!("  Funding tx: https://mempool.space/testnet/tx/{}", txid);
    }
    println!();

    // Clean shutdown
    tokio::task::spawn_blocking(move || {
        provider_payments.stop();
        customer_payments.stop();
    })
    .await
    .ok();

    Ok(())
}

fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}.{}s", secs, d.subsec_millis() / 100)
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}
