// No borsh — manual deserialization to keep binary small.
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    msg,
    program::invoke,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction,
    sysvar::Sysvar,
};

use crate::error::RouterError;
use crate::instruction::{
    InitConfigArgs, SwapArgs,
    CONFIG_DISCRIMINATOR, CONFIG_SEED, CONFIG_SIZE,
    INTEGRATOR_DISCRIMINATOR, INTEGRATOR_SEED, INTEGRATOR_SIZE,
    INIT_CONFIG_DISC, SWAP_DISC,
    ADD_INTEGRATOR_DISC, REMOVE_INTEGRATOR_DISC,
};

/// SPL Token program ID (hardcoded to avoid spl-token dependency).
const SPL_TOKEN_PROGRAM: Pubkey =
    solana_program::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// SPL Token-2022 program ID.
const SPL_TOKEN_2022_PROGRAM: Pubkey =
    solana_program::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// SPL Token `TransferChecked` instruction tag (identical in Token-2022).
const TRANSFER_CHECKED_TAG: u8 = 12;

/// Minimum length of a mint account's base state (SPL Token `Mint::LEN`).
/// Token-2022 mints with extensions are longer, but the base layout is identical.
const MINT_BASE_LEN: usize = 82;

/// Byte offset of `decimals` inside the mint base state:
/// mint_authority COption<Pubkey>(36) + supply u64(8) = 44.
const MINT_DECIMALS_OFFSET: usize = 44;

/// Parsed config values read from the on-chain PDA.
struct ConfigData {
    fee_bps: u16,
    treasury_wallet: Pubkey,
    referral_split_bps: u16,
}

/// Main processor dispatch.
pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    if instruction_data.is_empty() {
        return Err(RouterError::InvalidInstructionData.into());
    }

    match instruction_data[0] {
        SWAP_DISC => process_swap(program_id, accounts, &instruction_data[1..]),
        INIT_CONFIG_DISC => process_init_config(program_id, accounts, &instruction_data[1..]),
        ADD_INTEGRATOR_DISC => process_add_integrator(program_id, accounts, &instruction_data[1..]),
        REMOVE_INTEGRATOR_DISC => process_remove_integrator(program_id, accounts, &instruction_data[1..]),
        _ => Err(RouterError::InvalidInstructionData.into()),
    }
}

// ── Config instructions ──

fn process_init_config(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let args = InitConfigArgs::parse(data)
        .ok_or(ProgramError::from(RouterError::InvalidInstructionData))?;

    if args.fee_bps > 10000 {
        return Err(RouterError::InvalidFeeBps.into());
    }
    if args.referral_split_bps > 10000 {
        return Err(RouterError::InvalidFeeBps.into());
    }

    if accounts.len() < 3 {
        return Err(RouterError::NotEnoughAccounts.into());
    }

    let account_iter = &mut accounts.iter();
    let admin = next_account_info(account_iter)?;
    let config_pda = next_account_info(account_iter)?;
    let system_program = next_account_info(account_iter)?;

    if !admin.is_signer {
        msg!("flow-router: admin must be a signer");
        return Err(ProgramError::MissingRequiredSignature);
    }

    // Admin in instruction data must match the signer — prevents accidental misconfiguration
    if args.admin != *admin.key {
        msg!("flow-router: admin in data ({}) does not match signer ({})", args.admin, admin.key);
        return Err(RouterError::InvalidAccount.into());
    }

    let (expected_pda, bump) =
        Pubkey::find_program_address(&[CONFIG_SEED], program_id);
    if *config_pda.key != expected_pda {
        msg!("flow-router: config PDA mismatch");
        return Err(RouterError::InvalidAccount.into());
    }

    if config_pda.data_len() > 0 {
        msg!("flow-router: config already initialized");
        return Err(RouterError::AlreadyInitialized.into());
    }

    if *system_program.key != solana_program::system_program::id() {
        msg!("flow-router: invalid system program");
        return Err(RouterError::InvalidAccount.into());
    }

    let rent = Rent::get()?;
    let lamports = rent.minimum_balance(CONFIG_SIZE);
    let seeds: &[&[u8]] = &[CONFIG_SEED, &[bump]];

    invoke_signed(
        &system_instruction::create_account(
            admin.key,
            config_pda.key,
            lamports,
            CONFIG_SIZE as u64,
            program_id,
        ),
        &[admin.clone(), config_pda.clone(), system_program.clone()],
        &[seeds],
    )?;

    let mut config_data = config_pda.try_borrow_mut_data()?;
    config_data[..8].copy_from_slice(&CONFIG_DISCRIMINATOR);
    config_data[8..40].copy_from_slice(&args.admin.to_bytes());
    config_data[40..42].copy_from_slice(&args.fee_bps.to_le_bytes());
    config_data[42..74].copy_from_slice(&args.treasury_wallet.to_bytes());
    config_data[74..76].copy_from_slice(&args.referral_split_bps.to_le_bytes());

    msg!(
        "flow-router: config initialized, fee_bps={}, referral_split_bps={}",
        args.fee_bps,
        args.referral_split_bps
    );

    Ok(())
}

