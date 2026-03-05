use elisym_core::*;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Use a fixed identity for the customer too
    let agent = AgentNodeBuilder::new(
        "customer-agent",
        "AI agent looking for translation services",
    )
    // ATTN: Testnet-only hardcoded key — do NOT use on mainnet!
    .secret_key("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
    .capabilities(vec!["customer".into()])
    .build()
    .await?;

    let my_pubkey = agent.identity.public_key();
    println!("Customer started: {}", agent.identity.npub());

    // Search for translation agents
    let filter = AgentFilter {
        capabilities: vec!["translation".into()],
        job_kind: Some(5100),
        ..Default::default()
    };

    let agents = agent.discovery.search_agents(&filter).await?;
    println!("Found {} agents with 'translation' capability", agents.len());

    // Filter out ourselves and pick a real provider
    let provider = agents.iter().find(|a| a.pubkey != my_pubkey);

    let provider = match provider {
        Some(p) => p,
        None => {
            println!("No external provider found. Make sure the provider example is running first.");
            return Ok(());
        }
    };

    println!(
        "Using provider: {} ({})",
        provider.card.name,
        provider.pubkey.to_hex()
    );

    // Subscribe to results and feedback before sending request
    let mut results = agent.marketplace.subscribe_to_results(&[100], &[]).await?;
    let mut feedback = agent.marketplace.subscribe_to_feedback().await?;

    // Submit a translation job to the provider
    let request_id = agent
        .marketplace
        .submit_job_request(
            100, // kind:5100
            "Hello, how are you?",
            "text",
            Some("text/plain"),
            Some(1000), // bid 1 sat
            Some(&provider.pubkey),
            vec!["en-to-es".into()],
        )
        .await?;

    println!("Submitted job request: {}", request_id);
    println!("Waiting for result...");

    // Wait for feedback and results with a timeout
    let timeout = tokio::time::sleep(std::time::Duration::from_secs(60));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            Some(fb) = feedback.recv() => {
                println!("Feedback: status={}, info={:?}", fb.status, fb.extra_info);

                // Handle payment-required feedback with LDK
                if fb.status == "payment-required" {
                    if let Some(invoice) = &fb.payment_request {
                        if let Some(ref payments) = agent.payments {
                            match payments.pay(invoice) {
                                Ok(result) => println!("Payment sent: {:?}", result),
                                Err(e) => println!("Payment failed: {}", e),
                            }
                        }
                    }
                }
            }
            Some(result) = results.recv() => {
                println!("\n>>> Result from {}: {}", result.provider.to_hex(), result.content);
                println!("Job completed successfully!");
                break;
            }
            _ = &mut timeout => {
                println!("Timeout waiting for result.");
                break;
            }
        }
    }

    Ok(())
}
