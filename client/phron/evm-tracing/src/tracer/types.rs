use super::serializers::*;
use serde::Serialize;

use ethereum_types::{H256, U256};
use parity_scale_codec::{Decode, Encode};
use sp_std::{collections::btree_map::BTreeMap, vec::Vec};

#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, Serialize)]
#[serde(rename_all = "camelCase", untagged)]
pub enum Call {
	Explorer(super::calls::ExplorerCall),
	CallTracer(super::calls::CallTracerCall),
}

#[derive(Clone, Copy, Eq, PartialEq, Debug, Encode, Decode)]
pub enum TraceType {
	/// Classic geth with no javascript based tracing.
	Raw {
		disable_storage: bool,
		disable_memory: bool,
		disable_stack: bool,
	},
	/// List of calls and subcalls formatted with an input tracer (i.e. callTracer or Explorer).
	CallList,
	/// A single block trace. Use in `debug_traceTransactionByNumber` / `traceTransactionByHash`.
	Block,
}

/// Single transaction trace.
#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, Serialize)]
#[serde(rename_all = "camelCase", untagged)]
pub enum SingleTransactionTrace {
	/// Classical output of `debug_trace`.
	#[serde(rename_all = "camelCase")]
	Raw {
		gas: U256,
		#[serde(with = "hex")]
		return_value: Vec<u8>,
		struct_logs: Vec<RawStepLog>,
	},
	/// Matches the formatter used by Explorer.
	/// Is also used to built output of OpenEthereum's `trace_filter`.
	CallList(Vec<Call>),
	/// Used by Geth's callTracer.
	CallListNested(Call),
}

#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RawStepLog {
	#[serde(serialize_with = "serialize_u256")]
	pub depth: U256,

	//error: TODO
	#[serde(serialize_with = "serialize_u256")]
	pub gas: U256,

	#[serde(serialize_with = "serialize_u256")]
	pub gas_cost: U256,

	#[serde(
		serialize_with = "serialize_seq_h256",
		skip_serializing_if = "Option::is_none"
	)]
	pub memory: Option<Vec<H256>>,

	#[serde(serialize_with = "serialize_opcode")]
	pub op: Vec<u8>,

	#[serde(serialize_with = "serialize_u256")]
	pub pc: U256,

	#[serde(
		serialize_with = "serialize_seq_h256",
		skip_serializing_if = "Option::is_none"
	)]
	pub stack: Option<Vec<H256>>,

	#[serde(skip_serializing_if = "Option::is_none")]
	pub storage: Option<BTreeMap<H256, H256>>,
}
