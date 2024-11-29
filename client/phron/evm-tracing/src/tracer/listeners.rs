use std::{collections::BTreeMap, vec::Vec};

use super::{
	calls::{ExplorerCall as Call, ExplorerCallInner as CallInner},
	convert_memory,
	types::RawStepLog,
	CallResult, CallType, ContextType, CreateResult,
};
use ethereum_types::{H160, H256, U256};
use evm::{
	events::{Event, EvmEvent, Listener as ListenerT, StepEventFilter},
	runtime::{Capture, ExitError, ExitReason, ExitSucceed, RuntimeEvent},
};
use evm_gasometer::events::GasometerEvent;

/// Enum of the different "modes" of tracer for multiple runtime versions and
/// the kind of EVM events that are emitted.
enum TracingVersion {
	/// The first event of the transaction is `EvmEvent::TransactX`. It goes along other events
	/// such as `EvmEvent::Exit`. All contexts should have clear start/end boundaries.
	EarlyTransact,
	/// Older version in which the events above didn't existed.
	/// It means that we cannot rely on those events to perform any task, and must rely only
	/// on other events.
	Legacy,
}

pub struct ListenerCl {
	/// Version of the tracing.
	/// Defaults to legacy, and switch to a more modern version if recently added events are
	/// received.
	version: TracingVersion,

	// Transaction cost that must be added to the first context cost.
	transaction_cost: u64,

	// Final logs.
	pub entries: Vec<BTreeMap<u32, Call>>,
	// Next index to use.
	entries_next_index: u32,
	// Stack of contexts with data to keep between events.
	context_stack: Vec<Context>,

	// Type of the next call.
	// By default is None and corresponds to the root call, which
	// can be determined using the `is_static` field of the `Call` event.
	// Then by looking at call traps events we can set this value to the correct
	// call type, to be used when the following `Call` event is received.
	call_type: Option<CallType>,

	/// When `EvmEvent::TransactX` is received it creates its own context. However it will usually
	/// be followed by an `EvmEvent::Call/Create` that will also create a context, which must be
	/// prevented. It must however not be skipped if `EvmEvent::TransactX` was not received
	/// (in legacy mode).
	skip_next_context: bool,

	// /// To handle EvmEvent::Exit no emitted by previous runtimes versions,
	// /// entries are not inserted directly in `self.entries`.
	// pending_entries: Vec<(u32, Call)>,
	/// See `RuntimeEvent::StepResult` event explanatioins.
	step_result_entry: Option<(u32, Call)>,

	/// When tracing a block `Event::CallListNew` is emitted before each Ethereum transaction is
	/// processed. Since we use that event to **finish** the transaction, we must ignore the first
	/// one.
	call_list_first_transaction: bool,

	/// True if only the `GasometerEvent::RecordTransaction` event has been received.
	/// Allow to correctly handle transactions that cannot pay for the tx data in Legacy mode.
	record_transaction_event_only: bool,
}

struct Context {
	entries_index: u32,

	context_type: ContextType,

	from: H160,
	trace_address: Vec<u32>,
	subtraces: u32,
	value: U256,

	gas: u64,
	start_gas: Option<u64>,

	// input / data
	data: Vec<u8>,
	// to / create address
	to: H160,
}

impl Default for ListenerCl {
	fn default() -> Self {
		Self {
			version: TracingVersion::Legacy,
			transaction_cost: 0,

			entries: vec![],
			entries_next_index: 0,

			context_stack: vec![],

			call_type: None,
			step_result_entry: None,
			skip_next_context: false,
			call_list_first_transaction: true,
			record_transaction_event_only: false,
		}
	}
}

impl ListenerCl {
	pub fn using<R, F: FnOnce() -> R>(&mut self, f: F) -> R {
		evm::events::using(self, f)
	}

