//! The transaction mix: setup blocks that grow the state, then the range a
//! workload stores.

use std::{
    collections::{BTreeMap, BTreeSet},
    mem,
};

use alloy_consensus::Header;
use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use alpen_evm_ee::EvmPartialState;
use alpen_params::{AlpenParams, DEV_PARAMS_JSON};
use alpen_reth_evm::{
    address_to_subject,
    constants::{BRIDGEOUT_PRECOMPILE_ADDRESS, SCHNORR_PRECOMPILE_ADDRESS},
};
use alpen_reth_statediff::BatchBuilder;
use eyre::{ensure, eyre};
use rand_chacha::{
    rand_core::{RngCore, SeedableRng},
    ChaCha8Rng,
};
use serde::Serialize;
use strata_codec::encode_to_vec;
use tracing::info;

use super::{
    accounts::{Account, SignedTx},
    chain::{BuiltBlock, DevChain, DA_RATE},
    contracts,
};
use crate::{Deposit, Workload, WorkloadBlock};

/// One BTC in wei, the bridge's withdrawal denomination on dev.
const BTC: u128 = 1_000_000_000_000_000_000;

/// One whole token, at 18 decimals.
const TOKEN: u128 = 1_000_000_000_000_000_000;

/// Gas limits per transaction kind. Each leaves room over the gas the call
/// uses, because the DA fee is drawn from the unused part.
const TRANSFER_GAS: u64 = 50_000;
const TOKEN_GAS: u64 = 120_000;
const SWAP_GAS: u64 = 250_000;
const PRECOMPILE_GAS: u64 = 100_000;
const DEPLOY_GAS: u64 = 3_000_000;

/// Setup transactions packed into one block, by kind. Each pack stays under
/// the 30M dev block gas limit.
const FUNDING_PER_BLOCK: usize = 1_000;
const TOKEN_SEEDING_PER_BLOCK: usize = 400;
const APPROVALS_PER_BLOCK: usize = 300;

/// Tags of the token deployments. Setup deploys the first two; the range
/// redeploys the first (code earlier batches published) and deploys a new one.
const TOKEN_A_TAG: u64 = 1;
const TOKEN_B_TAG: u64 = 2;
const NEW_TOKEN_TAG: u64 = 3;

/// Knobs of a generated workload.
#[derive(Clone, Debug)]
pub struct Config {
    /// Seeds the accounts and every random choice.
    pub seed: u64,
    /// Funded EOAs created in setup.
    pub eoas: usize,
    /// EOAs that receive token A in setup. The first `holders` EOAs.
    pub holders: usize,
    /// Holders that also get token B and approve the pool. The first
    /// `traders` EOAs.
    pub traders: usize,
    /// Blocks in the stored range.
    pub blocks: usize,
    /// Random transactions per block in the range, drawn uniformly.
    pub min_txs_per_block: u64,
    pub max_txs_per_block: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            seed: 0,
            eoas: 10_000,
            holders: 5_000,
            traders: 300,
            blocks: 100,
            min_txs_per_block: 10,
            max_txs_per_block: 30,
        }
    }
}

/// Human-readable record of a generated workload, stored next to it.
#[derive(Debug, Serialize)]
pub struct Summary {
    pub seed: u64,
    pub eoas: usize,
    pub holders: usize,
    pub traders: usize,
    pub setup_blocks: u64,
    pub first_block: u64,
    pub last_block: u64,
    pub transactions: BTreeMap<&'static str, u64>,
    pub deposits: u64,
    pub gas_used: u64,
    pub da_rate_wei_per_byte: u64,
}