// ── Integrator whitelist ──

fn process_add_integrator(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    if data.len() < 32 {
        return Err(RouterError::InvalidInstructionData.into());
    }
    let integrator_wallet = pubkey_from_slice(&data[..32])?;

    if accounts.len() < 4 {
        return Err(RouterError::NotEnoughAccounts.into());
    }

    let account_iter = &mut accounts.iter();
    let admin = next_account_info(account_iter)?;
    let config_pda = next_account_info(account_iter)?;
    let integrator_pda = next_account_info(account_iter)?;
    let system_program = next_account_info(account_iter)?;

    if !admin.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    verify_config_ownership(config_pda, program_id)?;
    let config_data = config_pda.try_borrow_data()?;
    let stored_admin = pubkey_from_slice(&config_data[8..40])?;
    drop(config_data);
    if *admin.key != stored_admin {
        msg!("flow-router: unauthorized — signer is not admin");
        return Err(RouterError::Unauthorized.into());
    }

    let (expected_pda, bump) = Pubkey::find_program_address(
        &[INTEGRATOR_SEED, integrator_wallet.as_ref()],
        program_id,
    );
    if *integrator_pda.key != expected_pda {
        msg!("flow-router: integrator PDA mismatch");
        return Err(RouterError::InvalidAccount.into());
    }
    if integrator_pda.data_len() > 0 {
        msg!("flow-router: integrator already whitelisted");
        return Err(RouterError::AlreadyInitialized.into());
    }

    let rent = Rent::get()?;
    let lamports = rent.minimum_balance(INTEGRATOR_SIZE);
    let seeds: &[&[u8]] = &[INTEGRATOR_SEED, integrator_wallet.as_ref(), &[bump]];

    invoke_signed(
        &system_instruction::create_account(
            admin.key,
            integrator_pda.key,
            lamports,
            INTEGRATOR_SIZE as u64,
            program_id,
        ),
        &[admin.clone(), integrator_pda.clone(), system_program.clone()],
        &[seeds],
    )?;

    let mut pda_data = integrator_pda.try_borrow_mut_data()?;
    pda_data[..8].copy_from_slice(&INTEGRATOR_DISCRIMINATOR);
    pda_data[8..40].copy_from_slice(&integrator_wallet.to_bytes());

    msg!("flow-router: integrator added: {}", integrator_wallet);
    Ok(())
}

