//! Integration tests for fee collection on the router's output token.
//!
//! The router runs NATIVELY (`processor!`) inside `solana-program-test`; the token programs are
//! the real SPL Token 3.5.0 / Token-2022 8.0.0 BPF binaries bundled with `solana-program-test`.
//!
//! The "DEX hop" is a token-program `TransferChecked` from a pool-owned token account to the
//! user's output account, authorised by a pool-authority keypair that co-signs the transaction.
//! The router forwards the `is_signer` flag of the accounts it is handed into the hop CPI, so this
//! is a faithful stand-in for a swap leg that delivers output tokens.
//!
//! NOTE: in native mode the router's own `msg!` lines go to stdout (solana-msg `println!`), NOT
//! into the transaction log, so assertions are on error codes and token balances; the returned
//! transaction logs (token-program lines) are only used for failure diagnostics.
//!
//! All token instructions are hand-encoded (no spl-token dev-dependency) — the bundled token
//! programs are the judge of whether the encodings are right.
#![allow(deprecated)] // solana_sdk::system_instruction re-export

use solana_program_test::{processor, ProgramTest, ProgramTestContext};
use solana_sdk::{
    instruction::{AccountMeta, Instruction, InstructionError},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction, system_program,
    transaction::{Transaction, TransactionError},
};

const SPL_TOKEN: Pubkey = solana_sdk::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN_2022: Pubkey = solana_sdk::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

// Router error codes (src/error.rs)
const ERR_SLIPPAGE_EXCEEDED: u32 = 1;
const ERR_INVALID_ACCOUNT: u32 = 6;

// Router config used by every test
const ROUTER_FEE_BPS: u16 = 100; // 1%
const REFERRAL_SPLIT_BPS: u16 = 3000; // 30% of the fee to the referrer

// Transfer-fee extension config of the output mint
const MINT_TRANSFER_FEE_BPS: u16 = 250; // 2.5%
const MINT_TRANSFER_FEE_MAX: u64 = 1_000_000_000;

const DECIMALS: u8 = 6;
const POOL_SUPPLY: u64 = 1_000_000_000;
const HOP_AMOUNT: u64 = 1_000_000; // what the "DEX" sends to the user

// Account sizes. Base mint = 82, base token account = 165. With extensions the mint is padded to
// 165, then 1 byte account-type, then TLV entries (2 type + 2 len + value).
const MINT_LEN: usize = 82;
const ACCOUNT_LEN: usize = 165;
const MINT_LEN_TRANSFER_FEE: usize = 165 + 1 + 4 + 108; // TransferFeeConfig = 108 bytes
const ACCOUNT_LEN_TRANSFER_FEE: usize = 165 + 1 + 4 + 8; // TransferFeeAmount = 8 bytes
const MINT_LEN_TRANSFER_HOOK: usize = 165 + 1 + 4 + 64; // TransferHook = 64 bytes
const ACCOUNT_LEN_TRANSFER_HOOK: usize = 165 + 1 + 4 + 1; // TransferHookAccount = 1 byte

#[derive(Clone, Copy, PartialEq)]
enum MintKind {
    Plain,
    TransferFee,
    /// Transfer-hook extension pointing at the given hook program.
    TransferHook(Pubkey),
}

impl MintKind {
    fn mint_len(&self) -> usize {
        match self {
            MintKind::Plain => MINT_LEN,
            MintKind::TransferFee => MINT_LEN_TRANSFER_FEE,
            MintKind::TransferHook(_) => MINT_LEN_TRANSFER_HOOK,
        }
    }
    fn account_len(&self) -> usize {
        match self {
            MintKind::Plain => ACCOUNT_LEN,
            MintKind::TransferFee => ACCOUNT_LEN_TRANSFER_FEE,
            MintKind::TransferHook(_) => ACCOUNT_LEN_TRANSFER_HOOK,
        }
    }
}

/// The transfer-fee extension's fee for one transfer: ceil(amount * bps / 10_000), capped.
fn mint_transfer_fee(kind: MintKind, amount: u64) -> u64 {
    if kind != MintKind::TransferFee {
        return 0;
    }
    let fee = (amount as u128 * MINT_TRANSFER_FEE_BPS as u128).div_ceil(10_000) as u64;
    fee.min(MINT_TRANSFER_FEE_MAX)
}