	/// Called at the end of each transaction when tracing.
	/// Allow to insert the pending entries regardless of which runtime version
	/// is used (with or without EvmEvent::Exit).
	pub fn finish_transaction(&mut self) {
		// remove any leftover context
		let mut context_stack = vec![];
		core::mem::swap(&mut self.context_stack, &mut context_stack);

		// if there is a left over there have been an early exit.
		// we generate an entry from it and discord any inner context.
		if let Some(context) = context_stack.into_iter().next() {
			let mut gas_used = context.start_gas.unwrap_or(0) - context.gas;
			if context.entries_index == 0 {
				gas_used += self.transaction_cost;
			}

			let entry = match context.context_type {
				ContextType::Call(call_type) => {
					let res = CallResult::Error(
						b"early exit (out of gas, stack overflow, direct call to precompile, ...)"
							.to_vec(),
					);
					Call {
						from: context.from,
						trace_address: context.trace_address,
						subtraces: context.subtraces,
						value: context.value,
						gas: context.gas.into(),
						gas_used: gas_used.into(),
						inner: CallInner::Call {
							call_type,
							to: context.to,
							input: context.data,
							res,
						},
					}
				}
				ContextType::Create => {
					let res = CreateResult::Error {
							error: b"early exit (out of gas, stack overflow, direct call to precompile, ...)".to_vec(),
						};

					Call {
						value: context.value,
						trace_address: context.trace_address,
						subtraces: context.subtraces,
						gas: context.gas.into(),
						gas_used: gas_used.into(),
						from: context.from,
						inner: CallInner::Create {
							init: context.data,
							res,
						},
					}
				}
			};

			self.insert_entry(context.entries_index, entry);
			// Since only this context/entry is kept, we need update entries_next_index too.
			self.entries_next_index = context.entries_index + 1;
		}
		// However if the transaction had a too low gas limit to pay for the data cost itself,
		// and `EvmEvent::Exit` is not emitted in **Legacy mode**, then it has never produced any
		// context (and exited **early in the transaction**).
		else if self.record_transaction_event_only {
			let res = CallResult::Error(
				b"transaction could not pay its own data cost (impossible to gather more info)"
					.to_vec(),
			);

			let entry = Call {
				from: H160::repeat_byte(0),
				trace_address: vec![],
				subtraces: 0,
				value: 0.into(),
				gas: 0.into(),
				gas_used: 0.into(),
				inner: CallInner::Call {
					call_type: CallType::Call,
					to: H160::repeat_byte(0),
					input: vec![],
					res,
				},
			};

			self.insert_entry(self.entries_next_index, entry);
			self.entries_next_index += 1;
		}
	}

	/// Updates internal state based on the type of Gasometer event received.
	pub fn gasometer_event(&mut self, event: GasometerEvent) {
		match event {
			// Handle cost recording events
			GasometerEvent::RecordCost { snapshot, .. }
			| GasometerEvent::RecordDynamicCost { snapshot, .. }
			| GasometerEvent::RecordStipend { snapshot, .. } => {
				if let Some(current_context) = self.context_stack.last_mut() {
					// Update start_gas if it is not already set
					if current_context.start_gas.is_none() {
						current_context.start_gas = Some(snapshot.gas());
					}
					// Update current gas value
					current_context.gas = snapshot.gas();
				}
			}
			// Handle transaction recording events
			GasometerEvent::RecordTransaction { cost, .. } => {
				self.transaction_cost = cost;
				self.record_transaction_event_only = true;
			}
			// Ignore other types of events (new ones may be added in the future)
			#[allow(unreachable_patterns)]
			_ => (),
		}
	}

	pub fn runtime_event(&mut self, event: RuntimeEvent) {
		match event {
			RuntimeEvent::StepResult {
				result: Err(Capture::Trap(opcode)),
				..
			} => {
				if let Some(ContextType::Call(call_type)) = ContextType::from(opcode) {
					// Set the call type if the opcode corresponds to a call operation.
					self.call_type = Some(call_type);
				}
			}
			RuntimeEvent::StepResult {
				result: Err(Capture::Exit(reason)),
				return_value,
			} => {
				if let Some((key, entry)) = self.pop_context_to_entry(reason, return_value) {
					match self.version {
						TracingVersion::Legacy => {
							// Insert the entry directly in Legacy mode.
							self.insert_entry(key, entry);
						}
						TracingVersion::EarlyTransact => {
							// Store the generated entry to be used in EvmEvent::Exit.
							// This entry will be handled in EvmEvent::Exit for all cases.
							self.step_result_entry = Some((key, entry));
						}
					}
				}
			}
			// Ignore other types of messages (new ones may be added in the future).
			#[allow(unreachable_patterns)]
			_ => (),
		}
	}