/// What a transaction is, for the summary and for error messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    NativeTransfer,
    NativeToFreshAddress,
    LegacyTransfer,
    TokenTransfer,
    TokenToNewHolder,
    TokenApprove,
    TokenTransferFrom,
    Swap,
    BridgeOut,
    SchnorrVerify,
    TokenDeploy,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::NativeTransfer => "native_transfer",
            Self::NativeToFreshAddress => "native_to_fresh_address",
            Self::LegacyTransfer => "legacy_transfer",
            Self::TokenTransfer => "token_transfer",
            Self::TokenToNewHolder => "token_to_new_holder",
            Self::TokenApprove => "token_approve",
            Self::TokenTransferFrom => "token_transfer_from",
            Self::Swap => "swap",
            Self::BridgeOut => "bridge_out",
            Self::SchnorrVerify => "schnorr_verify",
            Self::TokenDeploy => "token_deploy",
        }
    }
}

/// Weights of the random transactions in each block of the range. The
/// scheduled ones (bridge-outs, precompile calls, deploys, the second half of
/// approve-then-pull) come on top.
const MIX: [(Kind, u64); 7] = [
    (Kind::NativeTransfer, 35),
    (Kind::NativeToFreshAddress, 10),
    (Kind::LegacyTransfer, 5),
    (Kind::TokenTransfer, 25),
    (Kind::TokenToNewHolder, 10),
    (Kind::TokenApprove, 5),
    (Kind::Swap, 10),
];

/// A transaction and what it must do.
struct Planned {
    kind: Kind,
    tx: SignedTx,
    /// Output the call must return, for precompile calls.
    expected_output: Option<Bytes>,
}

/// A pull an approval set up for the next block.
struct Pull {
    spender: usize,
    owner: Address,
    amount: U256,
}

/// Builds a workload with `config`.
pub fn generate(config: &Config) -> eyre::Result<(Workload, Summary)> {
    let params: AlpenParams = serde_json::from_str(DEV_PARAMS_JSON)?;
    let mut generator = Generator::new(config.clone(), DevChain::new(&params)?);
    let setup_blocks = generator.setup()?;
    generator.range(setup_blocks)
}

struct Generator {
    config: Config,
    chain: DevChain,
    chain_id: u64,
    rng: ChaCha8Rng,
    funder: Account,
    eoas: Vec<Account>,
    token_a: Address,
    token_b: Address,
    pair: Address,
    fresh_addresses: u64,
    next_spender: usize,
    pulls: Vec<Pull>,
    published_code_hashes: BTreeSet<[u8; 32]>,
    counts: BTreeMap<&'static str, u64>,
}

impl Generator {
    fn new(config: Config, chain: DevChain) -> Self {
        let eoas = (0..config.eoas as u64)
            .map(|index| Account::derive(config.seed, index))
            .collect();
        Self {
            chain_id: chain.chain_id(),
            rng: ChaCha8Rng::seed_from_u64(config.seed),
            next_spender: config.holders,
            config,
            chain,
            funder: Account::dev_funder(),
            eoas,
            token_a: Address::ZERO,
            token_b: Address::ZERO,
            pair: Address::ZERO,
            fresh_addresses: 0,
            pulls: Vec::new(),
            published_code_hashes: BTreeSet::new(),
            counts: BTreeMap::new(),
        }
    }

