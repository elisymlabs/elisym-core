//! Demo Customer Agent — discovers provider, submits task, pays via Solana.
//!
//! Run: SOLANA_SECRET_KEY=<base58> cargo run --example solana_demo_customer --features payments-solana

use elisym_core::*;
use nostr_sdk::ToBech32;
use std::time::{Duration, Instant};

fn ts() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

const SAMPLE_TEXT: &str = "\
Artificial intelligence has rapidly evolved from a niche research field into a \
transformative force reshaping industries worldwide. Modern AI systems, powered \
by large language models and neural networks, can now understand and generate \
human language, analyze complex datasets, and even write software code. These \
advances have led to unprecedented automation in healthcare diagnostics, \
financial analysis, and scientific research. However, the rapid deployment of \
AI also raises important questions about job displacement, algorithmic bias, \
data privacy, and the concentration of power in a few technology companies.";

const BID_LAMPORTS: u64 = 10_000_000; // 0.01 SOL

#[tokio::main]
async fn main() -> Result<()> {
    let total_start = Instant::now();

    let solana_keypair = std::env::var("SOLANA_SECRET_KEY").unwrap_or_else(|_| {
        eprintln!("Error: SOLANA_SECRET_KEY environment variable is required (base58 secret key)");
        std::process::exit(1);
    });

    println!();
    println!("  ╔═══════════════════════════════════════════════════╗");
    println!("  ║   elisym-core Demo: Customer Agent (Solana Devnet) ║");
    println!("  ╚═══════════════════════════════════════════════════╝");
    println!();

    // -- Step 1: Start agent + Solana provider --
    let step = Instant::now();
    println!("  [{}] [Step 1/5] Starting agent with Solana payment provider...", ts());

    let solana_provider = SolanaPaymentProvider::from_secret_key(
        SolanaPaymentConfig::default(), // Devnet + SOL
        &solana_keypair,
    )?;
    let solana_address = solana_provider.address();

    let customer = AgentNodeBuilder::new(
        "customer-agent",
        "Customer agent that requests AI summarization",
    )
    .capabilities(vec!["customer".into()])
    .solana_payment_provider(solana_provider)
    .build()
    .await?;

    let npub = customer.identity.npub();
    let balance = customer.solana_payments()
        .map(|p| p.balance().unwrap_or(0))
        .unwrap_or(0);
    println!("             Agent pubkey: {}", npub);
    println!("             Nostr profile: https://njump.me/{}", npub);
    println!("             Solana address: {}", solana_address);
    println!("             Solscan: https://solscan.io/account/{}?cluster=devnet", solana_address);
    println!("             SOL balance: {} lamports ({:.4} SOL)",
        balance, balance as f64 / 1_000_000_000.0);

    if balance < BID_LAMPORTS {
        println!();
        println!("  WARNING: Insufficient SOL balance!");
        println!("         Required:  {} lamports ({:.4} SOL)", BID_LAMPORTS, BID_LAMPORTS as f64 / 1_000_000_000.0);
        println!("         Available: {} lamports ({:.4} SOL)", balance, balance as f64 / 1_000_000_000.0);
        println!("         Get devnet SOL: solana airdrop 1 {} --url devnet", solana_address);
        println!();
        return Ok(());
    }

    println!("             Done in {}", fmt_duration(step.elapsed()));
    println!();

    // -- Step 2: Discover provider --
    let step = Instant::now();
    println!("  [{}] [Step 2/5] Discovering AI agents on Nostr...", ts());

    let filter = AgentFilter {
        capabilities: vec!["summarization".into()],
        ..Default::default()
    };

    let agents;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let found = customer.discovery.search_agents(&filter).await?;
        if !found.is_empty() {
            agents = found;
            break;
        }
        if attempt >= 30 {
            println!("             No summarization agents found after 30 attempts. Exiting.");
            return Ok(());
        }
        println!("             No agents found yet, retrying in 2s... (attempt {}/30)", attempt);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let provider = &agents[0];
    let provider_npub = provider.pubkey.to_bech32().unwrap_or_else(|_| provider.pubkey.to_hex());
    println!("             Found {} agent(s) with summarization capability", agents.len());
    println!("             Using: {} ({})", provider.card.name, &provider_npub[..20]);
    println!("             Provider profile: https://njump.me/{}", provider_npub);
    println!("             Done in {}", fmt_duration(step.elapsed()));
    println!();

    // -- Step 3: Submit job --
    let step = Instant::now();
    println!("  [{}] [Step 3/5] Submitting summarization task...", ts());
    println!("             Text: \"{}\"", SAMPLE_TEXT);
    println!("             Bid: {} lamports ({:.4} SOL)", BID_LAMPORTS, BID_LAMPORTS as f64 / 1_000_000_000.0);

    let mut feedback_rx = customer.marketplace.subscribe_to_feedback().await?;
    let mut results_rx = customer
        .marketplace
        .subscribe_to_results(&[100], &[provider.pubkey])
        .await?;

    let request_id = customer
        .marketplace
        .submit_job_request(
            100,
            SAMPLE_TEXT,
            "text",
            Some("text/plain"),
            Some(BID_LAMPORTS),
            Some(&provider.pubkey),
            vec!["summarization".into()],
        )
        .await?;

    let request_hex = request_id.to_hex();
    println!("             Job submitted via Nostr");
    println!("             Job event: https://njump.me/{}", request_hex);
    println!("             Done in {}", fmt_duration(step.elapsed()));
    println!();

    // -- Event loop: handle feedback, payment, result --
    let step = Instant::now();
    let timeout = tokio::time::sleep(Duration::from_secs(180));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            Some(fb) = feedback_rx.recv() => {
                if fb.request_id != request_id { continue; }
                match fb.parsed_status() {
                    Some(JobStatus::Processing) => {
                        println!("             Provider is processing the task...");
                    }
                    Some(JobStatus::PaymentRequired) => {
                        if let Some(request_str) = &fb.payment_request {
                            let fb_hex = fb.event_id.to_hex();
                            let chain = fb.payment_chain.as_deref().unwrap_or("unknown");
                            println!("  [{}] [Step 4/5] Payment requested! Paying via {} ...", ts(), chain);
                            println!("             Feedback event: https://njump.me/{}", fb_hex);
                            if let Some(ref payments) = customer.payments {
                                let balance_before = customer
                                    .solana_payments()
                                    .map(|p| p.balance().unwrap_or(0))
                                    .unwrap_or(0);
                                match payments.pay(request_str) {
                                    Ok(pr) => {
                                        println!("             Payment sent! Tx: {}", pr.payment_id);
                                        println!("             Solscan: https://solscan.io/tx/{}?cluster=devnet", pr.payment_id);
                                        println!("             Amount: 0.01 SOL ({} lamports)", BID_LAMPORTS);
                                        let balance_after = customer
                                            .solana_payments()
                                            .map(|p| p.balance().unwrap_or(0))
                                            .unwrap_or(0);
                                        println!("             SOL balance: {} -> {} lamports", balance_before, balance_after);
                                        println!("             Done in {}", fmt_duration(step.elapsed()));
                                    }
                                    Err(e) => {
                                        println!("             Payment failed: {}", e);
                                        break;
                                    }
                                }
                            }
                        }
                        println!();
                    }
                    Some(JobStatus::Error) => {
                        println!("             Error from provider: {}", fb.extra_info.unwrap_or_default());
                        break;
                    }
                    _ => {
                        println!("             Feedback: {}", fb.status);
                    }
                }
            }
            Some(result) = results_rx.recv() => {
                if result.request_id != request_id { continue; }
                let result_hex = result.event_id.to_hex();
                println!("  [{}] [Step 5/5] Result received! ({})", ts(), fmt_duration(step.elapsed()));
                println!("             Result event: https://njump.me/{}", result_hex);
                println!();
                println!("  Summary: {}", result.content);
                println!();
                println!("  --- Demo Complete! ---");
                println!("  Discovery: Nostr (NIP-89)");
                println!("  Task:      NIP-90 Data Vending Machine");
                println!("  AI:        Claude (claude-sonnet-4-20250514)");
                println!("  Payment:   0.01 SOL via Solana (devnet)");
                println!("  Time:      {}", fmt_duration(total_start.elapsed()));
                println!();
                println!("  Customer:  https://njump.me/{}", npub);
                println!("  Solana:    https://solscan.io/account/{}?cluster=devnet", solana_address);
                println!("  Provider:  https://njump.me/{}", provider_npub);
                println!("  Job:       https://njump.me/{}", request_hex);
                println!("  Result:    https://njump.me/{}", result_hex);
                println!();
                break;
            }
            _ = &mut timeout => {
                println!("  Timeout: no result received in 3 minutes.");
                break;
            }
        }
    }

    drop(feedback_rx);
    drop(results_rx);
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