	////////////////////////////////////////////////////////////
	pub fn evm_event(&mut self, event: EvmEvent) {
		match event {
			EvmEvent::TransactCall {
				caller,
				address,
				value,
				data,
				..
			} => {
				self.record_transaction_event_only = false;
				self.version = TracingVersion::EarlyTransact;
				self.context_stack.push(Context {
					entries_index: self.entries_next_index,
					context_type: ContextType::Call(CallType::Call),
					from: caller,
					trace_address: vec![],
					subtraces: 0,
					value,
					gas: 0,
					start_gas: None,
					data,
					to: address,
				});

				self.entries_next_index += 1;
				self.skip_next_context = true;
			}

			EvmEvent::TransactCreate {
				caller,
				value,
				init_code,
				address,
				..
			}
			| EvmEvent::TransactCreate2 {
				caller,
				value,
				init_code,
				address,
				..
			} => {
				self.record_transaction_event_only = false;
				self.version = TracingVersion::EarlyTransact;
				self.context_stack.push(Context {
					entries_index: self.entries_next_index,
					context_type: ContextType::Create,
					from: caller,
					trace_address: vec![],
					subtraces: 0,
					value,
					gas: 0,
					start_gas: None,
					data: init_code,
					to: address,
				});

				self.entries_next_index += 1;
				self.skip_next_context = true;
			}

			EvmEvent::Call {
				code_address,
				input,
				is_static,
				context,
				..
			} => {
				self.record_transaction_event_only = false;

				let call_type = match (self.call_type, is_static) {
					(None, true) => CallType::StaticCall,
					(None, false) => CallType::Call,
					(Some(call_type), _) => call_type,
				};

				if !self.skip_next_context {
					let trace_address = if let Some(context) = self.context_stack.last_mut() {
						let mut trace_address = context.trace_address.clone();
						trace_address.push(context.subtraces);
						context.subtraces += 1;
						trace_address
					} else {
						vec![]
					};

					let from = if let Some(parent_context) = self.context_stack.last() {
						parent_context.to.clone()
					} else {
						context.caller
					};

					self.context_stack.push(Context {
						entries_index: self.entries_next_index,
						context_type: ContextType::Call(call_type),
						from,
						trace_address,
						subtraces: 0,
						value: context.apparent_value,
						gas: 0,
						start_gas: None,
						data: input.to_vec(),
						to: code_address,
					});

					self.entries_next_index += 1;
				} else {
					self.skip_next_context = false;
				}
			}

			EvmEvent::Create {
				caller,
				address,
				value,
				init_code,
				..
			} => {
				self.record_transaction_event_only = false;

				if !self.skip_next_context {
					let trace_address = if let Some(context) = self.context_stack.last_mut() {
						let mut trace_address = context.trace_address.clone();
						trace_address.push(context.subtraces);
						context.subtraces += 1;
						trace_address
					} else {
						vec![]
					};

					self.context_stack.push(Context {
						entries_index: self.entries_next_index,
						context_type: ContextType::Create,
						from: caller,
						trace_address,
						subtraces: 0,
						value,
						gas: 0,
						start_gas: None,
						data: init_code.to_vec(),
						to: address,
					});

					self.entries_next_index += 1;
				} else {
					self.skip_next_context = false;
				}
			}

			EvmEvent::Suicide {
				address,
				target,
				balance,
			} => {
				let trace_address = if let Some(context) = self.context_stack.last_mut() {
					let mut trace_address = context.trace_address.clone();
					trace_address.push(context.subtraces);
					context.subtraces += 1;
					trace_address
				} else {
					vec![]
				};

				self.insert_entry(
					self.entries_next_index,
					Call {
						from: address,
						trace_address,
						subtraces: 0,
						value: 0.into(),
						gas: 0.into(),
						gas_used: 0.into(),
						inner: CallInner::SelfDestruct {
							to: target,
							balance,
						},
					},
				);
				self.entries_next_index += 1;
			}

			EvmEvent::Exit {
				reason,
				return_value,
			} => {
				self.record_transaction_event_only = false;

				let entry = self
					.step_result_entry
					.take()
					.or_else(|| self.pop_context_to_entry(reason, return_value));

				if let Some((key, entry)) = entry {
					self.insert_entry(key, entry);
				}
			}

			EvmEvent::PrecompileSubcall { .. } => {
				self.call_type = Some(CallType::Call);
			}

			// We ignore other kinds of events (new ones may be added in the future).
			#[allow(unreachable_patterns)]
			_ => (),
		}
	}

	////////////////////////////////////////////////////////////

	fn insert_entry(&mut self, key: u32, entry: Call) {
		if let Some(ref mut last) = self.entries.last_mut() {
			last.insert(key, entry);
		} else {
			let mut btree_map = BTreeMap::new();
			btree_map.insert(key, entry);
			self.entries.push(btree_map);
		}
	}

