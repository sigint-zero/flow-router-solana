use solana_program::pubkey::Pubkey;

/// Instruction discriminators.
pub const SWAP_DISC: u8 = 0;
// 1 = reserved (was swap_multi, replaced by generic N-hop swap)
pub const INIT_CONFIG_DISC: u8 = 2;
// 3 = reserved (was update_config, removed for immutability)
pub const ADD_INTEGRATOR_DISC: u8 = 4;
pub const REMOVE_INTEGRATOR_DISC: u8 = 5;

/// PDA seeds for the router config account.
pub const CONFIG_SEED: &[u8] = b"config";
/// PDA seed prefix for integrator whitelist accounts.
pub const INTEGRATOR_SEED: &[u8] = b"integrator";
/// Discriminator stored in the first 8 bytes of the config account.
pub const CONFIG_DISCRIMINATOR: [u8; 8] = *b"flowconf";
/// Discriminator for integrator PDA accounts.
pub const INTEGRATOR_DISCRIMINATOR: [u8; 8] = *b"flowintg";
/// Total on-chain size: discriminator(8) + admin(32) + fee_bps(2) + treasury_wallet(32) + referral_split_bps(2) = 76
pub const CONFIG_SIZE: usize = 76;
/// Integrator PDA size: discriminator(8) + wallet(32) = 40
pub const INTEGRATOR_SIZE: usize = 40;

/// Maximum number of hops in a single swap instruction.
pub const MAX_HOPS: usize = 5;

/// A single hop in the route plan.
#[derive(Debug)]
pub struct Hop {
    /// The DEX program to CPI into for this leg.
    pub dex_program: Pubkey,
    /// Raw instruction data for the DEX swap instruction.
    pub dex_data: Vec<u8>,
    /// Number of accounts that belong to this hop's CPI.
    pub dex_account_count: u8,
}

/// Generic N-hop swap via sequential CPI.
///
/// Account layout:
///   [0]                payer              (signer)
///   [1..1+N+1]         token_accounts     input, intermediate_1..N-1, output  (all writable)
///   [1+N+1]            config_pda         (read-only)
///   [1+N+2]            protocol_fee_acct  (writable)
///   [1+N+3]            referral_acct      (writable, or program_id to skip)
///   [1+N+4]            token_program      (read-only)
///   [1+N+5]            output_mint        (read-only) mint of the output token account
///   [1+N+6..]          remaining          all DEX accounts for all hops (concatenated)
///
/// For N=1 (single-hop): [payer, input, output, config, fee, referral, token_prog, out_mint, ...dex]
/// For N=2 (2-hop):      [payer, input, inter, output, config, fee, referral, token_prog, out_mint, ...dex1, ...dex2]
/// For N=3 (3-hop):      [payer, input, inter1, inter2, output, config, fee, referral, token_prog, out_mint, ...dex1, ...dex2, ...dex3]
///
/// Fee is always taken from the output token after all hops complete and slippage is verified.
/// It is moved with `TransferChecked` (hence `output_mint`), which works for SPL Token and for
/// Token-2022 mints with extensions (transfer-fee). Instruction DATA is unchanged.
#[derive(Debug)]
pub struct SwapArgs {
    /// Route plan: 1..MAX_HOPS legs executed sequentially.
    pub hops: Vec<Hop>,
    /// Input amount in smallest token units.
    pub amount_in: u64,
    /// Minimum final output amount (slippage protection).
    pub min_amount_out: u64,
}

/// Data for initialize_config instruction.
///
/// Accounts:
///   [0] admin (signer, writable) — pays for account creation
///   [1] config_pda (writable) — the PDA to initialize
///   [2] system_program
///
/// Data: [2u8] + admin(32) + fee_bps(u16) + treasury_wallet(32) + referral_split_bps(u16)
#[derive(Debug)]
pub struct InitConfigArgs {
    pub admin: Pubkey,
    pub fee_bps: u16,
    pub treasury_wallet: Pubkey,
    pub referral_split_bps: u16,
}

// ── Manual deserialization (no borsh dependency) ──

fn read_pubkey(data: &[u8], offset: &mut usize) -> Option<Pubkey> {
    if *offset + 32 > data.len() { return None; }
    let pk = Pubkey::new_from_array(data[*offset..*offset + 32].try_into().ok()?);
    *offset += 32;
    Some(pk)
}
fn read_u8(data: &[u8], offset: &mut usize) -> Option<u8> {
    if *offset >= data.len() { return None; }
    let v = data[*offset]; *offset += 1; Some(v)
}
fn read_u16(data: &[u8], offset: &mut usize) -> Option<u16> {
    if *offset + 2 > data.len() { return None; }
    let v = u16::from_le_bytes(data[*offset..*offset + 2].try_into().ok()?);
    *offset += 2; Some(v)
}
fn read_u64(data: &[u8], offset: &mut usize) -> Option<u64> {
    if *offset + 8 > data.len() { return None; }
    let v = u64::from_le_bytes(data[*offset..*offset + 8].try_into().ok()?);
    *offset += 8; Some(v)
}
fn read_vec(data: &[u8], offset: &mut usize) -> Option<Vec<u8>> {
    if *offset + 4 > data.len() { return None; }
    let len = u32::from_le_bytes(data[*offset..*offset + 4].try_into().ok()?) as usize;
    *offset += 4;
    if len > data.len().saturating_sub(*offset) { return None; }
    let v = data[*offset..*offset + len].to_vec();
    *offset += len; Some(v)
}

impl SwapArgs {
    /// Parse swap instruction data.
    ///
    /// Wire format: num_hops(u8) + [hop × N] + amount_in(u64) + min_amount_out(u64)
    /// Each hop:    dex_program(32) + dex_data(vec) + dex_account_count(u8)
    pub fn parse(data: &[u8]) -> Option<Self> {
        let mut off = 0;
        let num_hops = read_u8(data, &mut off)? as usize;
        if num_hops == 0 || num_hops > MAX_HOPS {
            return None;
        }
        let mut hops = Vec::with_capacity(num_hops);
        for _ in 0..num_hops {
            hops.push(Hop {
                dex_program: read_pubkey(data, &mut off)?,
                dex_data: read_vec(data, &mut off)?,
                dex_account_count: read_u8(data, &mut off)?,
            });
        }
        Some(Self {
            hops,
            amount_in: read_u64(data, &mut off)?,
            min_amount_out: read_u64(data, &mut off)?,
        })
    }
}

impl InitConfigArgs {
    pub fn parse(data: &[u8]) -> Option<Self> {
        let mut off = 0;
        Some(Self {
            admin: read_pubkey(data, &mut off)?,
            fee_bps: read_u16(data, &mut off)?,
            treasury_wallet: read_pubkey(data, &mut off)?,
            referral_split_bps: read_u16(data, &mut off)?,
        })
    }
}
