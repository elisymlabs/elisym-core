//! Open a Lightning channel from customer → provider for testing.
//! cargo run --no-default-features --features payments-ldk --example open_channel

use elisym_core::*;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    println!("Starting LDK nodes...\n");

    // Provider — listening on 9735
    let mut provider_payments = LdkPaymentProvider::new(LdkPaymentConfig {
        storage_dir: "/tmp/elisym-ldk-provider".to_string(),
        network: ldk_node::bitcoin::Network::Testnet,
        esplora_url: "https://mempool.space/testnet/api".to_string(),
        listening_address: Some("0.0.0.0:9735".to_string()),
    });
    provider_payments.start().await?;

    let provider_node_id = provider_payments.node_id()?;
    println!("Provider Node ID: {}", provider_node_id);
    println!("Provider Balance: {} sats", provider_payments.onchain_balance()?);

    // Customer — listening on 9736
    let mut customer_payments = LdkPaymentProvider::new(LdkPaymentConfig {
        storage_dir: "/tmp/elisym-ldk-customer".to_string(),
        network: ldk_node::bitcoin::Network::Testnet,
        esplora_url: "https://mempool.space/testnet/api".to_string(),
        listening_address: Some("0.0.0.0:9736".to_string()),
    });
    customer_payments.start().await?;

    let customer_node_id = customer_payments.node_id()?;
    println!("Customer Node ID: {}", customer_node_id);
    println!("Customer Balance: {} sats", customer_payments.onchain_balance()?);

    // Wait for sync
    println!("\nWaiting 10s for blockchain sync...");
    tokio::time::sleep(Duration::from_secs(10)).await;

    println!("Provider Balance after sync: {} sats", provider_payments.onchain_balance()?);
    println!("Customer Balance after sync: {} sats", customer_payments.onchain_balance()?);

    // Check existing channels
    let channels = customer_payments.list_channels()?;
    if !channels.is_empty() {
        println!("\nExisting channels: {:?}", channels);
        println!("Channel already exists, skipping open_channel.");
    } else {
        // Open channel: customer → provider (30k sats capacity)
        // Customer needs outbound liquidity to pay provider
        let channel_amount = 30_000; // sats
        println!(
            "\nOpening {} sat channel: customer → provider (127.0.0.1:9735)...",
            channel_amount
        );

        match customer_payments.open_channel(&provider_node_id, "127.0.0.1:9735", channel_amount) {
            Ok(channel_id) => {
                println!("Channel open initiated! ID: {}", channel_id);
            }
            Err(e) => {
                println!("Failed to open channel: {}", e);
            }
        }
    }

    // Keep nodes running so they can complete the funding handshake
    // and broadcast the funding transaction
    println!("\nKeeping nodes alive for funding tx broadcast and confirmations...");
    println!("Checking channel status every 30 seconds. Press Ctrl+C to stop.\n");

    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;

        let cust_balance = customer_payments.onchain_balance().unwrap_or(0);
        let prov_balance = provider_payments.onchain_balance().unwrap_or(0);
        let channels = customer_payments.list_channels().unwrap_or_default();

        println!(
            "Customer: {} sats on-chain | Provider: {} sats on-chain | Channels: {}",
            cust_balance,
            prov_balance,
            if channels.is_empty() {
                "none (pending...)".to_string()
            } else {
                format!("{:?}", channels)
            }
        );

        if !channels.is_empty() {
            println!("\nChannel is open! You can now run the payment_flow example.");
            break;
        }
    }

    // Clean shutdown
    tokio::task::spawn_blocking(move || {
        provider_payments.stop();
        customer_payments.stop();
    })
    .await
    .ok();

    Ok(())
}
