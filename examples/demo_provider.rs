//! Demo Provider Agent — AI summarization via Claude API + Lightning payments.
//!
//! Run: ANTHROPIC_API_KEY=sk-... cargo run --example demo_provider

use elisym_core::*;
use std::time::{Duration, Instant};

fn ts() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

const JOB_PRICE_MSAT: u64 = 1_000_000; // 1000 sats

// ── Claude API types ──

#[derive(serde::Serialize)]
struct ClaudeRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<ClaudeMessage>,
}

#[derive(serde::Serialize)]
struct ClaudeMessage {
    role: String,
    content: String,
}

#[derive(serde::Deserialize)]
struct ClaudeResponse {
    content: Vec<ClaudeContentBlock>,
}

#[derive(serde::Deserialize)]
struct ClaudeContentBlock {
    text: Option<String>,
}

async fn call_claude(api_key: &str, text: &str) -> Result<String> {
    let client = reqwest::Client::new();

    let request = ClaudeRequest {
        model: "claude-sonnet-4-20250514".to_string(),
        max_tokens: 300,
        messages: vec![ClaudeMessage {
            role: "user".to_string(),
            content: format!(
                "Summarize the following text in 5-8 words:\n\n{}",
                text
            ),
        }],
    };

    let response = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&request)
        .send()
        .await
        .map_err(|e| ElisymError::Config(format!("Claude API request failed: {}", e)))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "unknown".to_string());
        return Err(ElisymError::Config(format!(
            "Claude API error {}: {}",
            status, body
        )));
    }

    let body: ClaudeResponse = response
        .json()
        .await
        .map_err(|e| ElisymError::Config(format!("Failed to parse Claude response: {}", e)))?;

    body.content
        .into_iter()
        .find_map(|b| b.text)
        .ok_or_else(|| ElisymError::Config("No text in Claude response".into()))
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let total_start = Instant::now();

    let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_else(|_| {
        eprintln!("Error: ANTHROPIC_API_KEY environment variable is required");
        std::process::exit(1);
    });

    println!();
    println!("  \u{2554}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2557}");
    println!("  \u{2551}       elisym-core Demo: AI Provider Agent         \u{2551}");
    println!("  \u{255a}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{255d}");
    println!();

    // ── Step 1: Start agent + Lightning node ──
    let step = Instant::now();
    println!("  [{}] [Step 1/5] Starting agent and Lightning node...", ts());

    let provider = AgentNodeBuilder::new(
        "summarization-agent",
        "AI agent that summarizes text using Claude",
    )
    // ATTN: Testnet-only hardcoded key — do NOT use on mainnet!
    // Pubkey: npub1dgz2hxxeu3m54kqxuvpdmh4k804pddwttu3raem50r5xrw6c86esxd0p6w
    // To fund: run `cargo run --example demo_setup` — it prints the LDK on-chain address
    // when balance is insufficient. Or use your own secret key.
    .secret_key("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    .capabilities(vec!["summarization".into()])
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

    // Wait for blockchain sync
    tokio::time::sleep(Duration::from_secs(5)).await;

    let npub = provider.identity.npub();
    println!("             Agent pubkey: {}", npub);
    println!("             Nostr profile: https://njump.me/{}", npub);
    if let Some(ref payments) = provider.payments {
        let balance = payments.onchain_balance().unwrap_or(0);
        let channels = payments.list_channels().unwrap_or_default();
        let usable = channels.iter().filter(|c| c.is_usable).count();
        let inbound: u64 = channels.iter().filter(|c| c.is_usable).map(|c| c.inbound_capacity_msat / 1000).sum();
        let outbound: u64 = channels.iter().filter(|c| c.is_usable).map(|c| c.outbound_capacity_msat / 1000).sum();
        println!("             Lightning: {} usable channels, {} sats inbound, {} sats outbound", usable, inbound, outbound);
        println!("             On-chain: {} sats", balance);
    }
    println!("             Done in {}", fmt_duration(step.elapsed()));
    println!();

    // ── Step 2: Listen for job requests ──
    let step = Instant::now();
    println!("  [{}] [Step 2/5] Listening for job requests (kind:5100)...", ts());
    println!("             Waiting for customer to submit a task...");
    println!();

    let mut jobs = provider
        .marketplace
        .subscribe_to_job_requests(&[100])
        .await?;

    // Wait for a job
    let job = tokio::select! {
        Some(job) = jobs.recv() => job,
        _ = tokio::time::sleep(Duration::from_secs(300)) => {
            println!("  Timeout: no job received in 5 minutes.");
            return Ok(());
        }
    };
    let step2_elapsed = step.elapsed();

    // ── Step 3: Process with Claude ──
    let step = Instant::now();
    let job_event_hex = job.event_id.to_hex();
    println!("  [{}] [Step 3/5] Job received! (waited {})", ts(), fmt_duration(step2_elapsed));
    println!("             Processing with Claude...");
    println!("             Job ID: {}", &job_event_hex[..16]);
    println!("             Job event: https://njump.me/{}", job_event_hex);
    println!("             Customer: {}", &job.customer.to_hex()[..16]);
    println!("             Input: \"{}\"", &job.input_data);

    // Send processing feedback
    provider
        .marketplace
        .submit_job_feedback(
            &job.raw_event,
            JobStatus::Processing,
            None,
            None,
            None,
        )
        .await?;

    // Call Claude API
    println!("             Calling Claude API (claude-sonnet-4-20250514)...");
    let summary = call_claude(&api_key, &job.input_data).await?;

    println!("             Done in {}", fmt_duration(step.elapsed()));
    println!();
    println!("  Claude API Response:");
    println!("  {}", summary);
    println!();

    // ── Step 4: Request payment ──
    let step = Instant::now();
    println!("  [{}] [Step 4/5] Requesting payment (1000 sats)...", ts());

    let payments = provider
        .payments
        .as_ref()
        .ok_or_else(|| ElisymError::Payment("Payments not configured".into()))?;

    // Snapshot balance before payment
    let before_channels = payments.list_channels().unwrap_or_default();
    let before_outbound: u64 = before_channels.iter().filter(|c| c.is_usable).map(|c| c.outbound_capacity_msat / 1000).sum();
    let before_inbound: u64 = before_channels.iter().filter(|c| c.is_usable).map(|c| c.inbound_capacity_msat / 1000).sum();

    let invoice = payments.make_invoice(JOB_PRICE_MSAT, "elisym summarization job", 3600)?;
    println!("             Invoice: {}...{}", &invoice[..30], &invoice[invoice.len() - 10..]);

    let feedback_event_id = provider
        .marketplace
        .submit_job_feedback(
            &job.raw_event,
            JobStatus::PaymentRequired,
            None,
            Some(JOB_PRICE_MSAT),
            Some(&invoice),
        )
        .await?;

    let feedback_hex = feedback_event_id.to_hex();
    println!("             Invoice sent to customer via Nostr");
    println!("             Feedback event: https://njump.me/{}", feedback_hex);

    // Poll for payment
    print!("             Waiting for payment");
    let mut paid = false;
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        match payments.lookup_invoice(&invoice) {
            Ok(status) if status.settled => {
                paid = true;
                break;
            }
            _ => print!("."),
        }
    }
    println!();

    if !paid {
        println!("             Payment timeout!");
        provider
            .marketplace
            .submit_job_feedback(
                &job.raw_event,
                JobStatus::Error,
                Some("payment-timeout"),
                None,
                None,
            )
            .await?;
        return Err(ElisymError::Payment("Payment timeout".into()));
    }

    println!("             Payment confirmed! 1000 sats received");
    // Show balance change
    let after_channels = payments.list_channels().unwrap_or_default();
    let after_outbound: u64 = after_channels.iter().filter(|c| c.is_usable).map(|c| c.outbound_capacity_msat / 1000).sum();
    let after_inbound: u64 = after_channels.iter().filter(|c| c.is_usable).map(|c| c.inbound_capacity_msat / 1000).sum();
    println!("             Lightning outbound: {} -> {} sats (+{})", before_outbound, after_outbound, after_outbound.saturating_sub(before_outbound));
    println!("             Lightning inbound:  {} -> {} sats ({})", before_inbound, after_inbound, after_inbound as i64 - before_inbound as i64);
    println!("             Done in {}", fmt_duration(step.elapsed()));
    println!();

    // ── Step 5: Deliver result ──
    let step = Instant::now();
    println!("  [{}] [Step 5/5] Delivering result to customer...", ts());

    let result_event_id = provider
        .marketplace
        .submit_job_result(&job.raw_event, &summary, Some(JOB_PRICE_MSAT))
        .await?;

    let result_hex = result_event_id.to_hex();
    println!("             Result delivered via Nostr");
    println!("             Result event: https://njump.me/{}", result_hex);
    println!("             Done in {}", fmt_duration(step.elapsed()));
    println!();
    println!("  --- Job Complete! ---");
    println!("  Task: Text Summarization");
    println!("  AI:   Claude (claude-sonnet-4-20250514)");
    println!("  Paid: 1000 sats via Lightning");
    println!("  Time: {}", fmt_duration(total_start.elapsed()));
    println!();
    println!("  Provider: https://njump.me/{}", npub);
    println!("  Job:      https://njump.me/{}", job_event_hex);
    println!("  Feedback: https://njump.me/{}", feedback_hex);
    println!("  Result:   https://njump.me/{}", result_hex);
    println!();

    // Clean shutdown
    drop(jobs);
    tokio::task::spawn_blocking(move || drop(provider))
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