    /// Funds the EOAs, deploys two tokens and a pool, seeds token holders and
    /// traders. Returns the number of blocks built.
    fn setup(&mut self) -> eyre::Result<u64> {
        let mut blocks = 0;
        let chain_id = self.chain_id;

        let addresses: Vec<Address> = self.eoas.iter().map(Account::address).collect();
        for batch in addresses.chunks(FUNDING_PER_BLOCK) {
            let txs = batch
                .iter()
                .map(|to| {
                    self.funder.eip1559(
                        chain_id,
                        Some(*to),
                        U256::from(10 * BTC),
                        Bytes::new(),
                        TRANSFER_GAS,
                    )
                })
                .collect();
            self.setup_block(txs)?;
            blocks += 1;
        }

        let supply = U256::from(1_000_000_000_000 * TOKEN);
        let liquidity = U256::from(1_000_000_000 * TOKEN);
        self.token_a = self.funder.next_create_address();
        let deploy_a = self.deploy(TOKEN_A_TAG, supply);
        self.token_b = self.funder.next_create_address();
        let deploy_b = self.deploy(TOKEN_B_TAG, supply);
        self.pair = self.funder.next_create_address();
        let deploy_pair = self.funder.eip1559(
            chain_id,
            None,
            U256::ZERO,
            contracts::pair_init_code(self.token_a, self.token_b),
            DEPLOY_GAS,
        );
        let approve_a = self.funder_call(self.token_a, contracts::approve(self.pair, U256::MAX));
        let approve_b = self.funder_call(self.token_b, contracts::approve(self.pair, U256::MAX));
        let seed_pool = self.funder.eip1559(
            chain_id,
            Some(self.pair),
            U256::ZERO,
            contracts::add_liquidity(liquidity, liquidity),
            SWAP_GAS,
        );
        self.setup_block(vec![
            deploy_a,
            deploy_b,
            deploy_pair,
            approve_a,
            approve_b,
            seed_pool,
        ])?;
        blocks += 1;

        let holding = U256::from(1_000_000 * TOKEN);
        for batch in addresses[..self.config.holders].chunks(TOKEN_SEEDING_PER_BLOCK) {
            let txs = batch
                .iter()
                .map(|holder| self.funder_call(self.token_a, contracts::transfer(*holder, holding)))
                .collect();
            self.setup_block(txs)?;
            blocks += 1;
        }

        let traders = &addresses[..self.config.traders];
        let txs = traders
            .iter()
            .map(|trader| self.funder_call(self.token_b, contracts::transfer(*trader, holding)))
            .collect();
        self.setup_block(txs)?;
        blocks += 1;

        for token in [self.token_a, self.token_b] {
            for batch in (0..self.config.traders)
                .collect::<Vec<_>>()
                .chunks(APPROVALS_PER_BLOCK)
            {
                let txs = batch
                    .iter()
                    .map(|&trader| {
                        self.eoas[trader].eip1559(
                            chain_id,
                            Some(token),
                            U256::ZERO,
                            contracts::approve(self.pair, U256::MAX),
                            TOKEN_GAS,
                        )
                    })
                    .collect();
                self.setup_block(txs)?;
                blocks += 1;
            }
        }

        info!(blocks, "built setup blocks");
        Ok(blocks)
    }

    fn setup_block(&mut self, txs: Vec<SignedTx>) -> eyre::Result<()> {
        let built = self.chain.build_block(txs, &[])?;
        for (index, outcome) in built.outcomes.iter().enumerate() {
            ensure!(
                outcome.success,
                "setup tx {index} in block {} failed",
                built.number
            );
        }
        self.published_code_hashes.extend(
            built
                .state_changes
                .deployed_bytecodes
                .keys()
                .map(|hash| hash.0),
        );
        Ok(())
    }

