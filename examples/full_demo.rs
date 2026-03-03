//! Full end-to-end demo: job payment → channel close → withdraw.
//!
//! This demonstrates the complete elisym payment lifecycle:
//! 1. Provider and Customer start with funded LDK Lightning wallets
//! 2. Customer submits a translation job (NIP-90)
//! 3. Provider generates a 1500 sat BOLT11 invoice
//! 4. Provider sends feedback(payment-required) with the invoice
//! 5. Customer pays the invoice over Lightning
//! 6. Provider confirms payment, delivers the result
//! 7. Channel is closed — funds return on-chain
//! 8. Each agent withdraws to their own address
//!
//! cargo run --example full_demo

use elisym_core::*;
use std::time::Duration;

const JOB_PRICE_MSAT: u64 = 1_500_000; // 1500 sats

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║           elisym-core: Full Payment Flow Demo               ║");
    println!("║  NIP-90 Job Marketplace + Lightning Network Payments        ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    // ════════════════════════════════════════════════════════════
    // STEP 1: Start both agents with LDK Lightning nodes
    // ════════════════════════════════════════════════════════════
    println!("[ STEP 1 ] Starting Lightning nodes...");
    println!();

    let provider = AgentNodeBuilder::new(
        "translation-agent",
        "AI Translation Agent with Lightning wallet",
    )
    // ATTN: Testnet-only hardcoded key — do NOT use on mainnet!
    // Pubkey: npub1dgz2hxxeu3m54kqxuvpdmh4k804pddwttu3raem50r5xrw6c86esxd0p6w
    .secret_key("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    .capabilities(vec!["translation".into()])
    .supported_job_kinds(vec![5100])
    .payment_config(PaymentConfig {
        storage_dir: "/tmp/elisym-ldk-provider".to_string(),
        network: ldk_node::bitcoin::Network::Testnet,
        esplora_url: "https://mempool.space/testnet/api".to_string(),
        listening_address: Some("0.0.0.0:9735".to_string()),
        ..Default::default()
    })
    .build()
    .await?;

    let customer = AgentNodeBuilder::new(
        "customer-agent",
        "Customer Agent with Lightning wallet",
    )
    // ATTN: Testnet-only hardcoded key — do NOT use on mainnet!
    // Pubkey: npub1dp5qwd78dk4msqwtygz02ld7fezhne8hzrxk0hqmggn4jtypax6szwtzka
    .secret_key("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
    .capabilities(vec!["customer".into()])
    .payment_config(PaymentConfig {
        storage_dir: "/tmp/elisym-ldk-customer".to_string(),
        network: ldk_node::bitcoin::Network::Testnet,
        esplora_url: "https://mempool.space/testnet/api".to_string(),
        listening_address: Some("0.0.0.0:9736".to_string()),
        ..Default::default()
    })
    .build()
    .await?;

    let provider_payments = provider.payments.as_ref().expect("LDK not configured");
    let customer_payments = customer.payments.as_ref().expect("LDK not configured");

    // Wait for initial blockchain sync
    println!("  Waiting for blockchain sync...");
    tokio::time::sleep(Duration::from_secs(10)).await;

    let provider_balance_before = provider_payments.onchain_balance()?;
    let customer_balance_before = customer_payments.onchain_balance()?;

    println!("  Provider: {} (on-chain: {} sats)", provider.identity.npub(), provider_balance_before);
    println!("  Customer: {} (on-chain: {} sats)", customer.identity.npub(), customer_balance_before);

    // Open channel if none exists
    let channel_amount = 30_000u64;
    let channels = customer_payments.list_channels()?;
    if channels.iter().any(|c| c.is_usable) {
        println!("  Channel already usable, skipping open_channel.");
    } else {
        let provider_node_id = provider_payments.node_id()?;
        if channels.is_empty() {
            println!("  Opening {} sat channel: customer → provider...", channel_amount);
            let channel_id = customer_payments.open_channel(
                &provider_node_id,
                "127.0.0.1:9735",
                channel_amount,
            )?;
            println!("  Channel open initiated! ID: {}", channel_id);
        } else {
            println!("  Channel exists but not yet usable, waiting...");
        }

        // Wait for the channel to become usable
        print!("  Waiting for channel confirmations");
        let mut ready = false;
        for i in 1..=40 {
            tokio::time::sleep(Duration::from_secs(30)).await;
            print!(".");
            if customer_payments.list_channels()?.iter().any(|c| c.is_usable) {
                println!(" done (~{}m)", i / 2);
                ready = true;
                break;
            }
        }
        if !ready {
            println!();
            return Err(ElisymError::Payment(
                "Channel did not become usable within timeout. \
                 On testnet this requires on-chain confirmations — try again later.".into()
            ));
        }
    }

    let outbound_sats = customer_payments.list_channels()?.first()
        .map(|c| c.outbound_capacity_msat / 1000)
        .unwrap_or(0);
    println!("  Channel:  {} sats capacity, {} sats outbound (customer→provider)",
        channel_amount, outbound_sats
    );

    // ════════════════════════════════════════════════════════════
    // STEP 2: Customer submits a translation job via Nostr (NIP-90)
    // ════════════════════════════════════════════════════════════
    println!();
    println!("[ STEP 2 ] Customer submits translation job (NIP-90, kind:5100)");
    println!();

    let mut jobs = provider.marketplace.subscribe_to_job_requests(&[100]).await?;
    let mut feedback_rx = customer.marketplace.subscribe_to_feedback().await?;
    let mut results_rx = customer.marketplace.subscribe_to_results(&[100], &[]).await?;

    let request_id = customer
        .marketplace
        .submit_job_request(
            100,
            "Hello, how are you?",
            "text",
            Some("text/plain"),
            Some(JOB_PRICE_MSAT),
            Some(&provider.identity.public_key()),
            vec!["en-to-es".into()],
        )
        .await?;

    println!("  Job request sent: {}", &request_id.to_hex()[..16]);
    println!("  Input: \"Hello, how are you?\"");
    println!("  Price: {} sats", JOB_PRICE_MSAT / 1000);

    // ════════════════════════════════════════════════════════════
    // STEP 3: Provider receives job, generates invoice
    // ════════════════════════════════════════════════════════════
    let provider_handle = tokio::spawn(async move {
        let job = jobs.recv().await.expect("No job received");

        println!();
        println!("[ STEP 3 ] Provider received job, generating invoice...");
        println!();
        println!("  Job ID:  {}", &job.event_id.to_hex()[..16]);
        println!("  Input:   \"{}\"", job.input_data);

        // Do the work
        let result_text = "Hola, como estas?";

        // Generate BOLT11 invoice for 1500 sats
        let payments = provider.payments.as_ref().unwrap();
        let invoice = payments
            .make_invoice(JOB_PRICE_MSAT, "elisym translation job", 3600)
            .expect("Failed to create invoice");

        println!("  Invoice: {}...{}", &invoice[..30], &invoice[invoice.len()-10..]);
        println!("  Amount:  {} sats", JOB_PRICE_MSAT / 1000);

        // ════════════════════════════════════════════════════════════
        // STEP 4: Provider sends payment-required feedback
        // ════════════════════════════════════════════════════════════
        println!();
        println!("[ STEP 4 ] Provider sends payment-required feedback with invoice");

        provider
            .marketplace
            .submit_job_feedback(
                &job.raw_event,
                JobStatus::PaymentRequired,
                None,
                Some(JOB_PRICE_MSAT),
                Some(&invoice),
            )
            .await
            .expect("Failed to send feedback");

        println!("  Feedback sent via Nostr relay");

        // Wait for payment
        print!("  Waiting for payment");
        let mut paid = false;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            match payments.lookup_invoice(&invoice) {
                Ok(status) if status.settled => {
                    paid = true;
                    break;
                }
                _ => print!("."),
            }
        }

        if paid {
            println!();
            println!("  Payment confirmed! {} sats received", JOB_PRICE_MSAT / 1000);
        } else {
            println!(" TIMEOUT");
        }

        // ════════════════════════════════════════════════════════════
        // STEP 6: Provider delivers result (only after payment!)
        // ════════════════════════════════════════════════════════════
        println!();
        println!("[ STEP 6 ] Provider delivers result after payment confirmation");

        provider
            .marketplace
            .submit_job_result(&job.raw_event, result_text, Some(JOB_PRICE_MSAT))
            .await
            .expect("Failed to send result");

        println!("  Result: \"{}\"", result_text);

        provider
    });

    // ════════════════════════════════════════════════════════════
    // STEP 5: Customer receives invoice, pays via Lightning
    // ════════════════════════════════════════════════════════════
    let timeout = tokio::time::sleep(Duration::from_secs(60));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            Some(fb) = feedback_rx.recv() => {
                if fb.status == "payment-required" {
                    if let Some(invoice) = &fb.payment_invoice {
                        println!();
                        println!("[ STEP 5 ] Customer received invoice, paying via Lightning...");
                        println!();
                        println!("  Invoice received from provider");
                        if let Some(ref payments) = customer.payments {
                            match payments.pay_invoice(invoice) {
                                Ok(pr) => {
                                    println!("  Payment sent! ID: {}...", &pr.payment_id[..16]);
                                    println!("  Amount: {} sats", JOB_PRICE_MSAT / 1000);
                                }
                                Err(e) => println!("  Payment failed: {}", e),
                            }
                        }
                    }
                }
            }
            Some(result) = results_rx.recv() => {
                println!();
                println!("  Customer received result: \"{}\"", result.content);
                println!();
                println!("  ======================================");
                println!("  JOB COMPLETED WITH PAYMENT!");
                println!("  ======================================");
                break;
            }
            _ = &mut timeout => {
                println!("  Timeout!");
                break;
            }
        }
    }

    let provider = provider_handle.await.expect("Provider task failed");

    // ════════════════════════════════════════════════════════════
    // STEP 7: Close the Lightning channel
    // ════════════════════════════════════════════════════════════
    println!();
    println!("[ STEP 7 ] Closing Lightning channel...");
    println!();

    let provider_payments = provider.payments.as_ref().unwrap();
    let customer_payments = customer.payments.as_ref().unwrap();
    let provider_id = provider_payments.node_id()?;
    let _customer_id = customer_payments.node_id()?;

    customer_payments.close_channel(&provider_id)?;
    println!("  Channel close initiated (cooperative)");

    // Wait for close
    print!("  Waiting for close");
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_secs(3)).await;
        print!(".");
        if customer_payments.list_channels()?.is_empty()
            && provider_payments.list_channels()?.is_empty()
        {
            println!(" done");
            break;
        }
    }

    // Wait for closing tx to settle
    println!("  Waiting 30s for closing tx to confirm...");
    tokio::time::sleep(Duration::from_secs(30)).await;

    let provider_final = provider_payments.onchain_balance()?;
    let customer_final = customer_payments.onchain_balance()?;

    println!();
    println!("  Provider on-chain: {} sats", provider_final);
    println!("  Customer on-chain: {} sats", customer_final);

    // ════════════════════════════════════════════════════════════
    // SUMMARY
    // ════════════════════════════════════════════════════════════
    // provider_balance_before is pure on-chain (no channel funds).
    // customer_balance_before is on-chain BEFORE channel open,
    // so total customer funds before = customer_balance_before.
    let provider_earned = provider_final as i64 - provider_balance_before as i64;
    let customer_lost = customer_balance_before as i64 - customer_final as i64;
    let payment_sats = (JOB_PRICE_MSAT / 1000) as i64;
    let total_fees = customer_lost - payment_sats;

    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║                        SUMMARY                             ║");
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!("║                                                            ║");
    println!("║  Job:      \"Hello, how are you?\" → \"Hola, como estas?\"     ║");
    println!("║  Payment:  {} sats via Lightning (BOLT11)                 ║", payment_sats);
    println!("║                                                            ║");
    println!("║  Provider (translation-agent):                             ║");
    println!("║    before:  {:>6} sats                                    ║", provider_balance_before);
    println!("║    after:   {:>6} sats  ({:+} sats)                      ║", provider_final, provider_earned);
    println!("║                                                            ║");
    println!("║  Customer (customer-agent):                                ║");
    println!("║    before:  {:>6} sats                                    ║", customer_balance_before);
    println!("║    after:   {:>6} sats  (-{} paid, -{} fees)             ║", customer_final, payment_sats, total_fees);
    println!("║                                                            ║");
    println!("║  Funds stay in each agent's wallet (no extra withdraw).    ║");
    println!("║  Use `withdraw` example to send to an external address.    ║");
    println!("║                                                            ║");
    println!("║  Protocol: Nostr (NIP-90) + Lightning Network (BOLT11)     ║");
    println!("║  Network:  Bitcoin Testnet                                 ║");
    println!("╚══════════════════════════════════════════════════════════════╝");

    // Clean shutdown
    drop(feedback_rx);
    drop(results_rx);
    tokio::task::spawn_blocking(move || {
        drop(provider);
        drop(customer);
    })
    .await
    .ok();

    Ok(())
}
