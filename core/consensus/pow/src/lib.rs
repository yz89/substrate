// Copyright 2017-2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Proof of work consensus for Substrate.
//!
//! To use this engine, you can either implement a standalone `PowAlgorithm`
//! or use the runtime PoW algorithm engine. The runtime engine requires you
//! to implement the `PowApi` defined in `substrate-consensus-pow-primitives`.
//! There is also a helper module in SRML (`srml-pow`) to make the process easier.

use std::sync::Arc;
use std::thread;
use client::{
	BlockOf, blockchain::{HeaderBackend, ProvideCache},
	block_builder::api::BlockBuilder as BlockBuilderApi, backend::AuxStore,
};
use sr_primitives::Justification;
use sr_primitives::generic::{BlockId, Digest, DigestItem};
use sr_primitives::traits::{Block as BlockT, Header as HeaderT, ProvideRuntimeApi};
use srml_timestamp::{TimestampInherentData, InherentError as TIError};
use pow_primitives::{PowApi, Difficulty, Seal, POW_ENGINE_ID};
use primitives::H256;
use inherents::{InherentDataProviders, InherentData};
use consensus_common::{
	BlockImportParams, BlockOrigin, ForkChoiceStrategy,
	well_known_cache_keys::Id as CacheKeyId, Environment, Proposer,
};
use consensus_common::import_queue::{BoxBlockImport, BasicQueue, Verifier};
use codec::{Encode, Decode};
use log::*;

/// Auxiliary prefix for PoW engine.
pub const POW_AUX_PREFIX: [u8; 4] = *b"PoW:";

/// Auxiliary data for PoW.
#[derive(Encode, Decode, Clone, Debug, Default)]
pub struct PowAux {
	pub total_difficulty: Difficulty,
}

/// Algorithm used for proof of work.
pub trait PowAlgorithm<B: BlockT> {
	/// Get the next block's difficulty.
	fn difficulty(&self, parent: &BlockId<B>) -> Result<Difficulty, String>;
	/// Verify proof of work against the given difficulty.
	fn verify(
		&self,
		parent: &BlockId<B>,
		pre_hash: &H256,
		seal: &Seal,
		difficulty: Difficulty
	) -> Result<bool, String>;
	/// Mine a seal that satisfy the given difficulty.
	fn mine(
		&self,
		parent: &BlockId<B>,
		pre_hash: &H256,
		seed: &H256,
		difficulty: Difficulty,
		round: u32
	) -> Result<Option<Seal>, String>;
}

/// Delegates algorithm selection to Substrate runtime.
pub struct PowRuntimeAlgorithm<C> {
	client: Arc<C>,
}

impl<C> PowRuntimeAlgorithm<C> {
	/// Create a new runtime algorithm given a client.
	pub fn new(client: Arc<C>) -> Self {
		Self { client }
	}
}

impl<B: BlockT<Hash=H256>, C> PowAlgorithm<B> for PowRuntimeAlgorithm<C> where
	C: ProvideRuntimeApi,
	C::Api: PowApi<B>,
{
	fn difficulty(&self, parent: &BlockId<B>) -> Result<Difficulty, String> {
		self.client.runtime_api().difficulty(parent)
			.map_err(|e| format!("Fetch difficulty failed: {:?}", e))
	}

	fn verify(
		&self,
		parent: &BlockId<B>,
		pre_hash: &H256,
		seal: &Seal,
		difficulty: Difficulty
	) -> Result<bool, String> {
		self.client.runtime_api().verify(parent, pre_hash, seal, difficulty)
			.map_err(|e| format!("Verify PoW seal failed: {:?}", e))
	}

	fn mine(
		&self,
		parent: &BlockId<B>,
		pre_hash: &H256,
		seed: &H256,
		difficulty: Difficulty,
		round: u32
	) -> Result<Option<Seal>, String> {
		self.client.runtime_api().mine(parent, pre_hash, seed, difficulty, round)
			.map_err(|e| format!("Mine PoW seal failed: {:?}", e))
	}
}

