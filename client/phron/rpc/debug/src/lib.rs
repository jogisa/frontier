pub use fc_rpc_core_debug::{DebugServer, TraceParams};
use futures::StreamExt;
use jsonrpsee::core::{async_trait, RpcResult};

use tokio::{
	self,
	sync::{oneshot, Semaphore},
};

use ethereum_types::H256;
use fc_evm_tracing::tracer::{
	types::{SingleTransactionTrace, TraceType},
	ResponseLayout,
};
use fc_rpc::{frontier_backend_client, internal_err, OverrideHandle};
use fc_rpc_core_types::{RequestBlockId, RequestBlockTag};
use fp_rpc_phron::{DebugRuntimeApi, EthereumRuntimeRPCApi, TracerInput};
use sc_client_api::backend::{Backend, StateBackend, StorageProvider};
use sc_utils::mpsc::TracingUnboundedSender;

//use sp_api::{ApiExt, BlockId, Core, HeaderT, ProvideRuntimeApi};
use sp_api::{ApiExt, Core, ProvideRuntimeApi};
pub use sp_runtime::{generic::BlockId, traits::Header as HeaderT};

use sp_block_builder::BlockBuilder;
use sp_blockchain::{
	Backend as BlockchainBackend, Error as BlockChainError, HeaderBackend, HeaderMetadata,
};
use sp_runtime::traits::{BlakeTwo256, Block as BlockT, UniqueSaturatedInto};
use std::{future::Future, marker::PhantomData, sync::Arc};

pub enum RequesterInput {
	Transaction(H256),
	Block(RequestBlockId),
}

pub enum Response {
	Single(SingleTransactionTrace),
	Block(Vec<SingleTransactionTrace>),
}

pub type Responder = oneshot::Sender<RpcResult<Response>>;
pub type DebugRequester =
	TracingUnboundedSender<((RequesterInput, Option<TraceParams>), Responder)>;

pub struct Debug {
	pub requester: DebugRequester,
}

impl Debug {
	pub fn new(requester: DebugRequester) -> Self {
		Self { requester }
	}
}

#[async_trait]
impl DebugServer for Debug {
	/// Handler for `debug_traceTransaction` request. Communicates with the service-defined task
	/// using channels.
	async fn trace_transaction(
		&self,
		transaction_hash: H256,
		params: Option<TraceParams>,
	) -> RpcResult<SingleTransactionTrace> {
		let requester = self.requester.clone();

		let (tx, rx) = oneshot::channel();
		// Send a message from the rpc handler to the service level task.
		requester
			.unbounded_send(((RequesterInput::Transaction(transaction_hash), params), tx))
			.map_err(|err| {
				internal_err(format!(
					"failed to send request to debug service : {:?}",
					err
				))
			})?;

		// Receive a message from the service level task and send the rpc response.
		rx.await
			.map_err(|err| internal_err(format!("debug service dropped the channel : {:?}", err)))?
			.map(|res| match res {
				Response::Single(res) => res,
				_ => unreachable!(),
			})
	}

	async fn trace_block(
		&self,
		id: RequestBlockId,
		params: Option<TraceParams>,
	) -> RpcResult<Vec<SingleTransactionTrace>> {
		let requester = self.requester.clone();

		let (tx, rx) = oneshot::channel();
		// Send a message from the rpc handler to the service level task.
		requester
			.unbounded_send(((RequesterInput::Block(id), params), tx))
			.map_err(|err| {
				internal_err(format!(
					"failed to send request to debug service : {:?}",
					err
				))
			})?;

		// Receive a message from the service level task and send the rpc response.
		rx.await
			.map_err(|err| internal_err(format!("debug service dropped the channel : {:?}", err)))?
			.map(|res| match res {
				Response::Block(res) => res,
				_ => unreachable!(),
			})
	}
}

pub struct DebugHandler<B: BlockT, C, BE>(PhantomData<(B, C, BE)>);

