use solana_program::program_error::ProgramError;

#[derive(Debug, Clone, Copy)]
pub enum RouterError {
    /// Instruction data too short to parse.
    InvalidInstructionData = 0,
    /// Output amount is less than the minimum required.
    SlippageExceeded = 1,
    /// Fee BPS exceeds maximum (10000).
    InvalidFeeBps = 2,
    /// Token balance read failed.
    BalanceReadFailed = 3,
    /// CPI to DEX program failed.
    CpiFailed = 4,
    /// Not enough accounts provided.
    NotEnoughAccounts = 5,
    /// Invalid account for the given position.
    InvalidAccount = 6,
    /// Config PDA already initialized.
    AlreadyInitialized = 7,
    /// Signer is not the admin authority.
    Unauthorized = 8,
}

impl From<RouterError> for ProgramError {
    fn from(e: RouterError) -> Self {
        ProgramError::Custom(e as u32)
    }
}
