# flow-router-solana

On-chain Solana program that executes a swap route of 1 to 5 DEX legs as sequential CPIs inside one
instruction, enforces a single minimum on the final output, and optionally collects a fee from the
output token. It is the settlement layer used by
[flow-trades](https://github.com/sigint-zero/flow-trades-solana), which builds the DEX
instructions and wraps them in a router call; any client can do the same.

**Deployments:** `FLoWxxKoBrZtNj5NTPuy1tZcSU6Nnjtz7v5snrrUsNqm` is the first generation (immutable,
50 bps, plain SPL `Transfer` fee collection, no `output_mint` account). This source is the second
generation: `TransferChecked` fee collection, configurable fee including zero, upgradeable.

---

## Quick Start

```bash
cargo test                 # native program-test suite, no network
cargo build-sbf            # target/deploy/flow_router.so
solana program deploy target/deploy/flow_router.so --program-id program-keypair.json --keypair deployer.json
```

Then send `initialize_config` (below) and point your client at the new program id; flow-trades takes
it as `ROUTER_PROGRAM_ID`.

---

## How It Works

```
swap
  ├── read config PDA (fee_bps, treasury wallet, referral split)
  ├── validate token program (SPL Token or Token-2022) and the output mint
  ├── validate the integrator whitelist PDA if a referral account is passed
  ├── execute N sequential DEX CPIs (1 to 5 hops)
  ├── require final output ≥ min_amount_out
  └── fee_bps > 0: TransferChecked protocol share → treasury, referral share → integrator
```

The slippage check is on the gross amount that arrives in the user's output token account, before the
router fee. Intermediate legs carry no floor of their own; a hop that fails reverts the whole
transaction.

---

## Instructions

### `initialize_config` (disc 2)

| # | Account | Signer | Writable |
|---|---------|--------|----------|
| 0 | admin | yes | yes |
| 1 | config PDA `["config"]` | no | yes |
| 2 | system program | no | no |

Data: `[2] + admin(32) + fee_bps(u16 LE) + treasury_wallet(32) + referral_split_bps(u16 LE)`

The config is write-once. `fee_bps` 0–10000 (`0` disables fee collection), `referral_split_bps` is the
share of the fee paid to a whitelisted integrator. There is no update instruction; changing the config
means an upgrade or a new deployment (see Deployment).

### `add_integrator` (disc 4) / `remove_integrator` (disc 5)

| # | Account | Signer | Writable |
|---|---------|--------|----------|
| 0 | admin (must match config) | yes | yes |
| 1 | config PDA | no | no |
| 2 | integrator PDA `["integrator", wallet]` | no | yes |
| 3 | system program (add only) | no | no |

Data: `[4 or 5] + integrator_wallet(32)`

### `swap` (disc 0)

| # | Account | Signer | Writable | |
|---|---------|--------|----------|---|
| 0 | payer | yes | no | |
| 1 .. N+1 | token accounts | no | yes | `[input, intermediate_1..N-1, output]` |
| N+2 | config PDA | no | no | |
| N+3 | protocol_fee_acct | no | yes | treasury's token account for the output mint; unread when `fee_bps = 0` |
| N+4 | referral_acct | no | yes | integrator's token account, or the program id to skip |
| N+5 | token_program | no | no | SPL Token or Token-2022, must own the output account |
| N+6 | output_mint | no | no | mint of the output token account |
| N+7 .. | DEX accounts | | | all hops concatenated, then the deduplicated DEX program ids |
| last | integrator PDA | no | no | only when a referral account is passed |

Data: `[0] + num_hops(u8) + hops + amount_in(u64) + min_amount_out(u64)`, each hop
`dex_program(32) + data_len(u32) + data + account_count(u8)`.

The first-generation program has no `output_mint` account; its DEX accounts start at N+6. Instruction
data is identical.

---

## Fee Structure

| | |
|---|---|
| Rate | `fee_bps` from the config PDA; `0` = nothing computed, nothing transferred, fee-account slots not read |
| Side | output token, after all hops and the slippage check |
| Split | `referral_split_bps` to the integrator, remainder to the treasury |
| Accounts | per-mint token accounts owned by the treasury / integrator wallet, validated by owner |
| Transfer | `TransferChecked`, so Token-2022 mints with extensions work; a transfer-fee mint withholds its fee from the shares |
| Not supported | transfer-hook mints when `fee_bps > 0` (the hook's accounts are not forwarded; the swap reverts) |

---

## Security Model

| Property | Enforcement |
|----------|-------------|
| Config | write-once PDA, discriminator and owner checked on every swap |
| Admin | `initialize_config` requires the admin in data to be the signer; integrator changes require that admin |
| Referrals | integrator PDA must exist for the referral account's owner, else `Unauthorized` |
| Token program | must be SPL Token or Token-2022 and must own the output token account |
| Output mint | must be owned by that token program and equal the mint stored in the output account |
| Fee account | owner must be the treasury wallet (checked only when a fee is due) |
| Slippage | one check on the final output, after every CPI |
| Panics | none in program code; every failure is a typed `ProgramError` |

### Error Codes

| Code | Name |
|------|------|
| 0 | InvalidInstructionData |
| 1 | SlippageExceeded |
| 2 | InvalidFeeBps |
| 3 | BalanceReadFailed |
| 4 | CpiFailed (unused; CPI errors propagate) |
| 5 | NotEnoughAccounts |
| 6 | InvalidAccount |
| 7 | AlreadyInitialized |
| 8 | Unauthorized |

---

## On-Chain Layout

Config PDA, seeds `["config"]`, 76 bytes:

```
0    8   discriminator "flowconf"
8   32   admin
40   2   fee_bps (u16)
42  32   treasury_wallet
74   2   referral_split_bps (u16)
```

Integrator PDA, seeds `["integrator", wallet]`, 40 bytes: discriminator `"flowintg"` + wallet.

---

## Deploy

```bash
cargo build-sbf       # target/deploy/flow_router.so

solana-keygen new -o program-keypair.json
solana program deploy target/deploy/flow_router.so \
  --program-id program-keypair.json --keypair deployer.json

# initialize_config: [2] + admin(32) + fee_bps(u16 LE) + treasury(32) + referral_split_bps(u16 LE)
# accounts: admin (signer, writable), config PDA ["config"], system program

solana program show <PROGRAM_ID>      # Authority must not read "none"
```

Rent for the ~106 KB binary is about 1.08 SOL (program data is twice the binary plus 45 bytes).

**Deployment policy: the program stays upgradeable.** Keep the upgrade authority on a deployer or
cold key; do not run `set-upgrade-authority --final`. A retired deployment is closed with
`solana program close <PROGRAM_ID> --recipient <wallet>` to reclaim the rent; config and integrator
PDAs have no close instruction. Fee or layout changes that keep the config format are in-place
upgrades; a config-format change needs a new program id because the config PDA is write-once.

A possible future instruction, not implemented: an admin-only `set_fee` (discriminator 3 is reserved)
to change `fee_bps` / `referral_split_bps` without an upgrade.

---

## Using It From a Client

Build the DEX instructions as you normally would, then emit one `swap` instruction: payer, the
token-account chain, the five fixed accounts, every DEX instruction's accounts in order, the DEX
program ids, and serialize each DEX instruction into a hop. Intermediate hops should spend what the
previous hop is guaranteed to deliver at your slippage; `min_amount_out` applies to the route's final
output only. The reference implementation is `execution/router.rs::wrap_swap` in
[flow-trades](https://github.com/sigint-zero/flow-trades-solana), which also selects the
first- or second-generation account layout from the program id.

---

## Testing

```bash
cargo test                 # 10 tests: 1 unit, 9 program-test
```

`tests/swap_fee_transfer.rs` runs the program natively with `solana-program-test` against the bundled
SPL Token and Token-2022 programs: fee collection on plain and transfer-fee mints (exact share
arithmetic including the withheld transfer fee), the referral split, slippage on the gross amount,
output-mint validation, rejection of the first-generation account layout, transfer-hook error
propagation, and the zero-fee configuration (full output to the user, no transfer, fee slots ignored).

`cargo run --release --features vanity --bin vanity` grinds a program keypair with a `FLOW` prefix.

---

## Contributing

Pull requests and issues are welcome. To get involved, [join the Telegram](https://t.me/+3BPRvJoUvlViMzg1).

## License

MIT — see [LICENSE](./LICENSE).