fn process_remove_integrator(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    if data.len() < 32 {
        return Err(RouterError::InvalidInstructionData.into());
    }
    let integrator_wallet = pubkey_from_slice(&data[..32])?;

    if accounts.len() < 3 {
        return Err(RouterError::NotEnoughAccounts.into());
    }

    let account_iter = &mut accounts.iter();
    let admin = next_account_info(account_iter)?;
    let config_pda = next_account_info(account_iter)?;
    let integrator_pda = next_account_info(account_iter)?;

    if !admin.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    verify_config_ownership(config_pda, program_id)?;
    let config_data = config_pda.try_borrow_data()?;
    let stored_admin = pubkey_from_slice(&config_data[8..40])?;
    drop(config_data);
    if *admin.key != stored_admin {
        return Err(RouterError::Unauthorized.into());
    }

    let (expected_pda, _) = Pubkey::find_program_address(
        &[INTEGRATOR_SEED, integrator_wallet.as_ref()],
        program_id,
    );
    if *integrator_pda.key != expected_pda {
        return Err(RouterError::InvalidAccount.into());
    }
    if integrator_pda.owner != program_id {
        return Err(RouterError::InvalidAccount.into());
    }

    let lamports = integrator_pda.lamports();
    **integrator_pda.try_borrow_mut_lamports()? = 0;
    **admin.try_borrow_mut_lamports()? = admin.lamports()
        .checked_add(lamports)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let mut pda_data = integrator_pda.try_borrow_mut_data()?;
    for byte in pda_data.iter_mut() {
        *byte = 0;
    }

    msg!("flow-router: integrator removed: {}", integrator_wallet);
    Ok(())
}

// ── Generic N-hop swap ──

