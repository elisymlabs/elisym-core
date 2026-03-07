use elisym_core::*;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Agent A — sender
    let agent_a = AgentNodeBuilder::new("agent-alpha", "Sender agent")
        // ATTN: Testnet-only hardcoded key — do NOT use on mainnet!
        // Pubkey: npub1dgz2hxxeu3m54kqxuvpdmh4k804pddwttu3raem50r5xrw6c86esxd0p6w
        .secret_key("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        .capabilities(vec!["messaging-test".into()])
        .build()
        .await?;

    // Agent B — receiver
    let agent_b = AgentNodeBuilder::new("agent-beta", "Receiver agent")
        // ATTN: Testnet-only hardcoded key — do NOT use on mainnet!
        // Pubkey: npub1dp5qwd78dk4msqwtygz02ld7fezhne8hzrxk0hqmggn4jtypax6szwtzka
        .secret_key("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        .capabilities(vec!["messaging-test".into()])
        .build()
        .await?;

    println!("Agent A: {}", agent_a.identity.npub());
    println!("Agent B: {}", agent_b.identity.npub());

    // Agent B subscribes to incoming private messages
    let mut inbox = agent_b.messaging.subscribe_to_messages().await?;

    // Give subscription time to propagate to relays
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Agent A sends a private NIP-17 message to Agent B
    println!("\n--- Agent A sending NIP-17 private message to Agent B ---");
    agent_a
        .messaging
        .send_message(&agent_b.identity.public_key(), "Hello Agent B! This is a secret message.")
        .await?;
    println!("Message sent!");

    // Also test structured JSON message
    #[derive(serde::Serialize)]
    struct TaskProposal {
        task: String,
        bid: u64,
    }

    println!("\n--- Agent A sending structured JSON message to Agent B ---");
    agent_a
        .messaging
        .send_structured_message(
            &agent_b.identity.public_key(),
            &TaskProposal {
                task: "Translate 'hello world' to Spanish".into(),
                bid: 5000,
            },
        )
        .await?;
    println!("Structured message sent!");

    // Wait for Agent B to receive messages
    println!("\n--- Agent B waiting for messages ---");

    let timeout = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(timeout);

    let mut received = 0;
    loop {
        tokio::select! {
            Some(msg) = inbox.recv() => {
                received += 1;
                println!(
                    "\n[Message #{}] From: {}\n  Content: {}\n  Timestamp: {}",
                    received,
                    msg.sender.to_hex(),
                    msg.content,
                    msg.timestamp
                );
                if received >= 2 {
                    println!("\nAll messages received!");
                    break;
                }
            }
            _ = &mut timeout => {
                println!("\nTimeout. Received {} of 2 messages.", received);
                break;
            }
        }
    }

    Ok(())
}