/// A verifier for PoW blocks.
pub struct PowVerifier<C, Algorithm> {
	client: Arc<C>,
	algorithm: Algorithm,
	inherent_data_providers: inherents::InherentDataProviders,
}

impl<C, Algorithm> PowVerifier<C, Algorithm> {
	fn check_header<B: BlockT<Hash=H256>>(
		&self,
		mut header: B::Header,
		block_id: BlockId<B>,
	) -> Result<(B::Header, Difficulty, DigestItem<H256>), String> where
		Algorithm: PowAlgorithm<B>,
	{
		let hash = header.hash();

		let (seal, inner_seal) = match header.digest_mut().pop() {
			Some(DigestItem::Seal(id, seal)) => {
				if id == POW_ENGINE_ID {
					(DigestItem::Seal(id, seal.clone()), seal)
				} else {
					return Err(format!("Header uses the wrong engine {:?}", id))
				}
			},
			_ => return Err(format!("Header {:?} is unsealed", hash)),
		};

		let pre_hash = header.hash();
		let difficulty = self.algorithm.difficulty(&block_id)?;

		if !self.algorithm.verify(
			&block_id,
			&pre_hash,
			&inner_seal,
			difficulty,
		)? {
			return Err("PoW validation error: invalid seal".into());
		}

		Ok((header, difficulty, seal))
	}

	fn check_inherents<B: BlockT<Hash=H256>>(
		&self,
		block: B,
		block_id: BlockId<B>,
		inherent_data: InherentData,
		timestamp_now: u64,
	) -> Result<(), String> where
		C: ProvideRuntimeApi, C::Api: BlockBuilderApi<B>
	{
		const MAX_TIMESTAMP_DRIFT_SECS: u64 = 60;

		let inherent_res = self.client.runtime_api().check_inherents(
			&block_id,
			block,
			inherent_data,
		).map_err(|e| format!("{:?}", e))?;

		if !inherent_res.ok() {
			inherent_res
				.into_errors()
				.try_for_each(|(i, e)| match TIError::try_from(&i, &e) {
					Some(TIError::ValidAtTimestamp(timestamp)) => {
						if timestamp > timestamp_now + MAX_TIMESTAMP_DRIFT_SECS {
							return Err("Rejecting block too far in future".into());
						}

						Ok(())
					},
					Some(TIError::Other(e)) => Err(e.into()),
					None => Err(self.inherent_data_providers.error_to_string(&i, &e)),
				})
		} else {
			Ok(())
		}
	}
}