/// Process a generic N-hop swap (1 to 5 hops).
///
/// Account layout (for N hops):
///   [0]            payer              (signer)
///   [1..N+2]       token_accounts     [input, inter_1..N-1, output]  (N+1 writable)
///   [N+2]          config_pda         (read-only)
///   [N+3]          protocol_fee_acct  (writable)
///   [N+4]          referral_acct      (writable, or program_id to skip)
///   [N+5]          token_program      (read-only)
///   [N+6]          output_mint        (read-only) mint of the OUTPUT token account
///   [N+7..]        remaining          DEX accounts for all hops (concatenated)
///
/// `output_mint` is required because fees are collected with `TransferChecked`
/// (the only transfer Token-2022 accepts for mints carrying extensions such as
/// transfer-fee — plain `Transfer` fails there with `MintRequiredForTransfer`, 0x1f).
/// It must be the mint recorded in the output token account and be owned by
/// `token_program`; anything else is rejected with `InvalidAccount`.
///
/// Transfer-fee mints: the treasury / referral accounts receive the fee share MINUS
/// the mint's own transfer fee. The slippage check is on the gross amount that
/// arrived in the user's output account and is unaffected.
///
/// Transfer-hook mints are NOT supported: the hook's extra accounts are not
/// forwarded to the fee transfer, so the token program's CPI error propagates and
/// the whole swap reverts.
fn process_swap(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let args = SwapArgs::parse(data)
        .ok_or(ProgramError::from(RouterError::InvalidInstructionData))?;

    let num_hops = args.hops.len();
    let num_token_accounts = num_hops + 1;
    let min_accounts = 1 + num_token_accounts + 5;

    if accounts.len() < min_accounts {
        msg!("flow-router: need {} accounts, got {}", min_accounts, accounts.len());
        return Err(RouterError::NotEnoughAccounts.into());
    }

    let account_iter = &mut accounts.iter();
    let payer = next_account_info(account_iter)?;

    let mut token_accounts = Vec::with_capacity(num_token_accounts);
    for _ in 0..num_token_accounts {
        token_accounts.push(next_account_info(account_iter)?.clone());
    }

    let output_token_acct = &token_accounts[num_token_accounts - 1];

    let config_pda = next_account_info(account_iter)?;
    let protocol_fee_acct = next_account_info(account_iter)?;
    let referral_token_acct = next_account_info(account_iter)?;
    let token_program = next_account_info(account_iter)?;
    let output_mint = next_account_info(account_iter)?;

    if !payer.is_signer {
        msg!("flow-router: payer must be a signer");
        return Err(ProgramError::MissingRequiredSignature);
    }

    // Validate token program is a real SPL Token program
    if *token_program.key != SPL_TOKEN_PROGRAM && *token_program.key != SPL_TOKEN_2022_PROGRAM {
        msg!("flow-router: invalid token program {}", token_program.key);
        return Err(RouterError::InvalidAccount.into());
    }

    // Validate the output mint (needed for TransferChecked) and read its decimals.
    // Mint decimals are immutable, so reading them before the hops is safe.
    let output_decimals = read_mint_decimals(output_mint, output_token_acct, token_program)?;

    let config = read_config(config_pda, program_id)?;

    // Validate integrator whitelist if referral is provided
    if *referral_token_acct.key != *program_id {
        let ref_data = referral_token_acct.try_borrow_data()?;
        if ref_data.len() < 64 {
            msg!("flow-router: referral account data too short");
            return Err(RouterError::InvalidAccount.into());
        }
        let integrator_wallet = pubkey_from_slice(&ref_data[32..64])?;
        let (expected_pda, _) = Pubkey::find_program_address(
            &[INTEGRATOR_SEED, integrator_wallet.as_ref()],
            program_id,
        );
        let found = accounts.iter().any(|a| *a.key == expected_pda && a.owner == program_id);
        if !found {
            msg!("flow-router: integrator {} not whitelisted", integrator_wallet);
            return Err(RouterError::Unauthorized.into());
        }
    }

    let all_remaining: Vec<AccountInfo> = account_iter.cloned().collect();

    // Read output balance before swaps
    let output_before = read_token_balance(output_token_acct)?;

    // Execute each hop sequentially
    let mut dex_offset: usize = 0;
    for (i, hop) in args.hops.iter().enumerate() {
        let count = hop.dex_account_count as usize;
        if dex_offset + count > all_remaining.len() {
            msg!("flow-router: hop {} needs {} accounts at offset {}, only {} remaining",
                i, count, dex_offset, all_remaining.len());
            return Err(RouterError::NotEnoughAccounts.into());
        }

        let hop_accounts = &all_remaining[dex_offset..dex_offset + count];

        let dex_ix = Instruction {
            program_id: hop.dex_program,
            accounts: hop_accounts
                .iter()
                .map(|a| AccountMeta {
                    pubkey: *a.key,
                    is_signer: a.is_signer,
                    is_writable: a.is_writable,
                })
                .collect(),
            data: hop.dex_data.clone(),
        };

        invoke(&dex_ix, &all_remaining).map_err(|e| {
            msg!("flow-router: hop {} CPI to {} failed: {}", i, hop.dex_program, e);
            e
        })?;

        dex_offset += count;
    }

    // Slippage check on final output
    let output_after = read_token_balance(output_token_acct)?;
    let received = output_after.saturating_sub(output_before);

    msg!(
        "flow-router: {} hop(s), {} in, {} out (min {})",
        num_hops, args.amount_in, received, args.min_amount_out
    );

    if received < args.min_amount_out {
        msg!("flow-router: slippage exceeded: got {} < min {}", received, args.min_amount_out);
        return Err(RouterError::SlippageExceeded.into());
    }

    // Collect fee from output token — verify passed token_program matches output account owner
    let token_program_id = detect_token_program(output_token_acct);
    if *token_program.key != token_program_id {
        msg!("flow-router: token_program mismatch: passed {} but output owned by {}",
            token_program.key, token_program_id);
        return Err(RouterError::InvalidAccount.into());
    }
    collect_fees(
        &config, program_id, output_token_acct, protocol_fee_acct,
        referral_token_acct, payer, received, &token_program_id, token_program,
        output_mint, output_decimals,
    )?;

    Ok(())
}

// ── Helpers ──

