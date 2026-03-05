use elisym_core::*;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Use a fixed identity so customer can find us
    let identity = AgentIdentity::from_secret_key(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )?;
    println!("Provider pubkey: {}", identity.npub());
    println!("Provider hex:    {}", identity.public_key().to_hex());

    let agent = AgentNodeBuilder::new(
        "translation-agent",
        "AI agent that translates text between languages",
    )
    // ATTN: Testnet-only hardcoded key — do NOT use on mainnet!
    // Pubkey: npub1dgz2hxxeu3m54kqxuvpdmh4k804pddwttu3raem50r5xrw6c86esxd0p6w
    .secret_key("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    .capabilities(vec!["translation".into(), "text-processing".into()])
    .supported_job_kinds(vec![5100])
    .build()
    .await?;

    println!("Provider started: {}", agent.identity.npub());
    println!(
        "Capabilities: {:?}",
        agent.capability_card.capabilities
    );

    // Subscribe to incoming job requests (kind:5100)
    let mut jobs = agent
        .marketplace
        .subscribe_to_job_requests(&[100])
        .await?;

    println!("Waiting for job requests...");

    while let Some(job) = jobs.recv().await {
        println!(
            "\n>>> Received job {} from {}: {}",
            job.event_id, job.customer, job.input_data
        );

        // Send processing feedback
        agent
            .marketplace
            .submit_job_feedback(&job.raw_event, JobStatus::Processing, None, None, None, None)
            .await?;

        // Simulate translation
        let result_text = format!("Traducción: '{}' → 'Hola, ¿cómo estás?'", job.input_data);

        // Submit result
        agent
            .marketplace
            .submit_job_result(
                &job.raw_event,
                &result_text,
                Some(1000), // 1 sat
            )
            .await?;

        println!("<<< Result sent for job {}", job.event_id);
    }

    Ok(())
}
