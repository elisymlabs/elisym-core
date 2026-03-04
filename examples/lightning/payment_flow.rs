//! Payment-first flow demo with built-in LDK wallet (NIP-90).
//!
//! This example demonstrates the full payment-first flow using
//! the agent's built-in LDK Lightning node:
//!
//! 1. Provider starts LDK node, subscribes to jobs
//! 2. Customer starts LDK node, discovers provider, submits a job
//! 3. Provider does the work, generates BOLT11 invoice
//! 4. Provider sends feedback(payment-required) with invoice
//! 5. Customer pays the invoice
//! 6. Provider verifies payment, sends result
//!
//! NOTE: For this to work end-to-end both agents need:
//! - Funded on-chain wallets (run `new_onchain_address()` and send testnet BTC)
//! - Open channels with liquidity
//!
//! For a quick test without real channels, run with:
//!   cargo run --example payment_flow --features payments-ldk
//!
//! The flow will run through all steps but payment will fail
//! without actual channel liquidity (expected in demo mode).

use elisym_core::*;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // ldk-node 0.5 brings rustls 0.23; install crypto provider for TLS
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // ── Provider with LDK node ──
    let provider = AgentNodeBuilder::new(
        "ldk-translation-agent",
        "Translation agent with built-in Lightning wallet",
    )
    // ATTN: Testnet-only hardcoded key — do NOT use on mainnet!
    .secret_key("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    .capabilities(vec!["translation".into()])
    .supported_job_kinds(vec![5100])
    .ldk_payment_config(LdkPaymentConfig {
        storage_dir: "/tmp/elisym-ldk-provider".to_string(),
        network: ldk_node::bitcoin::Network::Testnet,
        esplora_url: "https://mempool.space/testnet/api".to_string(),
        listening_address: Some("0.0.0.0:9735".to_string()),
    })
    .build()
    .await?;

    println!("Provider started: {}", provider.identity.npub());
    if let Some(ldk) = provider.ldk_payments() {
        println!("  Node ID: {}", ldk.node_id()?);
        println!("  On-chain balance: {} sats", ldk.onchain_balance()?);
        println!("  Fund address: {}", ldk.new_onchain_address()?);
        println!("  Channels: {:?}", ldk.list_channels()?);
    }

    let mut jobs = provider
        .marketplace
        .subscribe_to_job_requests(&[100])
        .await?;

    // ── Customer with LDK node ──
    let customer = AgentNodeBuilder::new(
        "ldk-customer-agent",
        "Customer agent with built-in Lightning wallet",
    )
    // ATTN: Testnet-only hardcoded key — do NOT use on mainnet!
    .secret_key("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
    .capabilities(vec!["customer".into()])
    .ldk_payment_config(LdkPaymentConfig {
        storage_dir: "/tmp/elisym-ldk-customer".to_string(),
        network: ldk_node::bitcoin::Network::Testnet,
        esplora_url: "https://mempool.space/testnet/api".to_string(),
        listening_address: Some("0.0.0.0:9736".to_string()),
    })
    .build()
    .await?;

    println!("\nCustomer started: {}", customer.identity.npub());
    if let Some(ldk) = customer.ldk_payments() {
        println!("  Node ID: {}", ldk.node_id()?);
        println!("  On-chain balance: {} sats", ldk.onchain_balance()?);
        println!("  Fund address: {}", ldk.new_onchain_address()?);
        println!("  Channels: {:?}", ldk.list_channels()?);
    }

    // Wait for LDK blockchain sync — poll until channel is ready
    println!("\nWaiting for blockchain sync and channel readiness...");
    for i in 1..=24 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if let Some(ldk) = customer.ldk_payments() {
            let channels = ldk.list_channels()?;
            let ready = channels.iter().any(|c| c.is_channel_ready);
            if ready {
                println!("  Channel ready after {}s!", i * 5);
                break;
            }
            if i % 6 == 0 {
                println!("  [{}s] still syncing...", i * 5);
            }
        }
    }

    if let Some(ldk) = customer.ldk_payments() {
        println!("Customer channels: {:?}", ldk.list_channels()?);
    }

    // Customer subscribes to feedback and results
    let mut feedback_rx = customer.marketplace.subscribe_to_feedback().await?;
    let mut results_rx = customer.marketplace.subscribe_to_results(&[100], &[]).await?;

    // Customer submits job
    let request_id = customer
        .marketplace
        .submit_job_request(
            100,
            "Hello, how are you?",
            "text",
            Some("text/plain"),
            Some(1000),
            Some(&provider.identity.public_key()),
            vec!["en-to-es".into()],
        )
        .await?;

    println!("\nCustomer submitted job: {}", request_id);

    // ── Provider handles job with payment-first flow ──
    let provider_handle = tokio::spawn(async move {
        if let Some(job) = jobs.recv().await {
            println!("\nProvider received job: {}", job.event_id);

            // 1. Do the work
            let result_text = format!("Traducción: '{}' → 'Hola, ¿cómo estás?'", job.input_data);

            // 2. Generate BOLT11 invoice
            let payments = provider.payments.as_ref().expect("LDK not configured");
            let payment_req = match payments.create_payment_request(1000, "elisym job payment", 3600) {
                Ok(req) => {
                    println!("Provider generated invoice: {}...", &req.request[..80.min(req.request.len())]);
                    req
                }
                Err(e) => {
                    println!("Provider failed to generate invoice: {}", e);
                    // Fallback: send result without payment
                    provider
                        .marketplace
                        .submit_job_result(&job.raw_event, &result_text, Some(1000))
                        .await
                        .ok();
                    return;
                }
            };

            // 3. Send payment-required feedback with invoice
            provider
                .marketplace
                .submit_job_feedback(
                    &job.raw_event,
                    JobStatus::PaymentRequired,
                    None,
                    Some(1000),
                    Some(&payment_req.request),
                    None,
                )
                .await
                .expect("Failed to send feedback");

            println!("Provider sent payment-required feedback");

            // 4. Wait for payment (poll lookup_payment)
            let mut paid = false;
            for _ in 0..30 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                match payments.lookup_payment(&payment_req.request) {
                    Ok(status) if status.settled => {
                        println!("Provider confirmed payment received!");
                        paid = true;
                        break;
                    }
                    Ok(_) => {} // still pending
                    Err(_) => {}
                }
            }

            // 5. Send result (only after payment, or timeout in demo)
            if paid {
                println!("Provider sending result after confirmed payment");
            } else {
                println!("Provider sending result (demo mode — payment not received)");
            }

            provider
                .marketplace
                .submit_job_result(&job.raw_event, &result_text, Some(1000))
                .await
                .expect("Failed to send result");

            println!("Provider sent result");
        }

        // Drop provider in blocking context to avoid LDK panic
        tokio::task::spawn_blocking(move || drop(provider)).await.ok();
    });

    // ── Customer handles feedback + payment + result ──
    let timeout = tokio::time::sleep(Duration::from_secs(60));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            Some(fb) = feedback_rx.recv() => {
                println!("\nCustomer got feedback: status={}", fb.status);

                if fb.status == "payment-required" {
                    if let Some(invoice) = &fb.payment_request {
                        println!("Customer received invoice, attempting payment...");
                        if let Some(ref payments) = customer.payments {
                            match payments.pay(invoice) {
                                Ok(result) => {
                                    println!("Customer payment initiated: {:?}", result);
                                }
                                Err(e) => {
                                    println!("Customer payment failed: {} (expected without channels)", e);
                                }
                            }
                        }
                    }
                }
            }
            Some(result) = results_rx.recv() => {
                println!("\nCustomer got result: {}", result.content);
                println!("\nPayment-first flow completed!");
                break;
            }
            _ = &mut timeout => {
                println!("\nTimeout waiting for result.");
                break;
            }
        }
    }

    provider_handle.await.ok();

    // Explicitly stop LDK nodes before dropping in async context
    drop(feedback_rx);
    drop(results_rx);
    tokio::task::spawn_blocking(move || drop(customer)).await.ok();

    Ok(())
}