impl<B: BlockT<Hash=H256>, C, Algorithm> Verifier<B> for PowVerifier<C, Algorithm> where
	C: ProvideRuntimeApi + Send + Sync + HeaderBackend<B> + AuxStore + ProvideCache<B> + BlockOf,
	C::Api: BlockBuilderApi<B>,
	Algorithm: PowAlgorithm<B> + Send + Sync,
{
	fn verify(
		&mut self,
		origin: BlockOrigin,
		header: B::Header,
		justification: Option<Justification>,
		mut body: Option<Vec<B::Extrinsic>>,
	) -> Result<(BlockImportParams<B>, Option<Vec<(CacheKeyId, Vec<u8>)>>), String> {
		let inherent_data = self.inherent_data_providers
			.create_inherent_data().map_err(String::from)?;
		let timestamp_now = inherent_data.timestamp_inherent_data().map_err(String::from)?;

		let best_hash = self.client.info().best_hash;
		let hash = header.hash();
		let parent_hash = *header.parent_hash();
		let parent_aux_key = POW_AUX_PREFIX.iter().chain(&parent_hash[..])
			.cloned().collect::<Vec<_>>();
		let best_aux_key = POW_AUX_PREFIX.iter().chain(&best_hash[..])
			.cloned().collect::<Vec<_>>();
		let best_aux = match self.client.get_aux(&best_aux_key)
			.map_err(|e| format!("{:?}", e))?
		{
			Some(bytes) => PowAux::decode(&mut &bytes[..]).map_err(|e| format!("{:?}", e))?,
			None => Default::default(),
		};
		let mut aux = match self.client.get_aux(&parent_aux_key)
			.map_err(|e| format!("{:?}", e))?
		{
			Some(bytes) => PowAux::decode(&mut &bytes[..]).map_err(|e| format!("{:?}", e))?,
			None => Default::default(),
		};

		let (checked_header, difficulty, seal) = self.check_header::<B>(
			header,
			BlockId::Hash(parent_hash),
		)?;
		aux.total_difficulty = aux.total_difficulty.saturating_add(difficulty);

		if let Some(inner_body) = body.take() {
			let block = B::new(checked_header.clone(), inner_body);

			self.check_inherents(
				block.clone(),
				BlockId::Hash(parent_hash),
				inherent_data,
				timestamp_now
			)?;

			let (_, inner_body) = block.deconstruct();
			body = Some(inner_body);
		}
		let key = POW_AUX_PREFIX.iter().chain(&hash[..]).cloned().collect::<Vec<_>>();
		let import_block = BlockImportParams {
			origin,
			header: checked_header,
			post_digests: vec![seal],
			body,
			finalized: false,
			justification,
			auxiliary: vec![(key, Some(aux.encode()))],
			fork_choice: ForkChoiceStrategy::Custom(aux.total_difficulty > best_aux.total_difficulty),
		};

		Ok((import_block, None))
	}
}

/// Register the PoW inherent data provider, if not registered already.
fn register_pow_inherent_data_provider(
	inherent_data_providers: &InherentDataProviders,
) -> Result<(), consensus_common::Error> {
	if !inherent_data_providers.has_provider(&srml_timestamp::INHERENT_IDENTIFIER) {
		inherent_data_providers
			.register_provider(srml_timestamp::InherentDataProvider)
			.map_err(Into::into)
			.map_err(consensus_common::Error::InherentData)
	} else {
		Ok(())
	}
}

/// The PoW import queue type.
pub type PowImportQueue<B> = BasicQueue<B>;

/// Import queue for PoW engine.
pub fn import_queue<B, C, Algorithm>(
	block_import: BoxBlockImport<B>,
	client: Arc<C>,
	algorithm: Algorithm,
	inherent_data_providers: InherentDataProviders,
) -> Result<PowImportQueue<B>, consensus_common::Error> where
	B: BlockT<Hash=H256>,
	C: ProvideRuntimeApi + HeaderBackend<B> + BlockOf + ProvideCache<B> + AuxStore,
	C: Send + Sync + AuxStore + 'static,
	C::Api: BlockBuilderApi<B> + PowApi<B>,
	Algorithm: PowAlgorithm<B> + Send + Sync + 'static,
{
	register_pow_inherent_data_provider(&inherent_data_providers)?;

	let verifier = PowVerifier {
		client: client.clone(),
		algorithm,
		inherent_data_providers,
	};

	Ok(BasicQueue::new(
		verifier,
		block_import,
		None,
		None
	))
}