// ── hand-encoded token instructions ──

fn ix_init_transfer_fee_config(mint: &Pubkey, authority: &Pubkey) -> Instruction {
    // TransferFeeExtension (26) / InitializeTransferFeeConfig (0)
    let mut data = vec![26u8, 0u8];
    for _ in 0..2 {
        data.push(1); // COption::Some
        data.extend_from_slice(authority.as_ref());
    }
    data.extend_from_slice(&MINT_TRANSFER_FEE_BPS.to_le_bytes());
    data.extend_from_slice(&MINT_TRANSFER_FEE_MAX.to_le_bytes());
    Instruction { program_id: TOKEN_2022, accounts: vec![AccountMeta::new(*mint, false)], data }
}

fn ix_init_transfer_hook(mint: &Pubkey, hook_program: &Pubkey) -> Instruction {
    // TransferHookExtension (36) / Initialize (0): authority (zero = none) + hook program id
    let mut data = vec![36u8, 0u8];
    data.extend_from_slice(&[0u8; 32]);
    data.extend_from_slice(hook_program.as_ref());
    Instruction { program_id: TOKEN_2022, accounts: vec![AccountMeta::new(*mint, false)], data }
}

fn ix_init_mint2(token_program: &Pubkey, mint: &Pubkey, authority: &Pubkey) -> Instruction {
    let mut data = vec![20u8, DECIMALS];
    data.extend_from_slice(authority.as_ref());
    data.push(0); // no freeze authority
    Instruction { program_id: *token_program, accounts: vec![AccountMeta::new(*mint, false)], data }
}

fn ix_init_account3(token_program: &Pubkey, account: &Pubkey, mint: &Pubkey, owner: &Pubkey) -> Instruction {
    let mut data = vec![18u8];
    data.extend_from_slice(owner.as_ref());
    Instruction {
        program_id: *token_program,
        accounts: vec![AccountMeta::new(*account, false), AccountMeta::new_readonly(*mint, false)],
        data,
    }
}