    /// Builds the stored range on top of the setup blocks.
    fn range(mut self, setup_blocks: u64) -> eyre::Result<(Workload, Summary)> {
        let mut blocks = Vec::with_capacity(self.config.blocks);
        let mut hashes = Vec::with_capacity(self.config.blocks);
        let mut prev_header_rlp = Vec::new();
        let mut gas_used = 0;
        let mut deposit_count = 0;

        // Assembled the way `ChunkSpec::fetch_input` in alpen-client does: one
        // node bag over the blocks' witness records, duplicates and all.
        let mut witness_state = Vec::new();
        let mut codes = Vec::new();
        let mut ancestor_headers = Vec::new();
        // Aggregated the way the DA blob provider does.
        let mut state_diff = BatchBuilder::new();

        for index in 0..self.config.blocks {
            let planned = self.plan_block(index);
            let deposits = self.plan_deposits(index);
            let (kinds, expected): (Vec<Kind>, Vec<Option<Bytes>>) = planned
                .iter()
                .map(|planned| (planned.kind, planned.expected_output.clone()))
                .unzip();
            let txs = planned.into_iter().map(|planned| planned.tx).collect();

            let built = self.chain.build_block(txs, &deposits)?;
            check_outcomes(&built, &kinds, &expected)?;
            for kind in kinds {
                *self.counts.entry(kind.name()).or_default() += 1;
            }

            let witness = built.witness;
            if index == 0 {
                prev_header_rlp = witness.raw_parent_header_rlp;
            }
            witness_state.extend(witness.witness_state);
            codes.extend(witness.codes);
            for header in witness.ancestor_headers {
                ancestor_headers.push(alloy_rlp::decode_exact::<Header>(&header[..])?);
            }
            state_diff.apply_block(&built.state_changes);

            blocks.push(WorkloadBlock {
                block_rlp: witness.raw_block_rlp,
                deposits: deposits
                    .iter()
                    .map(|(address, sats)| Deposit {
                        dest_subject: *address_to_subject(*address).inner(),
                        sats: *sats,
                    })
                    .collect(),
            });
            deposit_count += deposits.len() as u64;
            gas_used += built.gas_used;
            hashes.push((built.hash, built.number));
        }

        let (first_hash, first_block) = *hashes.first().ok_or_else(|| eyre!("empty range"))?;
        let (last_hash, last_block) = *hashes.last().expect("range is not empty");
        let prev_header: Header = alloy_rlp::decode_exact(&prev_header_rlp[..])?;
        let chunk_pre_state = EvmPartialState::from_witness_parts(
            witness_state,
            prev_header.state_root,
            codes,
            ancestor_headers,
        );
        let range_pre_state = self.chain.range_pre_state(first_hash, last_hash)?;
        info!(first_block, last_block, gas_used, "built range");

        let workload = Workload::new(
            prev_header_rlp,
            blocks,
            encode_to_vec(&chunk_pre_state)?,
            range_pre_state,
            encode_to_vec(&state_diff.build())?,
            self.published_code_hashes.into_iter().collect(),
        );
        let summary = Summary {
            seed: self.config.seed,
            eoas: self.config.eoas,
            holders: self.config.holders,
            traders: self.config.traders,
            setup_blocks,
            first_block,
            last_block,
            transactions: self.counts,
            deposits: deposit_count,
            gas_used,
            da_rate_wei_per_byte: DA_RATE,
        };
        Ok((workload, summary))
    }

    /// Plans the transactions of block `index` of the range.
    fn plan_block(&mut self, index: usize) -> Vec<Planned> {
        let mut planned: Vec<Planned> = mem::take(&mut self.pulls)
            .into_iter()
            .map(|pull| self.pull(pull))
            .collect();

        if index % 10 == 3 {
            planned.push(self.bridge_out());
        }
        if index % 5 == 1 {
            planned.push(self.schnorr_verify(index));
        }
        if index == self.config.blocks / 4 {
            planned.push(self.range_deploy(NEW_TOKEN_TAG));
        }
        if index == self.config.blocks * 3 / 4 {
            planned.push(self.range_deploy(TOKEN_A_TAG));
        }

        let count = self.between(
            self.config.min_txs_per_block.into(),
            self.config.max_txs_per_block.into(),
        );
        for _ in 0..count {
            let kind = self.draw_kind();
            planned.push(self.random_tx(kind));
        }
        planned
    }

    /// Deposits of block `index` of the range: one every ten blocks,
    /// alternating between a funded EOA and a fresh address.
    fn plan_deposits(&mut self, index: usize) -> Vec<(Address, u64)> {
        if index % 10 != 7 {
            return Vec::new();
        }
        let to = if index % 20 == 7 {
            self.random_eoa_address()
        } else {
            self.fresh_address()
        };
        let sats = self.between(1, 10) as u64 * 50_000_000;
        vec![(to, sats)]
    }

