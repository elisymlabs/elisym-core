//! Quick helper to show LDK wallet addresses and balances.
//! cargo run --no-default-features --features payments-ldk --example wallet_info

use elisym_core::*;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    println!("Starting LDK nodes...\n");

    // Provider
    let mut provider_payments = PaymentService::new(PaymentConfig {
        storage_dir: "/tmp/elisym-ldk-provider".to_string(),
        network: ldk_node::bitcoin::Network::Testnet,
        esplora_url: "https://mempool.space/testnet/api".to_string(),
        listening_address: Some("0.0.0.0:9735".to_string()),
        ..Default::default()
    });
    provider_payments.start().await?;

    // Customer
    let mut customer_payments = PaymentService::new(PaymentConfig {
        storage_dir: "/tmp/elisym-ldk-customer".to_string(),
        network: ldk_node::bitcoin::Network::Testnet,
        esplora_url: "https://mempool.space/testnet/api".to_string(),
        listening_address: Some("0.0.0.0:9736".to_string()),
        ..Default::default()
    });
    customer_payments.start().await?;

    // Wait for blockchain sync — LDK needs time to scan blocks via Esplora
    println!("Waiting 30s for blockchain sync...");
    tokio::time::sleep(Duration::from_secs(30)).await;

    println!("=== PROVIDER ===");
    println!("  Node ID:  {}", provider_payments.node_id()?);
    println!("  Balance:  {} sats", provider_payments.onchain_balance()?);
    println!("  Address:  {}", provider_payments.new_onchain_address()?);
    println!("  Channels: {:?}", provider_payments.list_channels()?);

    println!("\n=== CUSTOMER ===");
    println!("  Node ID:  {}", customer_payments.node_id()?);
    println!("  Balance:  {} sats", customer_payments.onchain_balance()?);
    println!("  Address:  {}", customer_payments.new_onchain_address()?);
    println!("  Channels: {:?}", customer_payments.list_channels()?);

    println!("\n---");
    println!("Send testnet BTC to both addresses above.");
    println!("After confirmation, re-run this to check balances.");

    // Clean shutdown
    tokio::task::spawn_blocking(move || {
        provider_payments.stop();
        customer_payments.stop();
    })
    .await
    .ok();

    Ok(())
}