	fn pop_context_to_entry(
		&mut self,
		reason: ExitReason,
		return_value: Vec<u8>,
	) -> Option<(u32, Call)> {
		if let Some(context) = self.context_stack.pop() {
			let mut gas_used = context.start_gas.unwrap_or(0) - context.gas;
			if context.entries_index == 0 {
				gas_used += self.transaction_cost;
			}

			Some((
				context.entries_index,
				match context.context_type {
					ContextType::Call(call_type) => {
						let call_result = match &reason {
							ExitReason::Succeed(ExitSucceed::Returned) => {
								CallResult::Output(return_value.to_vec())
							}
							ExitReason::Succeed(_) => CallResult::Output(vec![]),
							ExitReason::Error(error) => CallResult::Error(error_message(error)),
							ExitReason::Revert(_) => {
								CallResult::Error(b"execution reverted".to_vec())
							}
							ExitReason::Fatal(_) => CallResult::Error(vec![]),
						};

						Call {
							from: context.from,
							trace_address: context.trace_address,
							subtraces: context.subtraces,
							value: context.value,
							gas: context.gas.into(),
							gas_used: gas_used.into(),
							inner: CallInner::Call {
								call_type,
								to: context.to,
								input: context.data,
								res: call_result,
							},
						}
					}
					ContextType::Create => {
						let create_result = match &reason {
							ExitReason::Succeed(_) => CreateResult::Success {
								created_contract_address_hash: context.to,
								created_contract_code: return_value.to_vec(),
							},
							ExitReason::Error(error) => CreateResult::Error {
								error: error_message(error),
							},
							ExitReason::Revert(_) => CreateResult::Error {
								error: b"execution reverted".to_vec(),
							},
							ExitReason::Fatal(_) => CreateResult::Error { error: vec![] },
						};

						Call {
							value: context.value,
							trace_address: context.trace_address,
							subtraces: context.subtraces,
							gas: context.gas.into(),
							gas_used: gas_used.into(),
							from: context.from,
							inner: CallInner::Create {
								init: context.data,
								res: create_result,
							},
						}
					}
				},
			))
		} else {
			None
		}
	}
}

fn error_message(error: &ExitError) -> Vec<u8> {
	match error {
		ExitError::StackOverflow => "stack overflow",
		ExitError::StackUnderflow => "stack underflow",
		ExitError::InvalidJump => "invalid jump",
		ExitError::InvalidRange => "invalid range",
		ExitError::CreateContractLimit => "create contract limit",
		ExitError::OutOfFund => "out of funds",
		ExitError::DesignatedInvalid => "designated invalid",
		ExitError::CallTooDeep => "call too deep",
		ExitError::OutOfOffset => "out of offset",
		ExitError::OutOfGas => "out of gas",
		ExitError::CreateCollision => "create collision",
		ExitError::Other(err) => err,
		_ => "unexpected error",
	}
	.as_bytes()
	.to_vec()
}

impl ListenerT for ListenerCl {
	fn event(&mut self, event: Event) {
		match event {
			Event::Gasometer(gasometer_event) => self.gasometer_event(gasometer_event),
			Event::Runtime(runtime_event) => self.runtime_event(runtime_event),
			Event::Evm(evm_event) => self.evm_event(evm_event),
			Event::CallListNew() => {
				if !self.call_list_first_transaction {
					self.finish_transaction();
					self.skip_next_context = false;
					self.entries.push(BTreeMap::new());
				} else {
					self.call_list_first_transaction = false;
				}
			}
		};
	}

	fn step_event_filter(&self) -> StepEventFilter {
		StepEventFilter {
			enable_memory: false,
			enable_stack: false,
		}
	}
}

//RAW/////////////////////////
#[derive(Debug)]
pub struct ListenerRaw {
	disable_storage: bool,
	disable_memory: bool,
	disable_stack: bool,

	new_context: bool,
	context_stack: Vec<ContextRaw>,

	pub struct_logs: Vec<RawStepLog>,
	pub return_value: Vec<u8>,
	pub final_gas: u64,
	pub remaining_memory_usage: Option<usize>,
}

#[derive(Debug)]
struct ContextRaw {
	storage_cache: BTreeMap<H256, H256>,
	address: H160,
	current_step: Option<Step>,
	global_storage_changes: BTreeMap<H160, BTreeMap<H256, H256>>,
}

#[derive(Debug)]
struct Step {
	/// Current opcode.
	opcode: Vec<u8>,
	/// Depth of the context.
	depth: usize,
	/// Remaining gas.
	gas: u64,
	/// Gas cost of the following opcode.
	gas_cost: u64,
	/// Program counter position.
	position: usize,
	/// EVM memory copy (if not disabled).
	memory: Option<Vec<u8>>,
	/// EVM stack copy (if not disabled).
	stack: Option<Vec<H256>>,
}

impl ListenerRaw {
	pub fn new(
		disable_storage: bool,
		disable_memory: bool,
		disable_stack: bool,
		raw_max_memory_usage: usize,
	) -> Self {
		Self {
			disable_storage,
			disable_memory,
			disable_stack,
			remaining_memory_usage: Some(raw_max_memory_usage),

			struct_logs: vec![],
			return_value: vec![],
			final_gas: 0,

			new_context: false,
			context_stack: vec![],
		}
	}

	pub fn using<R, F: FnOnce() -> R>(&mut self, f: F) -> R {
		evm::events::using(self, f)
	}