    fn random_tx(&mut self, kind: Kind) -> Planned {
        let chain_id = self.chain_id;
        let tx = match kind {
            Kind::NativeTransfer => {
                let (from, to) = self.two_eoas(self.config.eoas);
                let to = self.eoas[to].address();
                let value = U256::from(self.between(BTC / 1_000, BTC / 10));
                self.eoas[from].eip1559(chain_id, Some(to), value, Bytes::new(), TRANSFER_GAS)
            }
            Kind::NativeToFreshAddress => {
                let from = self.random_index(self.config.eoas);
                let to = self.fresh_address();
                self.eoas[from].eip1559(
                    chain_id,
                    Some(to),
                    U256::from(BTC / 100),
                    Bytes::new(),
                    TRANSFER_GAS,
                )
            }
            Kind::LegacyTransfer => {
                let (from, to) = self.two_eoas(self.config.eoas);
                let to = self.eoas[to].address();
                let value = U256::from(self.between(BTC / 1_000, BTC / 10));
                self.eoas[from].legacy_transfer(chain_id, to, value, TRANSFER_GAS)
            }
            Kind::TokenTransfer => {
                let (from, to) = self.two_eoas(self.config.holders);
                let to = self.eoas[to].address();
                let amount = U256::from(self.between(TOKEN, 100 * TOKEN));
                self.token_call(from, self.token_a, contracts::transfer(to, amount))
            }
            Kind::TokenToNewHolder => {
                let from = self.random_index(self.config.holders);
                let to = self.fresh_address();
                self.token_call(
                    from,
                    self.token_a,
                    contracts::transfer(to, U256::from(TOKEN)),
                )
            }
            Kind::TokenApprove => {
                let owner = self.random_index(self.config.holders);
                // Spenders are EOAs outside the holder set, each used once, so
                // no approval overwrites another before its pull.
                let spender = self.next_spender;
                self.next_spender += 1;
                let amount = U256::from(10 * TOKEN);
                let spender_address = self.eoas[spender].address();
                self.pulls.push(Pull {
                    spender,
                    owner: self.eoas[owner].address(),
                    amount,
                });
                self.token_call(
                    owner,
                    self.token_a,
                    contracts::approve(spender_address, amount),
                )
            }
            Kind::Swap => {
                let trader = self.random_index(self.config.traders);
                let zero_for_one = self.rng.next_u32().is_multiple_of(2);
                let amount = U256::from(self.between(TOKEN, 100 * TOKEN));
                self.eoas[trader].eip1559(
                    chain_id,
                    Some(self.pair),
                    U256::ZERO,
                    contracts::swap(zero_for_one, amount),
                    SWAP_GAS,
                )
            }
            Kind::TokenTransferFrom | Kind::BridgeOut | Kind::SchnorrVerify | Kind::TokenDeploy => {
                unreachable!("{kind:?} is scheduled, not drawn")
            }
        };
        Planned {
            kind,
            tx,
            expected_output: None,
        }
    }

    /// Spends the whole allowance, which clears its storage slot.
    fn pull(&mut self, pull: Pull) -> Planned {
        let to = self.fresh_address();
        let tx = self.token_call(
            pull.spender,
            self.token_a,
            contracts::transfer_from(pull.owner, to, pull.amount),
        );
        Planned {
            kind: Kind::TokenTransferFrom,
            tx,
            expected_output: None,
        }
    }

    fn bridge_out(&mut self) -> Planned {
        let from = self.random_index(self.config.eoas);
        let mut pubkey_hash = [0u8; 20];
        self.rng.fill_bytes(&mut pubkey_hash);
        let tx = self.eoas[from].eip1559(
            self.chain_id,
            Some(BRIDGEOUT_PRECOMPILE_ADDRESS),
            U256::from(BTC),
            contracts::bridge_out(pubkey_hash),
            PRECOMPILE_GAS,
        );
        Planned {
            kind: Kind::BridgeOut,
            tx,
            expected_output: None,
        }
    }

    fn schnorr_verify(&mut self, index: usize) -> Planned {
        let from = self.random_index(self.config.eoas);
        let key = self.derived_secret(b"schnorr", index);
        let message = keccak256(index.to_be_bytes());
        let tx = self.eoas[from].eip1559(
            self.chain_id,
            Some(SCHNORR_PRECOMPILE_ADDRESS),
            U256::ZERO,
            contracts::schnorr_verify(key, message),
            PRECOMPILE_GAS,
        );
        Planned {
            kind: Kind::SchnorrVerify,
            tx,
            expected_output: Some(Bytes::from_static(&[1])),
        }
    }