#[allow(clippy::too_many_arguments)]
fn collect_fees<'a>(
    config: &ConfigData,
    program_id: &Pubkey,
    fee_token_acct: &AccountInfo<'a>,
    protocol_fee_acct: &AccountInfo<'a>,
    referral_token_acct: &AccountInfo<'a>,
    payer: &AccountInfo<'a>,
    fee_basis_amount: u64,
    token_program_id: &Pubkey,
    token_program: &AccountInfo<'a>,
    mint: &AccountInfo<'a>,
    decimals: u8,
) -> ProgramResult {
    if config.fee_bps == 0 { return Ok(()); }

    let fee_amount = fee_basis_amount
        .checked_mul(config.fee_bps as u64)
        .and_then(|v| v.checked_div(10000))
        .unwrap_or(0);

    if fee_amount == 0 { return Ok(()); }

    let has_referral = referral_token_acct.key != program_id;
    let referral_share = if has_referral {
        fee_amount.checked_mul(config.referral_split_bps as u64).unwrap_or(0) / 10000
    } else { 0 };
    let protocol_share = fee_amount.saturating_sub(referral_share);

    if protocol_share > 0 {
        let fee_acct_data = protocol_fee_acct.try_borrow_data()?;
        if fee_acct_data.len() < 64 {
            msg!("flow-router: protocol fee account data too short");
            return Err(RouterError::InvalidAccount.into());
        }
        let fee_acct_owner = pubkey_from_slice(&fee_acct_data[32..64])?;
        if fee_acct_owner != config.treasury_wallet {
            msg!("flow-router: protocol fee account owner mismatch");
            return Err(RouterError::InvalidAccount.into());
        }
    }

    if protocol_share > 0 {
        transfer_tokens(fee_token_acct, mint, protocol_fee_acct, payer, protocol_share, decimals, token_program_id, token_program)?;
    }
    if referral_share > 0 {
        transfer_tokens(fee_token_acct, mint, referral_token_acct, payer, referral_share, decimals, token_program_id, token_program)?;
    }

    msg!("flow-router: fee {} (protocol {}, referral {})", fee_amount, protocol_share, referral_share);
    Ok(())
}

/// Safe pubkey extraction from a byte slice. Returns InvalidInstructionData on failure.
#[inline]
fn pubkey_from_slice(data: &[u8]) -> Result<Pubkey, ProgramError> {
    let arr: [u8; 32] = data.try_into().map_err(|_| ProgramError::from(RouterError::InvalidInstructionData))?;
    Ok(Pubkey::new_from_array(arr))
}

/// Safe u16 extraction from a byte slice.
#[inline]
fn u16_from_slice(data: &[u8]) -> Result<u16, ProgramError> {
    let arr: [u8; 2] = data.try_into().map_err(|_| ProgramError::from(RouterError::InvalidInstructionData))?;
    Ok(u16::from_le_bytes(arr))
}

fn verify_config_ownership(config_account: &AccountInfo, program_id: &Pubkey) -> Result<(), ProgramError> {
    if config_account.owner != program_id {
        return Err(RouterError::InvalidAccount.into());
    }
    let config_data = config_account.try_borrow_data()?;
    if config_data.len() < CONFIG_SIZE {
        return Err(RouterError::InvalidAccount.into());
    }
    if config_data[..8] != CONFIG_DISCRIMINATOR {
        return Err(RouterError::InvalidAccount.into());
    }
    Ok(())
}

fn read_config(config_account: &AccountInfo, program_id: &Pubkey) -> Result<ConfigData, ProgramError> {
    verify_config_ownership(config_account, program_id)?;
    let config_data = config_account.try_borrow_data()?;
    Ok(ConfigData {
        fee_bps: u16_from_slice(&config_data[40..42])?,
        treasury_wallet: pubkey_from_slice(&config_data[42..74])?,
        referral_split_bps: u16_from_slice(&config_data[74..76])?,
    })
}

fn detect_token_program(account: &AccountInfo) -> Pubkey {
    if *account.owner == SPL_TOKEN_2022_PROGRAM { SPL_TOKEN_2022_PROGRAM } else { SPL_TOKEN_PROGRAM }
}