	pub fn gasometer_event(&mut self, event: GasometerEvent) {
		match event {
			GasometerEvent::RecordTransaction { cost, .. } => {
				// This is the first event of a transaction.
				// The next step will be the first context.
				self.new_context = true;
				self.final_gas = cost;
			}
			GasometerEvent::RecordCost { cost, snapshot } => {
				if let Some(context) = self.context_stack.last_mut() {
					// Register opcode cost. (Ignore costs not between Step and StepResult)
					if let Some(step) = &mut context.current_step {
						step.gas = snapshot.gas();
						step.gas_cost = cost;
					}

					self.final_gas = snapshot.used_gas;
				}
			}
			GasometerEvent::RecordDynamicCost {
				gas_cost, snapshot, ..
			} => {
				if let Some(context) = self.context_stack.last_mut() {
					// Register opcode cost. (Ignore costs not between Step and StepResult)
					if let Some(step) = &mut context.current_step {
						step.gas = snapshot.gas();
						step.gas_cost = gas_cost;
					}

					self.final_gas = snapshot.used_gas;
				}
			}
			// Ignore other kinds of messages (new ones may be added in the future).
			#[allow(unreachable_patterns)]
			_ => (),
		}
	}

	pub fn runtime_event(&mut self, event: RuntimeEvent) {
		match event {
			RuntimeEvent::Step {
				context,
				opcode,
				position,
				stack,
				memory,
			} => {
				// Create a context if needed.
				if self.new_context {
					self.new_context = false;

					self.context_stack.push(ContextRaw {
						storage_cache: BTreeMap::new(),
						address: context.address,
						current_step: None,
						global_storage_changes: BTreeMap::new(),
					});
				}

				let depth = self.context_stack.len();

				// Ignore steps outside of any context (shouldn't even be possible).
				if let Some(context) = self.context_stack.last_mut() {
					context.current_step = Some(Step {
						opcode,
						depth,
						gas: 0,      // 0 for now, will add with gas events
						gas_cost: 0, // 0 for now, will add with gas events
						position: *position.as_ref().unwrap_or(&0) as usize,
						memory: if self.disable_memory {
							None
						} else {
							let memory = memory.expect("memory data to not be filtered out");

							self.remaining_memory_usage = self
								.remaining_memory_usage
								.and_then(|inner| inner.checked_sub(memory.data.len()));

							if self.remaining_memory_usage.is_none() {
								return;
							}

							Some(memory.data.clone())
						},
						stack: if self.disable_stack {
							None
						} else {
							let stack = stack.expect("stack data to not be filtered out");

							self.remaining_memory_usage = self
								.remaining_memory_usage
								.and_then(|inner| inner.checked_sub(stack.data.len()));

							if self.remaining_memory_usage.is_none() {
								return;
							}

							Some(stack.data.clone())
						},
					});
				}
			}
			RuntimeEvent::StepResult {
				result,
				return_value,
			} => {
				// StepResult is expected to be emited after a step (in a context).
				// Only case StepResult will occur without a Step before is in a transfer
				// transaction to a non-contract address. However it will not contain any
				// steps and return an empty trace, so we can ignore this edge case.
				if let Some(context) = self.context_stack.last_mut() {
					if let Some(current_step) = context.current_step.take() {
						let Step {
							opcode,
							depth,
							gas,
							gas_cost,
							position,
							memory,
							stack,
						} = current_step;

						let memory = memory.map(convert_memory);

						let storage = if self.disable_storage {
							None
						} else {
							self.remaining_memory_usage =
								self.remaining_memory_usage.and_then(|inner| {
									inner.checked_sub(context.storage_cache.len() * 64)
								});

							if self.remaining_memory_usage.is_none() {
								return;
							}

							Some(context.storage_cache.clone())
						};

						self.struct_logs.push(RawStepLog {
							depth: depth.into(),
							gas: gas.into(),
							gas_cost: gas_cost.into(),
							memory,
							op: opcode,
							pc: position.into(),
							stack,
							storage,
						});
					}
				}

				// We match on the capture to handle traps/exits.
				match result {
					Err(Capture::Exit(reason)) => {
						// Exit = we exit the context (should always be some)
						if let Some(mut context) = self.context_stack.pop() {
							// If final context is exited, we store gas and return value.
							if self.context_stack.is_empty() {
								self.return_value = return_value.to_vec();
							}

							// If the context exited without revert we must keep track of the
							// updated storage keys.
							if !self.disable_storage && matches!(reason, ExitReason::Succeed(_)) {
								if let Some(parent_context) = self.context_stack.last_mut() {
									// Add cache to storage changes.
									context
										.global_storage_changes
										.insert(context.address, context.storage_cache);

									// Apply storage changes to parent, either updating its cache or map of changes.
									for (address, mut storage) in
										context.global_storage_changes.into_iter()
									{
										// Same address => We update its cache (only tracked keys)
										if parent_context.address == address {
											for (cached_key, cached_value) in
												parent_context.storage_cache.iter_mut()
											{
												if let Some(value) = storage.remove(cached_key) {
													*cached_value = value;
												}
											}
										}
										// Otherwise, update the storage changes.
										else {
											parent_context
												.global_storage_changes
												.entry(address)
												.or_insert_with(BTreeMap::new)
												.append(&mut storage);
										}
									}
								}
							}
						}
					}
					Err(Capture::Trap(opcode)) if ContextType::from(opcode.clone()).is_some() => {
						self.new_context = true;
					}
					_ => (),
				}
			}
			RuntimeEvent::SLoad {
				address: _,
				index,
				value,
			}
			| RuntimeEvent::SStore {
				address: _,
				index,
				value,
			} => {
				if let Some(context) = self.context_stack.last_mut() {
					if !self.disable_storage {
						context.storage_cache.insert(index, value);
					}
				}
			}
			// We ignore other kinds of message if any (new ones may be added in the future).
			#[allow(unreachable_patterns)]
			_ => (),
		}
	}
}