    fn range_deploy(&mut self, tag: u64) -> Planned {
        let from = self.random_index(self.config.eoas);
        let tx = self.eoas[from].eip1559(
            self.chain_id,
            None,
            U256::ZERO,
            contracts::token_init_code(tag, U256::from(1_000_000 * TOKEN)),
            DEPLOY_GAS,
        );
        Planned {
            kind: Kind::TokenDeploy,
            tx,
            expected_output: None,
        }
    }

    fn deploy(&mut self, tag: u64, supply: U256) -> SignedTx {
        self.funder.eip1559(
            self.chain_id,
            None,
            U256::ZERO,
            contracts::token_init_code(tag, supply),
            DEPLOY_GAS,
        )
    }

    fn funder_call(&mut self, to: Address, input: Bytes) -> SignedTx {
        self.funder
            .eip1559(self.chain_id, Some(to), U256::ZERO, input, TOKEN_GAS)
    }

    fn token_call(&mut self, from: usize, token: Address, input: Bytes) -> SignedTx {
        self.eoas[from].eip1559(self.chain_id, Some(token), U256::ZERO, input, TOKEN_GAS)
    }

    fn draw_kind(&mut self) -> Kind {
        let total: u64 = MIX.iter().map(|(_, weight)| weight).sum();
        let mut pick = self.rng.next_u64() % total;
        for (kind, weight) in MIX {
            if pick < weight {
                return kind;
            }
            pick -= weight;
        }
        unreachable!("pick is below the total weight")
    }

    /// Two distinct indices below `bound`.
    fn two_eoas(&mut self, bound: usize) -> (usize, usize) {
        let first = self.random_index(bound);
        let offset = 1 + self.random_index(bound - 1);
        (first, (first + offset) % bound)
    }

    fn random_index(&mut self, bound: usize) -> usize {
        (self.rng.next_u64() % bound as u64) as usize
    }

    fn random_eoa_address(&mut self) -> Address {
        let index = self.random_index(self.config.eoas);
        self.eoas[index].address()
    }

    /// An address no account in the workload has used.
    fn fresh_address(&mut self) -> Address {
        self.fresh_addresses += 1;
        let mut preimage = b"alpen-evm-workload/fresh".to_vec();
        preimage.extend_from_slice(&self.config.seed.to_be_bytes());
        preimage.extend_from_slice(&self.fresh_addresses.to_be_bytes());
        Address::from_word(keccak256(preimage))
    }

    fn derived_secret(&self, domain: &[u8], index: usize) -> B256 {
        let mut preimage = b"alpen-evm-workload/".to_vec();
        preimage.extend_from_slice(domain);
        preimage.extend_from_slice(&self.config.seed.to_be_bytes());
        preimage.extend_from_slice(&index.to_be_bytes());
        keccak256(preimage)
    }

    /// A value drawn from `low..=high`. Not exactly uniform; close enough for
    /// picking amounts.
    fn between(&mut self, low: u128, high: u128) -> u128 {
        let draw = (u128::from(self.rng.next_u64()) << 64) | u128::from(self.rng.next_u64());
        low + draw % (high - low + 1)
    }
}

fn check_outcomes(
    built: &BuiltBlock,
    kinds: &[Kind],
    expected: &[Option<Bytes>],
) -> eyre::Result<()> {
    for (index, outcome) in built.outcomes.iter().enumerate() {
        let kind = kinds[index];
        ensure!(
            outcome.success,
            "{} tx {index} in block {} failed",
            kind.name(),
            built.number
        );
        if let Some(expected) = &expected[index] {
            ensure!(
                &outcome.output == expected,
                "{} tx {index} in block {} returned {}, expected {expected}",
                kind.name(),
                built.number,
                outcome.output
            );
        }
    }
    Ok(())
}
