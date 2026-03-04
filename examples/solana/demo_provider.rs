//! Demo Provider Agent — AI summarization via Claude API + Solana payments.
//!
//! Run: ANTHROPIC_API_KEY=sk-... SOLANA_SECRET_KEY=<base58> cargo run --example solana_demo_provider --features payments-solana

use elisym_core::*;
use std::time::{Duration, Instant};

fn ts() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

const JOB_PRICE_LAMPORTS: u64 = 10_000_000; // 0.01 SOL

// -- Claude API types --

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
    let total_start = Instant::now();

    let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_else(|_| {
        eprintln!("Error: ANTHROPIC_API_KEY environment variable is required");
        std::process::exit(1);
    });

    let solana_keypair = std::env::var("SOLANA_SECRET_KEY").unwrap_or_else(|_| {
        eprintln!("Error: SOLANA_SECRET_KEY environment variable is required (base58 secret key)");
        std::process::exit(1);
    });

    println!();
    println!("  ╔═══════════════════════════════════════════════════╗");
    println!("  ║   elisym-core Demo: AI Provider (Solana Devnet)    ║");
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

    let provider = AgentNodeBuilder::new(
        "summarization-agent",
        "AI agent that summarizes text using Claude",
    )
    .capabilities(vec!["summarization".into()])
    .supported_job_kinds(vec![5100])
    .solana_payment_provider(solana_provider)
    .build()
    .await?;

    let npub = provider.identity.npub();
    let balance_before = provider.solana_payments()
        .map(|p| p.balance().unwrap_or(0))
        .unwrap_or(0);
    println!("             Agent pubkey: {}", npub);
    println!("             Nostr profile: https://njump.me/{}", npub);
    println!("             Solana address: {}", solana_address);
    println!("             Solscan: https://solscan.io/account/{}?cluster=devnet", solana_address);
    println!("             SOL balance: {} lamports ({:.4} SOL)",
        balance_before, balance_before as f64 / 1_000_000_000.0);
    println!("             Done in {}", fmt_duration(step.elapsed()));
    println!();

    // -- Step 2: Listen for job requests --
    let step = Instant::now();
    println!("  [{}] [Step 2/5] Listening for job requests (kind:5100)...", ts());
    println!("             Waiting for customer to submit a task...");
    println!();

    let mut jobs = provider
        .marketplace
        .subscribe_to_job_requests(&[100])
        .await?;

    let job = tokio::select! {
        Some(job) = jobs.recv() => job,
        _ = tokio::time::sleep(Duration::from_secs(300)) => {
            println!("  Timeout: no job received in 5 minutes.");
            return Ok(());
        }
    };
    let step2_elapsed = step.elapsed();

    // -- Step 3: Process with Claude --
    let step = Instant::now();
    let job_event_hex = job.event_id.to_hex();
    println!("  [{}] [Step 3/5] Job received! (waited {})", ts(), fmt_duration(step2_elapsed));
    println!("             Processing with Claude...");
    println!("             Job ID: {}", &job_event_hex[..16]);
    println!("             Job event: https://njump.me/{}", job_event_hex);
    println!("             Customer: {}", &job.customer.to_hex()[..16]);
    println!("             Input: \"{}\"", &job.input_data);

    provider
        .marketplace
        .submit_job_feedback(
            &job.raw_event,
            JobStatus::Processing,
            None,
            None,
            None,
            None,
        )
        .await?;

    println!("             Calling Claude API (claude-sonnet-4-20250514)...");
    let summary = call_claude(&api_key, &job.input_data).await?;

    println!("             Done in {}", fmt_duration(step.elapsed()));
    println!();
    println!("  Claude API Response:");
    println!("  {}", summary);
    println!();

    // -- Step 4: Request payment --
    let step = Instant::now();
    println!("  [{}] [Step 4/5] Requesting payment (0.01 SOL)...", ts());

    let payments = provider
        .payments
        .as_ref()
        .ok_or_else(|| ElisymError::Payment("Payments not configured".into()))?;

    let payment_req = payments.create_payment_request(JOB_PRICE_LAMPORTS, "elisym summarization job", 3600)?;
    let request_str = &payment_req.request;
    println!("             Payment request: {}...", &request_str[..60.min(request_str.len())]);

    let feedback_event_id = provider
        .marketplace
        .submit_job_feedback(
            &job.raw_event,
            JobStatus::PaymentRequired,
            None,
            Some(JOB_PRICE_LAMPORTS),
            Some(request_str),
            Some("solana"),
        )
        .await?;

    let feedback_hex = feedback_event_id.to_hex();
    println!("             Payment request sent to customer via Nostr");
    println!("             Feedback event: https://njump.me/{}", feedback_hex);

    // Poll for payment
    print!("             Waiting for payment");
    let mut paid = false;
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        match payments.lookup_payment(request_str) {
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
                None,
            )
            .await?;
        return Err(ElisymError::Payment("Payment timeout".into()));
    }

    let balance_after = provider
        .solana_payments()
        .map(|p| p.balance().unwrap_or(0))
        .unwrap_or(0);
    println!("             Payment confirmed! 0.01 SOL received");
    println!("             SOL balance: {} -> {} lamports", balance_before, balance_after);
    println!("             Done in {}", fmt_duration(step.elapsed()));
    println!();

    // -- Step 5: Deliver result --
    let step = Instant::now();
    println!("  [{}] [Step 5/5] Delivering result to customer...", ts());

    let result_event_id = provider
        .marketplace
        .submit_job_result(&job.raw_event, &summary, Some(JOB_PRICE_LAMPORTS))
        .await?;

    let result_hex = result_event_id.to_hex();
    println!("             Result delivered via Nostr");
    println!("             Result event: https://njump.me/{}", result_hex);
    println!("             Done in {}", fmt_duration(step.elapsed()));
    println!();
    println!("  --- Job Complete! ---");
    println!("  Task: Text Summarization");
    println!("  AI:   Claude (claude-sonnet-4-20250514)");
    println!("  Paid: 0.01 SOL via Solana (devnet)");
    println!("  Time: {}", fmt_duration(total_start.elapsed()));
    println!();
    println!("  Provider:  https://njump.me/{}", npub);
    println!("  Solana:    https://solscan.io/account/{}?cluster=devnet", solana_address);
    println!("  Job:       https://njump.me/{}", job_event_hex);
    println!("  Feedback:  https://njump.me/{}", feedback_hex);
    println!("  Result:    https://njump.me/{}", result_hex);
    println!();

    drop(jobs);
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