impl ListenerT for ListenerRaw {
	fn event(&mut self, event: Event) {
		if self.remaining_memory_usage.is_none() {
			return;
		}

		match event {
			Event::Gasometer(e) => self.gasometer_event(e),
			Event::Runtime(e) => self.runtime_event(e),
			_ => {}
		};
	}

	fn step_event_filter(&self) -> StepEventFilter {
		StepEventFilter {
			enable_memory: !self.disable_memory,
			enable_stack: !self.disable_stack,
		}
	}
}

#[cfg(test)]
#[allow(unused)]
mod tests {
	use super::*;
	use ethereum_types::H256;
	use evm::{
		events::CreateScheme,
		runtime::{Context as EvmContext, Memory, Stack},
	};
	use evm_gasometer::events::Snapshot;

	enum TestEvmEvent {
		Call,
		Create,
		Suicide,
		Exit,
		TransactCall,
		TransactCreate,
		TransactCreate2,
	}

	enum TestRuntimeEvent {
		Step,
		StepResult,
		SLoad,
		SStore,
	}

	enum TestGasometerEvent {
		RecordCost,
		RecordRefund,
		RecordStipend,
		RecordDynamicCost,
		RecordTransaction,
	}

	fn test_context() -> EvmContext {
		EvmContext {
			address: H160::default(),
			caller: H160::default(),
			apparent_value: U256::zero(),
		}
	}

	fn test_create_scheme() -> CreateScheme {
		CreateScheme::Legacy {
			caller: H160::default(),
		}
	}

	fn test_stack() -> Option<Stack> {
		None
	}

	fn test_memory() -> Option<Memory> {
		None
	}

	fn test_snapshot() -> Snapshot {
		Snapshot {
			gas_limit: 0u64,
			memory_gas: 0u64,
			used_gas: 0u64,
			refunded_gas: 0i64,
		}
	}

	fn test_emit_evm_event(
		event_type: TestEvmEvent,
		is_static: bool,
		exit_reason: Option<ExitReason>,
	) -> EvmEvent {
		match event_type {
			TestEvmEvent::Call => EvmEvent::Call {
				code_address: H160::default(),
				transfer: None,
				input: Vec::new(),
				target_gas: None,
				is_static,
				context: test_context(),
			},
			TestEvmEvent::Create => EvmEvent::Create {
				caller: H160::default(),
				address: H160::default(),
				scheme: test_create_scheme(),
				value: U256::zero(),
				init_code: Vec::new(),
				target_gas: None,
			},
			TestEvmEvent::Suicide => EvmEvent::Suicide {
				address: H160::default(),
				target: H160::default(),
				balance: U256::zero(),
			},
			TestEvmEvent::Exit => EvmEvent::Exit {
				reason: exit_reason.unwrap(),
				return_value: Vec::new(),
			},
			TestEvmEvent::TransactCall => EvmEvent::TransactCall {
				caller: H160::default(),
				address: H160::default(),
				value: U256::zero(),
				data: Vec::new(),
				gas_limit: 0u64,
			},
			TestEvmEvent::TransactCreate => EvmEvent::TransactCreate {
				caller: H160::default(),
				value: U256::zero(),
				init_code: Vec::new(),
				gas_limit: 0u64,
				address: H160::default(),
			},
			TestEvmEvent::TransactCreate2 => EvmEvent::TransactCreate2 {
				caller: H160::default(),
				value: U256::zero(),
				init_code: Vec::new(),
				salt: H256::default(),
				gas_limit: 0u64,
				address: H160::default(),
			},
		}
	}

