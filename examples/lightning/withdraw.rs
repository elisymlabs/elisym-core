//! Close any open channels and withdraw all on-chain funds.
//! cargo run --no-default-features --features payments-ldk --example withdraw -- <address>

use elisym_core::*;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let target_addr = std::env::args().nth(1).expect(
        "Usage: cargo run --no-default-features --features payments-ldk --example withdraw -- <btc-address>",
    );

    println!("Starting LDK nodes...\n");

    let mut provider = LdkPaymentProvider::new(LdkPaymentConfig {
        storage_dir: "/tmp/elisym-ldk-provider".to_string(),
        network: ldk_node::bitcoin::Network::Testnet,
        esplora_url: "https://mempool.space/testnet/api".to_string(),
        listening_address: Some("0.0.0.0:9735".to_string()),
    });
    provider.start().await?;

    let mut customer = LdkPaymentProvider::new(LdkPaymentConfig {
        storage_dir: "/tmp/elisym-ldk-customer".to_string(),
        network: ldk_node::bitcoin::Network::Testnet,
        esplora_url: "https://mempool.space/testnet/api".to_string(),
        listening_address: Some("0.0.0.0:9736".to_string()),
    });
    customer.start().await?;

    println!("Waiting 30s for blockchain sync...");
    tokio::time::sleep(Duration::from_secs(30)).await;

    let provider_id = provider.node_id()?;
    let customer_id = customer.node_id()?;

    // Close channels if any exist
    let prov_channels = provider.list_channels()?;
    let cust_channels = customer.list_channels()?;

    if !cust_channels.is_empty() || !prov_channels.is_empty() {
        println!("Closing open channels...");
        let _ = customer.close_channel(&provider_id);
        let _ = provider.close_channel(&customer_id);

        // Wait for closing tx
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if customer.list_channels()?.is_empty() && provider.list_channels()?.is_empty() {
                println!("Channels closed.");
                break;
            }
        }
    }

    println!("\nProvider balance: {} sats", provider.onchain_balance()?);
    println!("Customer balance: {} sats", customer.onchain_balance()?);
    println!("\nWithdrawing all funds to {}\n", target_addr);

    match provider.send_all_onchain(&target_addr) {
        Ok(txid) => println!("Provider withdraw tx: {}", txid),
        Err(e) => println!("Provider withdraw failed: {}", e),
    }

    match customer.send_all_onchain(&target_addr) {
        Ok(txid) => println!("Customer withdraw tx: {}", txid),
        Err(e) => println!("Customer withdraw failed: {}", e),
    }

    println!("\nDone!");

    tokio::task::spawn_blocking(move || {
        provider.stop();
        customer.stop();
    })
    .await
    .ok();

    Ok(())
}
