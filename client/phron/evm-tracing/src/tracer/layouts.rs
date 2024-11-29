use sp_std::{cmp::Ordering, vec::Vec};

use super::{
	calls::{CallTracerCall, CallTracerInner, ExplorerCallInner},
	listeners::{ListenerCl, ListenerRaw},
	types::{Call, SingleTransactionTrace},
	CallType, CreateResult,
};

pub struct RawLayout;
impl super::ResponseLayout for RawLayout {
	type Listener = ListenerRaw;
	type Response = SingleTransactionTrace;

	/// Formats the listener data into a `SingleTransactionTrace`.
	fn format(listener: ListenerRaw) -> Option<SingleTransactionTrace> {
		// Check if remaining memory usage is None
		match listener.remaining_memory_usage {
			// Construct a SingleTransactionTrace::Raw with the provided data
			Some(_) => Some(SingleTransactionTrace::Raw {
				struct_logs: listener.struct_logs,
				gas: listener.final_gas.into(),
				return_value: listener.return_value,
			}),
			None => None,
		}
	}
}

pub struct ExplorerLayout;
impl super::ResponseLayout for ExplorerLayout {
	type Listener = ListenerCl;
	type Response = SingleTransactionTrace;

	fn format(listener: ListenerCl) -> Option<SingleTransactionTrace> {
		// Use the last entry in the listener, if available
		if let Some(entry) = listener.entries.last() {
			// Mapping the entry values to create a list of Call::Explorer variants
			let call_list = entry
				.iter()
				.map(|(_, value)| Call::Explorer(value.clone()))
				.collect();

			// Returning a Some variant with SingleTransactionTrace::CallList
			Some(SingleTransactionTrace::CallList(call_list))
		} else {
			None
		}
	}
}

pub struct CallTracerLayout;
impl super::ResponseLayout for CallTracerLayout {
	type Listener = ListenerCl;
	type Response = Vec<SingleTransactionTrace>;

	fn format(listener: ListenerCl) -> Option<Vec<SingleTransactionTrace>> {
		let filtered_entries: Vec<Vec<Call>> = listener
			.entries
			.iter()
			.filter(|entry| !entry.is_empty())
			.map(|entry| {
				entry
					.iter()
					.map(|(_, it)| {
						let from = it.from;
						let trace_address = it.trace_address.clone();
						let value = it.value;
						let gas = it.gas;
						let gas_used = it.gas_used;
						let inner = it.inner.clone();

						let call = match inner {
							ExplorerCallInner::Call {
								input,
								to,
								res,
								call_type,
							} => Call::CallTracer(CallTracerCall {
								from,
								gas,
								gas_used,
								trace_address: Some(trace_address.clone()),
								inner: CallTracerInner::Call {
									call_type: match call_type {
										CallType::Call => "CALL".as_bytes().to_vec(),
										CallType::CallCode => "CALLCODE".as_bytes().to_vec(),
										CallType::DelegateCall => {
											"DELEGATECALL".as_bytes().to_vec()
										}
										CallType::StaticCall => "STATICCALL".as_bytes().to_vec(),
									},
									to,
									input,
									res,
									value: Some(value),
								},
								calls: Vec::new(),
							}),
							ExplorerCallInner::Create { init, res } => {
								Call::CallTracer(CallTracerCall {
									from,
									gas,
									gas_used,
									trace_address: Some(trace_address.clone()),
									inner: CallTracerInner::Create {
										input: init,
										error: match res {
											CreateResult::Success { .. } => None,
											CreateResult::Error { ref error } => {
												Some(error.clone())
											}
										},
										to: match res {
											CreateResult::Success {
												created_contract_address_hash,
												..
											} => Some(created_contract_address_hash),
											CreateResult::Error { .. } => None,
										},
										output: match res {
											CreateResult::Success {
												created_contract_code,
												..
											} => Some(created_contract_code),
											CreateResult::Error { .. } => None,
										},
										value,
										call_type: "CREATE".as_bytes().to_vec(),
									},
									calls: Vec::new(),
								})
							}
							ExplorerCallInner::SelfDestruct { balance, to } => {
								Call::CallTracer(CallTracerCall {
									from,
									gas,
									gas_used,
									trace_address: Some(trace_address.clone()),
									inner: CallTracerInner::SelfDestruct {
										value: balance,
										to,
										call_type: "SELFDESTRUCT".as_bytes().to_vec(),
									},
									calls: Vec::new(),
								})
							}
						};

						call
					})
					.collect()
			})
			.collect();

		let traces: Vec<SingleTransactionTrace> = filtered_entries
			.into_iter()
			.filter_map(|mut call_list| {
				if call_list.len() > 1 {
					sort_calls(&mut call_list);
					process_calls(&mut call_list);
				}

				if let Some(Call::CallTracer(CallTracerCall {
					ref mut trace_address,
					..
				})) = call_list.first_mut()
				{
					*trace_address = None;
				}

				if call_list.len() == 1 {
					Some(SingleTransactionTrace::CallListNested(
						call_list.pop().expect("call_list.len() == 1"),
					))
				} else {
					None
				}
			})
			.collect();

		if traces.is_empty() {
			None
		} else {
			Some(traces)
		}
	}
}

fn sort_calls(result: &mut Vec<Call>) {
	result.sort_by(|a, b| match (a, b) {
		(
			Call::CallTracer(CallTracerCall {
				trace_address: Some(a_trace),
				..
			}),
			Call::CallTracer(CallTracerCall {
				trace_address: Some(b_trace),
				..
			}),
		) => {
			let a_trace_len = a_trace.len();
			let b_trace_len = b_trace.len();

			if b_trace_len > a_trace_len
				|| (a_trace_len == b_trace_len
					&& a_trace
						.iter()
						.zip(b_trace.iter())
						.any(|(a_val, b_val)| a_val > b_val))
			{
				Ordering::Less
			} else {
				Ordering::Greater
			}
		}
		_ => unreachable!(),
	});
}

fn find_parent_index(last_call: &Call, call_list: &Vec<Call>) -> Option<usize> {
	call_list
		.iter()
		.position(|current_call| match (last_call, current_call) {
			(
				Call::CallTracer(CallTracerCall {
					trace_address: Some(last_trace_address),
					..
				}),
				Call::CallTracer(CallTracerCall {
					trace_address: Some(current_trace_address),
					..
				}),
			) => &current_trace_address[..] == &last_trace_address[0..last_trace_address.len() - 1],
			_ => unreachable!(),
		})
}

fn process_calls(call_stack: &mut Vec<Call>) {
	while call_stack.len() > 1 {
		let mut current_call = call_stack
			.pop()
			.expect("Expected call_stack to have more than 1 element before popping");

		if let Some(parent_index) = find_parent_index(&current_call, call_stack) {
			if let Call::CallTracer(CallTracerCall { trace_address, .. }) = &mut current_call {
				*trace_address = None;
			}

			if let Some(Call::CallTracer(CallTracerCall { calls, .. })) =
				call_stack.get_mut(parent_index)
			{
				calls.push(current_call);
			}
		}
	}
}