	fn test_emit_runtime_event(event_type: TestRuntimeEvent) -> RuntimeEvent {
		match event_type {
			TestRuntimeEvent::Step => RuntimeEvent::Step {
				context: test_context(),
				opcode: Vec::new(),
				position: Ok(0u64),
				stack: test_stack(),
				memory: test_memory(),
			},
			TestRuntimeEvent::StepResult => RuntimeEvent::StepResult {
				result: Ok(()),
				return_value: Vec::new(),
			},
			TestRuntimeEvent::SLoad => RuntimeEvent::SLoad {
				address: H160::default(),
				index: H256::default(),
				value: H256::default(),
			},
			TestRuntimeEvent::SStore => RuntimeEvent::SStore {
				address: H160::default(),
				index: H256::default(),
				value: H256::default(),
			},
		}
	}

	fn test_emit_gasometer_event(event_type: TestGasometerEvent) -> GasometerEvent {
		match event_type {
			TestGasometerEvent::RecordCost => GasometerEvent::RecordCost {
				cost: 0u64,
				snapshot: test_snapshot(),
			},
			TestGasometerEvent::RecordRefund => GasometerEvent::RecordRefund {
				refund: 0i64,
				snapshot: test_snapshot(),
			},
			TestGasometerEvent::RecordStipend => GasometerEvent::RecordStipend {
				stipend: 0u64,
				snapshot: test_snapshot(),
			},
			TestGasometerEvent::RecordDynamicCost => GasometerEvent::RecordDynamicCost {
				gas_cost: 0u64,
				memory_gas: 0u64,
				gas_refund: 0i64,
				snapshot: test_snapshot(),
			},
			TestGasometerEvent::RecordTransaction => GasometerEvent::RecordTransaction {
				cost: 0u64,
				snapshot: test_snapshot(),
			},
		}
	}

	fn do_transact_call_event(listener: &mut ListenerCl) {
		listener.evm_event(test_emit_evm_event(TestEvmEvent::TransactCall, false, None));
	}

	fn do_transact_create_event(listener: &mut ListenerCl) {
		listener.evm_event(test_emit_evm_event(
			TestEvmEvent::TransactCreate,
			false,
			None,
		));
	}

	fn do_gasometer_event(listener: &mut ListenerCl) {
		listener.gasometer_event(test_emit_gasometer_event(
			TestGasometerEvent::RecordTransaction,
		));
	}

	fn do_exit_event(listener: &mut ListenerCl) {
		listener.evm_event(test_emit_evm_event(
			TestEvmEvent::Exit,
			false,
			Some(ExitReason::Error(ExitError::OutOfGas)),
		));
	}

	fn do_evm_call_event(listener: &mut ListenerCl) {
		listener.evm_event(test_emit_evm_event(TestEvmEvent::Call, false, None));
	}

	fn do_evm_create_event(listener: &mut ListenerCl) {
		listener.evm_event(test_emit_evm_event(TestEvmEvent::Create, false, None));
	}

	fn do_evm_suicide_event(listener: &mut ListenerCl) {
		listener.evm_event(test_emit_evm_event(TestEvmEvent::Suicide, false, None));
	}

	fn do_runtime_step_event(listener: &mut ListenerCl) {
		listener.runtime_event(test_emit_runtime_event(TestRuntimeEvent::Step));
	}

	fn do_runtime_step_result_event(listener: &mut ListenerCl) {
		listener.runtime_event(test_emit_runtime_event(TestRuntimeEvent::StepResult));
	}

	// Call context

	// Early exit on TransactionCost.
	#[test]
	fn call_early_exit_tx_cost() {
		let mut listener = ListenerCl::default();
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_exit_event(&mut listener);
		listener.finish_transaction();
		assert_eq!(listener.entries.len(), 1);
		assert_eq!(listener.entries[0].len(), 1);
	}

	// Early exit somewhere between the first callstack event and stepping the bytecode.
	// I.e. precompile call.
	#[test]
	fn call_early_exit_before_runtime() {
		let mut listener = ListenerCl::default();
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_exit_event(&mut listener);
		listener.finish_transaction();
		assert_eq!(listener.entries.len(), 1);
		assert_eq!(listener.entries[0].len(), 1);
	}

	// Exit after Step without StepResult.
	#[test]
	fn call_step_without_step_result() {
		let mut listener = ListenerCl::default();
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_exit_event(&mut listener);
		listener.finish_transaction();
		assert_eq!(listener.entries.len(), 1);
		assert_eq!(listener.entries[0].len(), 1);
	}

