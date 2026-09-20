//! Vanity address grinder for the flow-router program.
//! Finds a keypair whose public key starts with "FLOW" in base58.
//!
//! Run: cargo run --release --bin vanity

use solana_program::pubkey::Pubkey;
use std::time::Instant;

fn main() {
    let prefix = "FLOW";
    eprintln!("Grinding for prefix: {prefix}...");
    eprintln!("(4-char base58 prefix ≈ ~5M attempts, ~30-60s on release build)\n");

    let start = Instant::now();
    let mut attempts: u64 = 0;
    let mut rng = rand::thread_rng();

    loop {
        // Generate random 32 bytes, derive ed25519 keypair
        let mut seed = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rng, &mut seed);
        let keypair = ed25519_dalek::SigningKey::from_bytes(&seed);
        let pubkey_bytes = keypair.verifying_key().to_bytes();
        let pubkey = Pubkey::new_from_array(pubkey_bytes);
        let b58 = pubkey.to_string();

        attempts += 1;

        let b58_lower = b58[..4.min(b58.len())].to_lowercase();
        if b58_lower.starts_with("flow") {
            let elapsed = start.elapsed();

            eprintln!("FOUND after {attempts} attempts in {elapsed:.1?}!");
            eprintln!("Public key:  {b58}");

            // Output as JSON byte array (solana keypair format: 32 secret + 32 public = 64 bytes)
            let full_key: Vec<u8> = seed.iter().chain(pubkey_bytes.iter()).copied().collect();
            println!("{}", serde_json::to_string(&full_key).unwrap_or_default());

            eprintln!("\nSave to file:");
            eprintln!("  cargo run --release --bin vanity --features vanity > flow-router-keypair.json");
            eprintln!("  solana program deploy --program-id flow-router-keypair.json target/deploy/flow_router.so");
            break;
        }

        if attempts % 1_000_000 == 0 {
            let rate = attempts as f64 / start.elapsed().as_secs_f64();
            eprintln!("  {attempts} attempts ({rate:.0}/sec)...");
        }
    }
}