fn ix_mint_to(token_program: &Pubkey, mint: &Pubkey, dest: &Pubkey, authority: &Pubkey, amount: u64) -> Instruction {
    let mut data = vec![7u8];
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: *token_program,
        accounts: vec![
            AccountMeta::new(*mint, false),
            AccountMeta::new(*dest, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

fn ix_transfer_checked(
    token_program: &Pubkey, source: &Pubkey, mint: &Pubkey, dest: &Pubkey, authority: &Pubkey, amount: u64,
) -> Instruction {
    let mut data = vec![12u8];
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(DECIMALS);
    Instruction {
        program_id: *token_program,
        accounts: vec![
            AccountMeta::new(*source, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*dest, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

// ── router instruction builders (mirror flow-trades `wrap_swap`, NEW layout) ──

fn config_pda(router: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"config"], router).0
}

fn integrator_pda(router: &Pubkey, wallet: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"integrator", wallet.as_ref()], router).0
}

fn ix_router_init(router: &Pubkey, admin: &Pubkey, treasury: &Pubkey) -> Instruction {
    ix_router_init_with_fee(router, admin, treasury, ROUTER_FEE_BPS)
}

fn ix_router_init_with_fee(router: &Pubkey, admin: &Pubkey, treasury: &Pubkey, fee_bps: u16) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(admin.as_ref());
    data.extend_from_slice(&fee_bps.to_le_bytes());
    data.extend_from_slice(treasury.as_ref());
    data.extend_from_slice(&REFERRAL_SPLIT_BPS.to_le_bytes());
    Instruction {
        program_id: *router,
        accounts: vec![
            AccountMeta::new(*admin, true),
            AccountMeta::new(config_pda(router), false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        data,
    }
}

fn ix_router_add_integrator(router: &Pubkey, admin: &Pubkey, wallet: &Pubkey) -> Instruction {
    let mut data = vec![4u8];
    data.extend_from_slice(wallet.as_ref());
    Instruction {
        program_id: *router,
        accounts: vec![
            AccountMeta::new(*admin, true),
            AccountMeta::new_readonly(config_pda(router), false),
            AccountMeta::new(integrator_pda(router, wallet), false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        data,
    }
}

/// Single-hop router swap in the NEW account layout:
///   [0] payer(s) [1] input(w) [2] output(w) [3] config [4] protocol_fee(w) [5] referral(w)|router id
///   [6] token_program [7] output_mint  [8..] hop accounts, then hop program id(s), then integrator PDA
#[allow(clippy::too_many_arguments)]
fn ix_router_swap(
    router: &Pubkey,
    payer: &Pubkey,
    input_acct: &Pubkey,
    output_acct: &Pubkey,
    protocol_fee_acct: &Pubkey,
    referral: Option<(&Pubkey, &Pubkey)>, // (referral token account, integrator wallet)
    token_program: &Pubkey,
    output_mint: &Pubkey,
    hop: &Instruction,
    amount_in: u64,
    min_amount_out: u64,
) -> Instruction {
    let mut data = vec![0u8, 1u8]; // SWAP_DISC, num_hops
    data.extend_from_slice(hop.program_id.as_ref());
    data.extend_from_slice(&(hop.data.len() as u32).to_le_bytes());
    data.extend_from_slice(&hop.data);
    data.push(hop.accounts.len() as u8);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_amount_out.to_le_bytes());

    let mut accounts = vec![
        AccountMeta::new_readonly(*payer, true),
        AccountMeta::new(*input_acct, false),
        AccountMeta::new(*output_acct, false),
        AccountMeta::new_readonly(config_pda(router), false),
        AccountMeta::new(*protocol_fee_acct, false),
        AccountMeta::new(referral.map(|r| *r.0).unwrap_or(*router), false),
        AccountMeta::new_readonly(*token_program, false),
        AccountMeta::new_readonly(*output_mint, false),
    ];
    accounts.extend(hop.accounts.iter().cloned());
    accounts.push(AccountMeta::new_readonly(hop.program_id, false));
    if let Some((_, wallet)) = referral {
        accounts.push(AccountMeta::new_readonly(integrator_pda(router, wallet), false));
    }
    Instruction { program_id: *router, accounts, data }
}

// ── harness ──

struct Env {
    ctx: ProgramTestContext,
    router: Pubkey,
    token_program: Pubkey,
    kind: MintKind,
    out_mint: Pubkey,
    /// A second, valid mint on the same token program (the user's INPUT token).
    in_mint: Pubkey,
    user: Keypair,
    pool_authority: Keypair,
    referrer: Pubkey,
    user_in: Pubkey,
    user_out: Pubkey,
    pool_out: Pubkey,
    treasury_fee: Pubkey,
    referral_fee: Pubkey,
}

async fn send(ctx: &mut ProgramTestContext, ixs: &[Instruction], extra: &[&Keypair]) -> Result<Vec<String>, (TransactionError, Vec<String>)> {
    let bh = ctx.banks_client.get_latest_blockhash().await.unwrap();
    let mut signers: Vec<&Keypair> = vec![&ctx.payer];
    signers.extend_from_slice(extra);
    let tx = Transaction::new_signed_with_payer(ixs, Some(&ctx.payer.pubkey()), &signers, bh);
    let res = ctx.banks_client.process_transaction_with_metadata(tx).await.unwrap();
    let logs = res.metadata.map(|m| m.log_messages).unwrap_or_default();
    match res.result {
        Ok(()) => Ok(logs),
        Err(e) => Err((e, logs)),
    }
}

async fn create_mint(ctx: &mut ProgramTestContext, token_program: &Pubkey, kind: MintKind) -> Pubkey {
    let mint = Keypair::new();
    let payer = ctx.payer.pubkey();
    let rent = ctx.banks_client.get_rent().await.unwrap();
    let len = kind.mint_len();
    let mut ixs = vec![system_instruction::create_account(
        &payer, &mint.pubkey(), rent.minimum_balance(len), len as u64, token_program,
    )];
    match kind {
        MintKind::Plain => {}
        MintKind::TransferFee => ixs.push(ix_init_transfer_fee_config(&mint.pubkey(), &payer)),
        MintKind::TransferHook(hook) => ixs.push(ix_init_transfer_hook(&mint.pubkey(), &hook)),
    }
    ixs.push(ix_init_mint2(token_program, &mint.pubkey(), &payer));
    send(ctx, &ixs, &[&mint]).await.expect("create mint");
    mint.pubkey()
}

async fn create_token_account(
    ctx: &mut ProgramTestContext, token_program: &Pubkey, kind: MintKind, mint: &Pubkey, owner: &Pubkey,
) -> Pubkey {
    let acct = Keypair::new();
    let payer = ctx.payer.pubkey();
    let rent = ctx.banks_client.get_rent().await.unwrap();
    let len = kind.account_len();
    let ixs = [
        system_instruction::create_account(&payer, &acct.pubkey(), rent.minimum_balance(len), len as u64, token_program),
        ix_init_account3(token_program, &acct.pubkey(), mint, owner),
    ];
    send(ctx, &ixs, &[&acct]).await.expect("create token account");
    acct.pubkey()
}

async fn token_amount(ctx: &mut ProgramTestContext, acct: &Pubkey) -> u64 {
    let a = ctx.banks_client.get_account(*acct).await.unwrap().expect("token account exists");
    u64::from_le_bytes(a.data[64..72].try_into().unwrap())
}

/// Withheld transfer fee stored in a Token-2022 account's TransferFeeAmount extension.
async fn withheld_amount(ctx: &mut ProgramTestContext, acct: &Pubkey) -> u64 {
    let a = ctx.banks_client.get_account(*acct).await.unwrap().expect("token account exists");
    assert_eq!(a.data.len(), ACCOUNT_LEN_TRANSFER_FEE);
    assert_eq!(a.data[165], 2, "account-type byte = Account");
    assert_eq!(u16::from_le_bytes(a.data[166..168].try_into().unwrap()), 2, "TLV type = TransferFeeAmount");
    assert_eq!(u16::from_le_bytes(a.data[168..170].try_into().unwrap()), 8, "TLV len");
    u64::from_le_bytes(a.data[170..178].try_into().unwrap())
}

async fn setup(token_program: Pubkey, kind: MintKind) -> Env {
    setup_with_fee(token_program, kind, ROUTER_FEE_BPS).await
}

async fn setup_with_fee(token_program: Pubkey, kind: MintKind, fee_bps: u16) -> Env {
    let router = Pubkey::new_unique();
    let pt = ProgramTest::new(
        "flow_router",
        router,
        processor!(flow_router::processor::process_instruction),
    );
    let mut ctx = pt.start_with_context().await;
    let admin = ctx.payer.pubkey();

    let user = Keypair::new();
    let pool_authority = Keypair::new();
    let treasury = Pubkey::new_unique();
    let referrer = Pubkey::new_unique();

    let out_mint = create_mint(&mut ctx, &token_program, kind).await;
    let in_mint = create_mint(&mut ctx, &token_program, MintKind::Plain).await;

    let user_in = create_token_account(&mut ctx, &token_program, MintKind::Plain, &in_mint, &user.pubkey()).await;
    let user_out = create_token_account(&mut ctx, &token_program, kind, &out_mint, &user.pubkey()).await;
    let pool_out = create_token_account(&mut ctx, &token_program, kind, &out_mint, &pool_authority.pubkey()).await;
    let treasury_fee = create_token_account(&mut ctx, &token_program, kind, &out_mint, &treasury).await;
    let referral_fee = create_token_account(&mut ctx, &token_program, kind, &out_mint, &referrer).await;

    send(&mut ctx, &[ix_mint_to(&token_program, &out_mint, &pool_out, &admin, POOL_SUPPLY)], &[])
        .await
        .expect("fund pool");
    send(&mut ctx, &[ix_router_init_with_fee(&router, &admin, &treasury, fee_bps)], &[]).await.expect("router init_config");

    Env {
        ctx, router, token_program, kind, out_mint, in_mint, user, pool_authority, referrer,
        user_in, user_out, pool_out, treasury_fee, referral_fee,
    }
}

impl Env {
    /// The stand-in DEX leg: pool → user TransferChecked, signed by the pool authority.
    fn hop(&self) -> Instruction {
        ix_transfer_checked(
            &self.token_program, &self.pool_out, &self.out_mint, &self.user_out,
            &self.pool_authority.pubkey(), HOP_AMOUNT,
        )
    }

    fn swap_ix(&self, mint_passed: &Pubkey, with_referral: bool, min_out: u64) -> Instruction {
        let hop = self.hop();
        ix_router_swap(
            &self.router, &self.user.pubkey(), &self.user_in, &self.user_out, &self.treasury_fee,
            with_referral.then_some((&self.referral_fee, &self.referrer)),
            &self.token_program, mint_passed, &hop, 5_000_000, min_out,
        )
    }

    async fn swap(&mut self, mint_passed: Pubkey, with_referral: bool, min_out: u64) -> Result<Vec<String>, (TransactionError, Vec<String>)> {
        let ix = self.swap_ix(&mint_passed, with_referral, min_out);
        let user = self.user.insecure_clone();
        let pool = self.pool_authority.insecure_clone();
        send(&mut self.ctx, &[ix], &[&user, &pool]).await
    }

    /// What the user's output account gains from the hop (gross of the router fee).
    fn expected_received(&self) -> u64 {
        HOP_AMOUNT - mint_transfer_fee(self.kind, HOP_AMOUNT)
    }
}

fn custom(code: u32) -> TransactionError {
    TransactionError::InstructionError(0, InstructionError::Custom(code))
}

fn count_log(logs: &[String], needle: &str) -> usize {
    logs.iter().filter(|l| l.contains(needle)).count()
}

fn dump(logs: &[String]) -> String {
    logs.join("\n")
}

/// Happy path + slippage revert, shared by the SPL Token and Token-2022 variants.
async fn run_swap_checks(token_program: Pubkey, kind: MintKind) {
    let mut env = setup(token_program, kind).await;
    let received = env.expected_received();
    let router_fee = received * ROUTER_FEE_BPS as u64 / 10_000;
    assert!(router_fee > 0);

    // 1) Slippage revert FIRST (state must be untouched by it): min_out one unit above received.
    let (err, logs) = env.swap(env.out_mint, false, received + 1).await.expect_err("slippage must revert");
    assert_eq!(err, custom(ERR_SLIPPAGE_EXCEEDED), "logs:\n{}", dump(&logs));
    assert_eq!(token_amount(&mut env.ctx, &env.user_out).await, 0);
    assert_eq!(token_amount(&mut env.ctx, &env.treasury_fee).await, 0);
    assert_eq!(token_amount(&mut env.ctx, &env.pool_out).await, POOL_SUPPLY);

    // 2) Success with min_out EXACTLY equal to the gross received amount: proves the slippage
    //    check is on the pre-router-fee amount (user net is below min_out and that is fine).
    let logs = env.swap(env.out_mint, false, received).await.unwrap_or_else(|(e, l)| {
        panic!("swap failed: {e:?}\nlogs:\n{}", dump(&l))
    });
    // Token-program log lines: the hop + ONE fee transfer, both TransferChecked.
    assert_eq!(count_log(&logs, "Instruction: TransferChecked"), 2, "logs:\n{}", dump(&logs));

    let fee_on_fee = mint_transfer_fee(kind, router_fee);
    let user_bal = token_amount(&mut env.ctx, &env.user_out).await;
    let treasury_bal = token_amount(&mut env.ctx, &env.treasury_fee).await;
    assert_eq!(user_bal, received - router_fee, "user net = received - router fee");
    assert_eq!(treasury_bal, router_fee - fee_on_fee, "treasury = fee share minus the mint's transfer fee");
    assert!(treasury_bal > 0);
    assert_eq!(token_amount(&mut env.ctx, &env.pool_out).await, POOL_SUPPLY - HOP_AMOUNT);
    assert_eq!(token_amount(&mut env.ctx, &env.referral_fee).await, 0);

    if kind == MintKind::TransferFee {
        assert!(fee_on_fee > 0, "test must actually exercise the transfer fee");
        // Conservation: what left the user's account is either in the treasury or withheld there.
        assert_eq!(withheld_amount(&mut env.ctx, &env.treasury_fee).await, fee_on_fee);
        assert_eq!(withheld_amount(&mut env.ctx, &env.user_out).await, mint_transfer_fee(kind, HOP_AMOUNT));
    }
}

#[tokio::test]
async fn token2022_transfer_fee_mint_swap_collects_fee() {
    run_swap_checks(TOKEN_2022, MintKind::TransferFee).await;
}

#[tokio::test]
async fn token2022_plain_mint_swap_collects_fee() {
    run_swap_checks(TOKEN_2022, MintKind::Plain).await;
}

#[tokio::test]
async fn spl_token_mint_swap_collects_fee_regression() {
    run_swap_checks(SPL_TOKEN, MintKind::Plain).await;
}

/// Referral share also goes through TransferChecked (whitelisted integrator).
#[tokio::test]
async fn token2022_transfer_fee_mint_with_referral_split() {
    let mut env = setup(TOKEN_2022, MintKind::TransferFee).await;
    let admin = env.ctx.payer.pubkey();
    let ix = ix_router_add_integrator(&env.router, &admin, &env.referrer);
    send(&mut env.ctx, &[ix], &[]).await.expect("add integrator");

    let received = env.expected_received();
    let fee = received * ROUTER_FEE_BPS as u64 / 10_000;
    let referral_share = fee * REFERRAL_SPLIT_BPS as u64 / 10_000;
    let protocol_share = fee - referral_share;

    let logs = env.swap(env.out_mint, true, received).await.unwrap_or_else(|(e, l)| {
        panic!("swap failed: {e:?}\nlogs:\n{}", dump(&l))
    });
    // hop + protocol share + referral share, all TransferChecked.
    assert_eq!(count_log(&logs, "Instruction: TransferChecked"), 3, "logs:\n{}", dump(&logs));
    assert_eq!(token_amount(&mut env.ctx, &env.user_out).await, received - fee);
    assert_eq!(
        token_amount(&mut env.ctx, &env.treasury_fee).await,
        protocol_share - mint_transfer_fee(env.kind, protocol_share)
    );
    assert_eq!(
        token_amount(&mut env.ctx, &env.referral_fee).await,
        referral_share - mint_transfer_fee(env.kind, referral_share)
    );
}

/// A mint that is a real mint of the right token program but NOT the output account's mint.
#[tokio::test]
async fn wrong_mint_same_program_rejected_invalid_account() {
    for (program, kind) in [(TOKEN_2022, MintKind::TransferFee), (SPL_TOKEN, MintKind::Plain)] {
        let mut env = setup(program, kind).await;
        let (err, logs) = env.swap(env.in_mint, false, 0).await.expect_err("wrong mint must be rejected");
        assert_eq!(err, custom(ERR_INVALID_ACCOUNT), "logs:\n{}", dump(&logs));
            // Rejected before any hop ran.
        assert_eq!(token_amount(&mut env.ctx, &env.pool_out).await, POOL_SUPPLY);
        assert_eq!(token_amount(&mut env.ctx, &env.user_out).await, 0);
    }
}

/// An account at the mint slot that is not owned by the passed token program.
#[tokio::test]
async fn mint_not_owned_by_token_program_rejected_invalid_account() {
    let mut env = setup(TOKEN_2022, MintKind::TransferFee).await;
    // A genuine SPL Token (v1) mint — wrong owner for a Token-2022 swap.
    let foreign_mint = create_mint(&mut env.ctx, &SPL_TOKEN, MintKind::Plain).await;
    let (err, logs) = env.swap(foreign_mint, false, 0).await.expect_err("foreign mint must be rejected");
    assert_eq!(err, custom(ERR_INVALID_ACCOUNT), "logs:\n{}", dump(&logs));

    // The config PDA (router-owned) at the mint slot is rejected the same way.
    let cfg = config_pda(&env.router);
    let (err, _) = env.swap(cfg, false, 0).await.expect_err("non-mint must be rejected");
    assert_eq!(err, custom(ERR_INVALID_ACCOUNT));
    assert_eq!(token_amount(&mut env.ctx, &env.pool_out).await, POOL_SUPPLY);
}

/// The mint account is mandatory: the OLD account layout (no mint) no longer parses as a swap.
#[tokio::test]
async fn old_layout_without_mint_is_rejected() {
    let mut env = setup(TOKEN_2022, MintKind::TransferFee).await;
    let mut ix = env.swap_ix(&env.out_mint, false, 0);
    ix.accounts.remove(7); // drop output_mint → old layout; pool_out slides into the mint slot
    let user = env.user.insecure_clone();
    let pool = env.pool_authority.insecure_clone();
    let (err, logs) = send(&mut env.ctx, &[ix], &[&user, &pool]).await.expect_err("old layout must fail");
    assert_eq!(err, custom(ERR_INVALID_ACCOUNT), "logs:\n{}", dump(&logs));
    assert_eq!(token_amount(&mut env.ctx, &env.pool_out).await, POOL_SUPPLY);
}

/// Transfer-hook mints are out of scope: the hook's extra accounts are not forwarded, so the fee
/// TransferChecked fails inside Token-2022 and that error propagates (whole swap reverts).
/// The hop here is a MintTo into the user's account (MintTo does not run the hook), so the FIRST
/// hook-triggering transfer is the router's fee transfer.
#[tokio::test]
async fn transfer_hook_mint_fee_transfer_error_propagates() {
    let hook_program = Pubkey::new_unique();
    let mut env = setup(TOKEN_2022, MintKind::TransferHook(hook_program)).await;
    let admin = env.ctx.payer.pubkey(); // mint authority, already a tx signer
    let hop = ix_mint_to(&TOKEN_2022, &env.out_mint, &env.user_out, &admin, HOP_AMOUNT);
    let ix = ix_router_swap(
        &env.router, &env.user.pubkey(), &env.user_in, &env.user_out, &env.treasury_fee, None,
        &TOKEN_2022, &env.out_mint, &hop, 5_000_000, HOP_AMOUNT,
    );
    let user = env.user.insecure_clone();
    let (err, logs) = send(&mut env.ctx, &[ix], &[&user]).await.expect_err("hook mint must fail at the fee transfer");
    println!("transfer-hook failure: {err:?}\n{}", dump(&logs));

    // The hop ran (MintTo) and the router got as far as the fee TransferChecked …
    assert_eq!(count_log(&logs, "Instruction: MintTo"), 1, "logs:\n{}", dump(&logs));
    assert_eq!(count_log(&logs, "Instruction: TransferChecked"), 1, "logs:\n{}", dump(&logs));
    // … and the failure is an ordinary instruction error of the router instruction (index 0),
    // not one of the router's own error codes being substituted for it.
    match err {
        TransactionError::InstructionError(0, ref inner) => {
            assert_ne!(*inner, InstructionError::Custom(ERR_INVALID_ACCOUNT));
            assert_ne!(*inner, InstructionError::Custom(ERR_SLIPPAGE_EXCEEDED));
        }
        other => panic!("unexpected error {other:?}"),
    }
    // Atomic revert: nothing minted, nothing collected.
    assert_eq!(token_amount(&mut env.ctx, &env.user_out).await, 0);
    assert_eq!(token_amount(&mut env.ctx, &env.treasury_fee).await, 0);
}

/// `fee_bps = 0`: the user keeps the whole output, nothing moves to the treasury,
/// no fee transfer is invoked, and the fee-account slots are not read — a plain
/// system account (the treasury wallet itself) is accepted there, so a client
/// does not have to create fee token accounts for a zero-fee deployment.
#[tokio::test]
async fn zero_fee_config_takes_nothing_and_ignores_fee_accounts() {
    for (token_program, kind) in [(SPL_TOKEN, MintKind::Plain), (TOKEN_2022, MintKind::TransferFee)] {
        let mut env = setup_with_fee(token_program, kind, 0).await;
        let expected = env.expected_received();
        let before_user = token_amount(&mut env.ctx, &env.user_out).await;
        let before_treasury = token_amount(&mut env.ctx, &env.treasury_fee).await;

        // slippage still enforced on the gross amount
        let err = env.swap(env.out_mint, false, expected + 1).await.err().expect("must revert");
        assert_eq!(err.0, custom(ERR_SLIPPAGE_EXCEEDED));

        // fee slot = a non-token account (the treasury wallet, system-owned)
        let treasury_wallet = Pubkey::new_unique();
        let hop = env.hop();
        let ix = ix_router_swap(
            &env.router, &env.user.pubkey(), &env.user_in, &env.user_out, &treasury_wallet, None,
            &env.token_program, &env.out_mint, &hop, 5_000_000, expected,
        );
        let user = env.user.insecure_clone();
        let pool = env.pool_authority.insecure_clone();
        let logs = send(&mut env.ctx, &[ix], &[&user, &pool]).await.expect("zero-fee swap");

        assert_eq!(token_amount(&mut env.ctx, &env.user_out).await - before_user, expected, "user keeps the whole output");
        assert_eq!(token_amount(&mut env.ctx, &env.treasury_fee).await, before_treasury, "treasury untouched");
        assert_eq!(count_log(&logs, "flow-router: fee "), 0, "no fee log");
        // exactly one TransferChecked: the hop itself, none for fees
        assert_eq!(count_log(&logs, "Instruction: TransferChecked"), 1, "{logs:#?}");
    }
}