	// Exit after StepResult.
	#[test]
	fn call_step_result() {
		let mut listener = ListenerCl::default();
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);
		do_exit_event(&mut listener);
		listener.finish_transaction();
		assert_eq!(listener.entries.len(), 1);
		assert_eq!(listener.entries[0].len(), 1);
	}

	// Suicide.
	#[test]
	fn call_suicide() {
		let mut listener = ListenerCl::default();
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_evm_suicide_event(&mut listener);
		do_exit_event(&mut listener);
		listener.finish_transaction();
		assert_eq!(listener.entries.len(), 1);
		assert_eq!(listener.entries[0].len(), 2);
	}

	// Create context

	// Early exit on TransactionCost.
	#[test]
	fn create_early_exit_tx_cost() {
		let mut listener = ListenerCl::default();
		do_transact_create_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_exit_event(&mut listener);
		listener.finish_transaction();
		assert_eq!(listener.entries.len(), 1);
		assert_eq!(listener.entries[0].len(), 1);
	}

	// Early exit somewhere between the first callstack event and stepping the bytecode
	// I.e. precompile call..
	#[test]
	fn create_early_exit_before_runtime() {
		let mut listener = ListenerCl::default();
		do_transact_create_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_create_event(&mut listener);
		do_exit_event(&mut listener);
		listener.finish_transaction();
		assert_eq!(listener.entries.len(), 1);
		assert_eq!(listener.entries[0].len(), 1);
	}

	// Exit after Step without StepResult.
	#[test]
	fn create_step_without_step_result() {
		let mut listener = ListenerCl::default();
		do_transact_create_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_create_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_exit_event(&mut listener);
		listener.finish_transaction();
		assert_eq!(listener.entries.len(), 1);
		assert_eq!(listener.entries[0].len(), 1);
	}

	// Exit after StepResult.
	#[test]
	fn create_step_result() {
		let mut listener = ListenerCl::default();
		do_transact_create_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_create_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);
		do_exit_event(&mut listener);
		listener.finish_transaction();
		assert_eq!(listener.entries.len(), 1);
		assert_eq!(listener.entries[0].len(), 1);
	}

	// Call Context Nested

	// Nested call early exit before stepping.
	#[test]
	fn nested_call_early_exit_before_runtime() {
		let mut listener = ListenerCl::default();
		// Main
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);
		// Nested
		do_evm_call_event(&mut listener);
		do_exit_event(&mut listener);
		// Main exit
		do_exit_event(&mut listener);
		listener.finish_transaction();
		assert_eq!(listener.entries.len(), 1);
		assert_eq!(listener.entries[0].len(), 2);
	}

	// Nested exit before step result.
	#[test]
	fn nested_call_without_step_result() {
		let mut listener = ListenerCl::default();
		// Main
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);
		// Nested
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_exit_event(&mut listener);
		// Main exit
		do_exit_event(&mut listener);
		listener.finish_transaction();
		assert_eq!(listener.entries.len(), 1);
		assert_eq!(listener.entries[0].len(), 2);
	}

	// Nested exit.
	#[test]
	fn nested_call_step_result() {
		let depth = 5;
		let mut listener = ListenerCl::default();
		// Main
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);
		// 5 nested calls
		for d in 0..depth {
			do_evm_call_event(&mut listener);
			do_runtime_step_event(&mut listener);
			do_runtime_step_result_event(&mut listener);
			do_exit_event(&mut listener);
		}
		// Main exit
		do_exit_event(&mut listener);
		listener.finish_transaction();
		assert_eq!(listener.entries.len(), 1);
		assert_eq!(listener.entries[0].len(), depth + 1);
	}

	// Call + Create mixed subnesting.

	#[test]
	fn subnested_call_and_create_mixbag() {
		let depth = 5;
		let subdepth = 10;
		let mut listener = ListenerCl::default();
		// Main
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);
		// 5 nested call/creates, each with 10 nested call/creates
		for d in 0..depth {
			if d % 2 == 0 {
				do_evm_call_event(&mut listener);
			} else {
				do_evm_create_event(&mut listener);
			}
			do_runtime_step_event(&mut listener);
			do_runtime_step_result_event(&mut listener);
			for s in 0..subdepth {
				// Some mixed call/create and early exits.
				if s % 2 == 0 {
					do_evm_call_event(&mut listener);
				} else {
					do_evm_create_event(&mut listener);
				}
				if s % 3 == 0 {
					do_runtime_step_event(&mut listener);
					do_runtime_step_result_event(&mut listener);
				}
				do_exit_event(&mut listener);
			}
			// Nested exit
			do_exit_event(&mut listener);
		}
		// Main exit
		do_exit_event(&mut listener);
		listener.finish_transaction();
		assert_eq!(listener.entries.len(), 1);
		// Each nested call contains 11 elements in the callstack (main + 10 subcalls).
		// There are 5 main nested calls for a total of 56 elements in the callstack: 1 main + 55 nested.
		assert_eq!(listener.entries[0].len(), (depth * (subdepth + 1)) + 1);
	}
}