/// Start the background mining thread for PoW. Note that because PoW mining
/// is CPU-intensive, it is not possible to use async future to define this.
/// However, it's not recommended to use background thread in the rest of the
/// codebase.
pub fn start_mine<B: BlockT<Hash=H256>, C, Algorithm, E>(
	mut block_import: BoxBlockImport<B>,
	client: Arc<C>,
	algorithm: Algorithm,
	mut env: E,
	preruntime: Vec<u8>,
	round: u32,
	inherent_data_providers: inherents::InherentDataProviders,
) where
	C: HeaderBackend<B> + AuxStore + ProvideRuntimeApi + 'static,
	C::Api: PowApi<B>,
	Algorithm: PowAlgorithm<B> + Send + Sync + 'static,
	E: Environment<B> + Send + Sync + 'static,
	E::Error: core::fmt::Debug,
{
	if let Err(_) = register_pow_inherent_data_provider(&inherent_data_providers) {
		warn!("Registering inherent data provider for timestamp failed");
	}

	thread::spawn(move || {
		loop {
			match mine_loop(
				&mut block_import,
				client.as_ref(),
				&algorithm,
				&mut env,
				&preruntime,
				round,
				&inherent_data_providers
			) {
				Ok(()) => (),
				Err(e) => error!(
					"Mining block failed with {:?}. Sleep for 1 second before restarting...",
					e
				),
			}

			std::thread::sleep(std::time::Duration::new(1, 0));
		}
	});
}

fn mine_loop<B: BlockT<Hash=H256>, C, Algorithm, E>(
	block_import: &mut BoxBlockImport<B>,
	client: &C,
	algorithm: &Algorithm,
	env: &mut E,
	preruntime: &[u8],
	round: u32,
	inherent_data_providers: &inherents::InherentDataProviders,
) -> Result<(), String> where
	C: HeaderBackend<B> + AuxStore,
	Algorithm: PowAlgorithm<B>,
	E: Environment<B>,
	E::Error: core::fmt::Debug,
{
	'outer: loop {
		let best_hash = client.info().best_hash;
		let best_header = client.header(BlockId::Hash(best_hash))
			.map_err(|e| format!("Best header does not exist: {:?}", e))?
			.ok_or("Fetching best header failed")?;
		let best_aux_key = POW_AUX_PREFIX.iter().chain(&best_hash[..])
			.cloned().collect::<Vec<_>>();
		let mut aux = match client.get_aux(&best_aux_key)
			.map_err(|e| format!("{:?}", e))?
		{
			Some(bytes) => PowAux::decode(&mut &bytes[..]).map_err(|e| format!("{:?}", e))?,
			None => Default::default(),
		};
		let mut proposer = env.init(&best_header).map_err(|e| format!("{:?}", e))?;

		let inherent_data = inherent_data_providers
			.create_inherent_data().map_err(String::from)?;
		let mut inherent_digest = Digest::default();
		inherent_digest.push(DigestItem::PreRuntime(POW_ENGINE_ID, preruntime.to_vec()));
		let block = futures::executor::block_on(proposer.propose(
			inherent_data,
			inherent_digest,
			std::time::Duration::new(0, 0)
		)).map_err(|e| format!("Block proposing error: {:?}", e))?;

		let (header, body) = block.deconstruct();
		let seed = H256::random();
		let (difficulty, seal) = {
			loop {
				let difficulty = algorithm.difficulty(
					&BlockId::Hash(best_hash),
				)?;

				let seal = algorithm.mine(
					&BlockId::Hash(best_hash),
					&header.hash(),
					&seed,
					difficulty,
					round,
				)?;

				if let Some(seal) = seal {
					break (difficulty, seal)
				}

				if best_hash != client.info().best_hash {
					continue 'outer
				}
			}
		};

		aux.total_difficulty = aux.total_difficulty.saturating_add(difficulty);
		let hash = header.hash();

		let key = POW_AUX_PREFIX.iter().chain(&hash[..]).cloned().collect::<Vec<_>>();
		let import_block = BlockImportParams {
			origin: BlockOrigin::Own,
			header,
			justification: None,
			post_digests: vec![DigestItem::Seal(POW_ENGINE_ID, seal)],
			body: Some(body),
			finalized: false,
			auxiliary: vec![(key, Some(aux.encode()))],
			fork_choice: ForkChoiceStrategy::Custom(true),
		};

		block_import.import_block(import_block, Default::default())
			.map_err(|e| format!("Error with block built on {:?}: {:?}", best_hash, e))?;
	}
}
