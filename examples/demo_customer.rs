//! Demo Customer Agent — discovers provider, submits task, pays via Lightning.
//!
//! Run: cargo run --example demo_customer

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

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let total_start = Instant::now();

    println!();
    println!("  \u{2554}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2557}");
    println!("  \u{2551}       elisym-core Demo: Customer Agent            \u{2551}");
    println!("  \u{255a}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{255d}");
    println!();

    // ── Step 1: Start agent + Lightning node ──
    let step = Instant::now();
    println!("  [{}] [Step 1/5] Starting agent and Lightning node...", ts());

    let customer = AgentNodeBuilder::new(
        "customer-agent",
        "Customer agent that requests AI summarization",
    )
    // ATTN: Testnet-only hardcoded key — do NOT use on mainnet!
    // Pubkey: npub1dp5qwd78dk4msqwtygz02ld7fezhne8hzrxk0hqmggn4jtypax6szwtzka
    // To fund: run `cargo run --example demo_setup` — it prints the LDK on-chain address
    // when balance is insufficient. The customer needs on-chain BTC to open a Lightning channel.
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

    // Wait for blockchain sync
    tokio::time::sleep(Duration::from_secs(5)).await;

    let npub = customer.identity.npub();
    println!("             Agent pubkey: {}", npub);
    println!("             Nostr profile: https://njump.me/{}", npub);
    if let Some(ref payments) = customer.payments {
        let balance = payments.onchain_balance().unwrap_or(0);
        let channels = payments.list_channels().unwrap_or_default();
        let usable = channels.iter().filter(|c| c.is_usable).count();
        let outbound: u64 = channels
            .iter()
            .filter(|c| c.is_usable)
            .map(|c| c.outbound_capacity_msat / 1000)
            .sum();
        println!("             Lightning node ready ({} sats on-chain, {} usable channels, {} sats outbound)", balance, usable, outbound);
    }
    println!("             Done in {}", fmt_duration(step.elapsed()));
    println!();

    // ── Step 2: Discover provider ──
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

    // ── Pre-flight: check Lightning balance ──
    let bid_msat: u64 = 1_000_000; // 1000 sats
    if let Some(ref payments) = customer.payments {
        let channels = payments.list_channels().unwrap_or_default();
        let usable = channels.iter().filter(|c| c.is_usable).count();
        let outbound_msat: u64 = channels
            .iter()
            .filter(|c| c.is_usable)
            .map(|c| c.outbound_capacity_msat)
            .sum();

        if usable == 0 {
            println!("  ERROR: No usable Lightning channels!");
            println!("         Run `cargo run --example demo_setup` to open a channel first.");
            println!();
            tokio::task::spawn_blocking(move || drop(customer)).await.ok();
            return Ok(());
        }

        if outbound_msat < bid_msat {
            println!("  ERROR: Insufficient Lightning outbound capacity!");
            println!("         Required:  {} sats (job bid)", bid_msat / 1000);
            println!("         Available: {} sats outbound", outbound_msat / 1000);
            println!("         Open a larger channel or fund the existing one.");
            println!("         Run `cargo run --example demo_setup` for channel setup.");
            println!();
            tokio::task::spawn_blocking(move || drop(customer)).await.ok();
            return Ok(());
        }

        println!("  [{}] [Pre-flight] Lightning balance OK: {} sats outbound ({} usable channel(s))",
            ts(),
            outbound_msat / 1000, usable);
        println!();
    }

    // ── Step 3: Submit job ──
    let step = Instant::now();
    println!("  [{}] [Step 3/5] Submitting summarization task...", ts());
    println!("             Text: \"{}\"", SAMPLE_TEXT);
    println!("             Bid: {} sats", bid_msat / 1000);

    // Subscribe to feedback and results before submitting
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
            Some(bid_msat),
            Some(&provider.pubkey),
            vec!["summarization".into()],
        )
        .await?;

    let request_hex = request_id.to_hex();
    println!("             Job submitted via Nostr");
    println!("             Job event: https://njump.me/{}", request_hex);
    println!("             Done in {}", fmt_duration(step.elapsed()));
    println!();

    // ── Event loop: handle feedback, payment, result ──
    let step = Instant::now();
    let timeout = tokio::time::sleep(Duration::from_secs(180));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            Some(fb) = feedback_rx.recv() => {
                // Skip stale feedback from previous runs
                if fb.request_id != request_id { continue; }
                match fb.parsed_status() {
                    Some(JobStatus::Processing) => {
                        println!("             Provider is processing the task...");
                    }
                    Some(JobStatus::PaymentRequired) => {
                        if let Some(invoice) = &fb.payment_invoice {
                            let fb_hex = fb.event_id.to_hex();
                            println!("  [{}] [Step 4/5] Payment requested! Paying via Lightning...", ts());
                            println!("             Feedback event: https://njump.me/{}", fb_hex);
                            println!("             Invoice: {}...{}", &invoice[..30.min(invoice.len())], &invoice[invoice.len().saturating_sub(10)..]);
                            if let Some(ref payments) = customer.payments {
                                // Snapshot before payment
                                let chs_before = payments.list_channels().unwrap_or_default();
                                let outbound_before: u64 = chs_before.iter().filter(|c| c.is_usable).map(|c| c.outbound_capacity_msat / 1000).sum();
                                match payments.pay_invoice(invoice) {
                                    Ok(pr) => {
                                        println!("             Payment sent! ID: {}...", &pr.payment_id[..16.min(pr.payment_id.len())]);
                                        println!("             Amount: 1000 sats");
                                        // Show balance change
                                        let chs_after = payments.list_channels().unwrap_or_default();
                                        let outbound_after: u64 = chs_after.iter().filter(|c| c.is_usable).map(|c| c.outbound_capacity_msat / 1000).sum();
                                        println!("             Lightning outbound: {} -> {} sats (-{})", outbound_before, outbound_after, outbound_before.saturating_sub(outbound_after));
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
                // Skip stale results from previous runs
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
                println!("  Payment:   1000 sats via Lightning (BOLT11)");
                println!("  Time:      {}", fmt_duration(total_start.elapsed()));
                println!();
                println!("  Customer: https://njump.me/{}", npub);
                println!("  Provider: https://njump.me/{}", provider_npub);
                println!("  Job:      https://njump.me/{}", request_hex);
                println!("  Result:   https://njump.me/{}", result_hex);
                println!();
                break;
            }
            _ = &mut timeout => {
                println!("  Timeout: no result received in 3 minutes.");
                break;
            }
        }
    }

    // Clean shutdown
    drop(feedback_rx);
    drop(results_rx);
    tokio::task::spawn_blocking(move || drop(customer))
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