impl<B, C, BE> DebugHandler<B, C, BE>
where
	BE: Backend<B> + 'static,
	BE::State: StateBackend<BlakeTwo256>,
	C: ProvideRuntimeApi<B>,
	C: StorageProvider<B, BE>,
	C: HeaderMetadata<B, Error = BlockChainError> + HeaderBackend<B>,
	C: Send + Sync + 'static,
	B: BlockT<Hash = H256> + Send + Sync + 'static,
	C::Api: BlockBuilder<B>,
	C::Api: DebugRuntimeApi<B>,
	C::Api: EthereumRuntimeRPCApi<B>,
	C::Api: ApiExt<B>,
{
	pub fn task(
		client: Arc<C>,
		backend: Arc<BE>,
		frontier_backend: Arc<dyn fc_api::Backend<B> + Send + Sync>,
		permit_pool: Arc<Semaphore>,
		overrides: Arc<OverrideHandle<B>>,
		raw_max_memory_usage: usize,
	) -> (impl Future<Output = ()>, DebugRequester) {
		// Create a channel for sending and receiving debug requests
		let (debug_requester_tx, mut debug_requester_rx): (DebugRequester, _) =
			sc_utils::mpsc::tracing_unbounded("debug-requester", 100_000);

		// Define the asynchronous task that processes incoming requests
		let fut = async move {
			loop {
				match debug_requester_rx.next().await {
					Some((
						(RequesterInput::Transaction(transaction_hash), params),
						response_tx,
					)) => {
						// Clone necessary variables for handling transaction requests
						let client_clone = client.clone();
						let backend_clone = backend.clone();
						let frontier_backend_clone = frontier_backend.clone();
						let permit_pool_clone = permit_pool.clone();
						let overrides_clone = overrides.clone();

						// Spawn a Tokio task to handle the transaction request
						tokio::task::spawn(async move {
							let _ = response_tx.send(
								async {
									let _permit = permit_pool_clone.acquire().await;
									tokio::task::spawn_blocking(move || {
										Self::handle_transaction_request(
											client_clone,
											backend_clone,
											frontier_backend_clone,
											transaction_hash,
											params,
											overrides_clone,
											raw_max_memory_usage,
										)
									})
									.await
									.map_err(|e| {
										internal_err(format!(
											"Internal error on spawned task: {:?}",
											e
										))
									})?
								}
								.await,
							);
						});
					}
					Some(((RequesterInput::Block(request_block_id), params), response_tx)) => {
						// Clone necessary variables for handling block requests
						let client_clone = client.clone();
						let backend_clone = backend.clone();
						let frontier_backend_clone = frontier_backend.clone();
						let permit_pool_clone = permit_pool.clone();
						let overrides_clone = overrides.clone();

						// Spawn a Tokio task to handle the block request
						tokio::task::spawn(async move {
							let _ = response_tx.send(
								async {
									let _permit = permit_pool_clone.acquire().await;

									tokio::task::spawn_blocking(move || {
										Self::handle_block_request(
											client_clone,
											backend_clone,
											frontier_backend_clone,
											request_block_id,
											params,
											overrides_clone,
										)
									})
									.await
									.map_err(|e| {
										internal_err(format!(
											"Internal error on spawned task: {:?}",
											e
										))
									})?
								}
								.await,
							);
						});
					}
					_ => {} // Handle other cases where no action is needed
				}
			}
		};

		(fut, debug_requester_tx)
	}

	fn handle_trace_params(params: Option<TraceParams>) -> RpcResult<(TracerInput, TraceType)> {
		// Constants for code hashes
		const EXPLORER_JS_CODE_HASH: [u8; 16] =
			hex_literal::hex!("94d9f08796f91eb13a2e82a6066882f7");
		const EXPLORER_JS_CODE_HASH_V2: [u8; 16] =
			hex_literal::hex!("89db13694675692951673a1e6e18ff02");

		match params {
			// Handle case where tracer field is Some
			Some(TraceParams {
				tracer: Some(tr), ..
			}) => {
				// Calculate hash based on the tracer
				let hash = sp_io::hashing::twox_128(&tr.as_bytes());

				// Determine the tracer based on the hash
				let tracer = match hash {
					EXPLORER_JS_CODE_HASH | EXPLORER_JS_CODE_HASH_V2 => Some(TracerInput::Explorer),
					_ if tr == "callTracer" => Some(TracerInput::CallTracer),
					_ => None,
				};

				// Check if tracer is Some and return the result
				if let Some(tr) = tracer {
					Ok((tr, TraceType::CallList))
				} else {
					return Err(internal_err(format!(
						"javascript based tracing is not available (hash :{:?})",
						hash
					)));
				}
			}

			// Handle case where params is Some but tracer field is None
			Some(p) => Ok((
				TracerInput::None,
				TraceType::Raw {
					disable_storage: p.disable_storage.unwrap_or(false),
					disable_memory: p.disable_memory.unwrap_or(false),
					disable_stack: p.disable_stack.unwrap_or(false),
				},
			)),

			// Handle case where params is None
			None => Ok((
				TracerInput::None,
				TraceType::Raw {
					disable_storage: false,
					disable_memory: false,
					disable_stack: false,
				},
			)),
		}
	}

	fn handle_block_request(
		client: Arc<C>,
		backend: Arc<BE>,
		frontier_backend: Arc<dyn fc_api::Backend<B> + Send + Sync>,
		request_block_id: RequestBlockId,
		params: Option<TraceParams>,
		overrides: Arc<OverrideHandle<B>>,
	) -> RpcResult<Response> {
		let (tracer_input, trace_type) = Self::handle_trace_params(params)?;

		let reference_id: BlockId<B> = match request_block_id {
			RequestBlockId::Number(n) => Ok(BlockId::Number(n.unique_saturated_into())),
			RequestBlockId::Tag(RequestBlockTag::Latest) => {
				Ok(BlockId::Number(client.info().best_number))
			}
			RequestBlockId::Tag(RequestBlockTag::Earliest) => {
				Ok(BlockId::Number(0u32.unique_saturated_into()))
			}
			RequestBlockId::Tag(RequestBlockTag::Pending) => {
				Err(internal_err("'pending' blocks are not supported"))
			}
			RequestBlockId::Hash(eth_hash) => {
				match futures::executor::block_on(frontier_backend_client::load_hash::<B, C>(
					client.as_ref(),
					frontier_backend.as_ref(),
					eth_hash,
				)) {
					Ok(Some(hash)) => Ok(BlockId::Hash(hash)),
					Ok(_) => Err(internal_err("Block hash not found".to_string())),
					Err(e) => Err(e),
				}
			}
		}?;

		// Get ApiRef. This handle allow to keep changes between txs in an internal buffer.
		let api = client.runtime_api();
		// Get Blockchain backend
		let blockchain = backend.blockchain();
		// Get the header I want to work with.
		let Ok(hash) = client.expect_block_hash_from_id(&reference_id) else {
			return Err(internal_err("Block header not found"));
		};
		let header = match client.header(hash) {
			Ok(Some(h)) => h,
			_ => return Err(internal_err("Block header not found")),
		};

		// Get parent blockid.
		let parent_block_hash = *header.parent_hash();

		let schema = fc_storage::onchain_storage_schema::<B, C, BE>(client.as_ref(), hash);

		// Using storage overrides we align with `:ethereum_schema` which will result in proper
		// SCALE decoding in case of migration.
		let statuses = match overrides.schemas.get(&schema) {
			Some(schema) => schema
				.current_transaction_statuses(hash)
				.unwrap_or_default(),
			_ => {
				return Err(internal_err(format!(
					"No storage override at {:?}",
					reference_id
				)))
			}
		};

		// Known ethereum transaction hashes.
		let eth_tx_hashes: Vec<_> = statuses.iter().map(|t| t.transaction_hash).collect();

		// If there are no ethereum transactions in the block return empty trace right away.
		if eth_tx_hashes.is_empty() {
			return Ok(Response::Block(vec![]));
		}

		// Get block extrinsics.
		let exts = blockchain
			.body(hash)
			.map_err(|e| internal_err(format!("Fail to read blockchain db: {:?}", e)))?
			.unwrap_or_default();

		// Trace the block.
		let f = || -> RpcResult<_> {
			api.initialize_block(parent_block_hash, &header)
				.map_err(|e| internal_err(format!("Runtime api access error: {:?}", e)))?;

			let _result = api
				.trace_block(parent_block_hash, exts, eth_tx_hashes)
				.map_err(|e| {
					internal_err(format!(
						"Blockchain error when replaying block {} : {:?}",
						reference_id, e
					))
				})?
				.map_err(|e| {
					internal_err(format!(
						"Internal runtime error when replaying block {} : {:?}",
						reference_id, e
					))
				})?;
			Ok(fp_rpc_phron::Response::Block)
		};

		return match trace_type {
			TraceType::CallList => {
				let mut proxy = fc_evm_tracing::tracer::CallList::default();
				proxy.using(f)?;
				proxy.finish_transaction();
				let response = match tracer_input {
					TracerInput::CallTracer => fc_evm_tracing::tracer::CallTracer::format(proxy)
						.ok_or("Trace result is empty.")
						.map_err(|e| internal_err(format!("{:?}", e))),
					_ => Err(internal_err(
						"Bug: failed to resolve the tracer format.".to_string(),
					)),
				}?;

				Ok(Response::Block(response))
			}
			_ => Err(internal_err(
				"debug_traceBlock functions currently only support callList mode (enabled
				by providing `{{'tracer': 'callTracer'}}` in the request)."
					.to_string(),
			)),
		};
	}

	/// Processes a transaction request by replaying it within the Runtime environment.
	///
	/// This function handles the replay of a transaction identified by its hash at a specified block height.
	/// It interacts with various components such as the client, backend, and frontier backend to retrieve
	/// transaction details and execute the replay operation.
	///
	/// The function supports handling trace parameters, loading transaction details, obtaining block hashes,
	/// accessing the API reference, and executing the transaction replay based on the provided inputs.
	///
	/// The replay operation involves initializing the block, tracing the transaction, and generating the
	/// appropriate response based on the trace type specified. The function also ensures proper error handling
	/// and validation throughout the process.
	///
	/// Returns a `RpcResult` containing the response of the transaction replay operation.

	fn handle_transaction_request(
		client: Arc<C>,
		backend: Arc<BE>,
		frontier_backend: Arc<dyn fc_api::Backend<B> + Send + Sync>,
		transaction_hash: H256,
		trace_params: Option<TraceParams>,
		overrides: Arc<OverrideHandle<B>>,
		raw_max_memory_usage: usize,
	) -> RpcResult<Response> {
		// Handle trace parameters
		let (tracer_input, trace_type) = match Self::handle_trace_params(trace_params) {
			Ok(data) => data,
			Err(e) => {
				return Err(internal_err(format!(
					"Failed to handle trace params: {:?}",
					e
				)))
			}
		};

		// Load transaction details
		let (hash, index) =
			match futures::executor::block_on(frontier_backend_client::load_transactions::<B, C>(
				client.as_ref(),
				frontier_backend.as_ref(),
				transaction_hash,
				false,
			)) {
				Ok(Some((hash, index))) => (hash, index as usize),
				Ok(None) => return Err(internal_err("Transaction hash not found")),
				Err(e) => {
					return Err(internal_err(format!(
						"Failed to load transactions: {:?}",
						e
					)))
				}
			};

		// Load block hash
		let reference_id =
			match futures::executor::block_on(frontier_backend_client::load_hash::<B, C>(
				client.as_ref(),
				frontier_backend.as_ref(),
				hash,
			)) {
				Ok(Some(hash)) => BlockId::Hash(hash),
				Ok(_) => return Err(internal_err("Block hash not found")),
				Err(e) => return Err(internal_err(format!("Failed to load block hash: {:?}", e))),
			};

		// Get API reference
		let api = client.runtime_api();
		// Get blockchain backend
		let blockchain = backend.blockchain();

		// Expect block hash from ID
		let reference_hash = match client.expect_block_hash_from_id(&reference_id) {
			Ok(result) => result,
			Err(_) => return Err(internal_err("Block header not found")),
		};

		// Retrieve header
		let header = match client.header(reference_hash) {
			Ok(Some(h)) => h,
			_ => return Err(internal_err("Block header not found")),
		};

		// Retrieve parent block hash
		let parent_block_hash = *header.parent_hash();

		// Get block extrinsics
		let exts = blockchain
			.body(reference_hash)
			.map_err(|e| internal_err(format!("Failed to read blockchain db: {:?}", e)))?
			.unwrap_or_default();

		// Get trace API version
		let trace_api_version = if let Ok(Some(api_version)) =
			api.api_version::<dyn DebugRuntimeApi<B>>(parent_block_hash)
		{
			api_version
		} else {
			return Err(internal_err("Runtime API version call failed (trace)"));
		};

		let schema =
			fc_storage::onchain_storage_schema::<B, C, BE>(client.as_ref(), reference_hash);

		// Get reference block
		let reference_block = match overrides.schemas.get(&schema) {
			Some(schema) => schema.current_block(reference_hash),
			_ => {
				return Err(internal_err(format!(
					"No storage override at {:?}",
					reference_hash
				)))
			}
		};

		// Retrieve transaction details
		if let Some(block) = reference_block {
			let transactions = block.transactions;
			if let Some(transaction) = transactions.get(index) {
				let f = || -> RpcResult<_> {
					// Initialize block
					api.initialize_block(parent_block_hash, &header)
						.map_err(|e| internal_err(format!("Runtime API access error: {:?}", e)))?;

					if trace_api_version >= 4 {
						let _result = api
							.trace_transaction(parent_block_hash, exts, &transaction)
							.map_err(|e| {
								internal_err(format!(
									"Runtime API access error (version {:?}): {:?}",
									trace_api_version, e
								))
							})?
							.map_err(|e| internal_err(format!("DispatchError: {:?}", e)))?;
					} else {
						// Legacy transactions
						let _result = match transaction {
							ethereum::TransactionV2::Legacy(tx) =>
							{
								#[allow(deprecated)]
								api.trace_transaction_before_version_4(parent_block_hash, exts, &tx)
									.map_err(|e| {
										internal_err(format!(
											"Runtime API access error (legacy): {:?}",
											e
										))
									})?
									.map_err(|e| internal_err(format!("DispatchError: {:?}", e)))?
							}
							_ => {
								return Err(internal_err(
									"Bug: pre-London runtime expects legacy transactions",
								))
							}
						};
					}

					Ok(fp_rpc_phron::Response::Single)
				};

				return match trace_type {
					TraceType::Raw {
						disable_storage,
						disable_memory,
						disable_stack,
					} => {
						let mut proxy = fc_evm_tracing::tracer::RawL::new(
							disable_storage,
							disable_memory,
							disable_stack,
							raw_max_memory_usage,
						);
						proxy.using(f)?;
						Ok(Response::Single(
							fc_evm_tracing::tracer::Raw::format(proxy)
								.ok_or(internal_err("Replayed transaction generated too much data. Try disabling memory or storage?"))?,
						))
					}
					TraceType::CallList => {
						let mut proxy = fc_evm_tracing::tracer::CallList::default();
						proxy.using(f)?;
						proxy.finish_transaction();
						let response = match tracer_input {
							TracerInput::Explorer => {
								fc_evm_tracing::tracer::Explorer::format(proxy)
									.ok_or("Trace result is empty.")
									.map_err(|e| internal_err(format!("{:?}", e)))
							}
							TracerInput::CallTracer => {
								let mut res = fc_evm_tracing::tracer::CallTracer::format(proxy)
									.ok_or("Trace result is empty.")
									.map_err(|e| internal_err(format!("{:?}", e)))?;
								Ok(res.pop().expect("Trace result is empty."))
							}
							_ => Err(internal_err("Bug: Failed to resolve the tracer format.")),
						}?;
						Ok(Response::Single(response))
					}
					not_supported => Err(internal_err(format!(
						"Bug: `handle_transaction_request` does not support {:?}.",
						not_supported
					))),
				};
			}
		}

		Err(internal_err("Runtime block call failed"))
	}
}