fn read_token_balance(account: &AccountInfo) -> Result<u64, ProgramError> {
    let data = account.try_borrow_data()?;
    if data.len() < 72 { return Err(RouterError::BalanceReadFailed.into()); }
    let bytes: [u8; 8] = data[64..72].try_into().map_err(|_| ProgramError::from(RouterError::BalanceReadFailed))?;
    Ok(u64::from_le_bytes(bytes))
}

/// Validate the output mint account and return its decimals.
///
/// The mint must (a) be owned by the passed token program and (b) be the mint
/// recorded in the output token account (token account data bytes 0..32).
fn read_mint_decimals(
    mint: &AccountInfo,
    output_token_acct: &AccountInfo,
    token_program: &AccountInfo,
) -> Result<u8, ProgramError> {
    if mint.owner != token_program.key {
        msg!("flow-router: output mint {} not owned by token program {}", mint.key, token_program.key);
        return Err(RouterError::InvalidAccount.into());
    }
    {
        let out_data = output_token_acct.try_borrow_data()?;
        if out_data.len() < 32 {
            msg!("flow-router: output token account data too short");
            return Err(RouterError::InvalidAccount.into());
        }
        if out_data[..32] != mint.key.to_bytes() {
            msg!("flow-router: output mint {} does not match output token account mint", mint.key);
            return Err(RouterError::InvalidAccount.into());
        }
    }
    let mint_data = mint.try_borrow_data()?;
    if mint_data.len() < MINT_BASE_LEN {
        msg!("flow-router: output mint data too short");
        return Err(RouterError::InvalidAccount.into());
    }
    Ok(mint_data[MINT_DECIMALS_OFFSET])
}

/// Build a `TransferChecked` instruction (tag 12 + amount u64 LE + decimals u8).
/// Valid for both SPL Token and Token-2022. Hand-encoded to avoid an spl-token dependency.
///
/// Accounts: source (w), mint (r), destination (w), authority (signer).
pub fn transfer_checked_ix(
    token_program_id: &Pubkey,
    source: &Pubkey,
    mint: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
    amount: u64,
    decimals: u8,
) -> Instruction {
    let mut data = Vec::with_capacity(10);
    data.push(TRANSFER_CHECKED_TAG);
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    Instruction {
        program_id: *token_program_id,
        accounts: vec![
            AccountMeta::new(*source, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

#[allow(clippy::too_many_arguments)]
fn transfer_tokens<'a>(
    source: &AccountInfo<'a>, mint: &AccountInfo<'a>, destination: &AccountInfo<'a>,
    authority: &AccountInfo<'a>, amount: u64, decimals: u8,
    token_program_id: &Pubkey, token_program: &AccountInfo<'a>,
) -> ProgramResult {
    // A failure here (e.g. a transfer-hook mint whose extra accounts were not supplied)
    // propagates as the token program's own CPI error and reverts the swap.
    invoke(
        &transfer_checked_ix(
            token_program_id, source.key, mint.key, destination.key, authority.key,
            amount, decimals,
        ),
        &[source.clone(), mint.clone(), destination.clone(), authority.clone(), token_program.clone()],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_checked_encoding() {
        let prog = SPL_TOKEN_2022_PROGRAM;
        let (src, mint, dst, auth) = (
            Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
        );
        let ix = transfer_checked_ix(&prog, &src, &mint, &dst, &auth, 0x0102_0304_0506_0708, 6);
        assert_eq!(ix.program_id, prog);
        assert_eq!(ix.data, vec![12, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, 6]);
        let metas: Vec<(Pubkey, bool, bool)> =
            ix.accounts.iter().map(|m| (m.pubkey, m.is_signer, m.is_writable)).collect();
        assert_eq!(
            metas,
            vec![
                (src, false, true),
                (mint, false, false),
                (dst, false, true),
                (auth, true, false),
            ]
        );
    }
}
