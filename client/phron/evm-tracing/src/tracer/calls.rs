extern crate alloc;
use serde::Serialize;

use super::{serializers::*, types::Call, CallResult, CallType, CreateResult};
use ethereum_types::{H160, U256};
use parity_scale_codec::{Decode, Encode};

#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, Serialize)]
#[serde(rename_all = "lowercase", tag = "type")]
pub enum ExplorerCallInner {
	Call {
		#[serde(rename(serialize = "callType"))]
		/// Type of call.
		call_type: CallType,
		to: H160,
		#[serde(serialize_with = "serialize_bytes_0x")]
		input: Vec<u8>,
		/// "output" or "error" field
		#[serde(flatten)]
		res: CallResult,
	},
	Create {
		#[serde(serialize_with = "serialize_bytes_0x")]
		init: Vec<u8>,
		#[serde(flatten)]
		res: CreateResult,
	},
	SelfDestruct {
		#[serde(skip)]
		balance: U256,
		to: H160,
	},
}

#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplorerCall {
	pub from: H160,
	/// Indices of parent calls.
	pub trace_address: Vec<u32>,
	/// Number of children calls.
	/// Not needed for Explorer, but needed for `crate::block`
	/// types that are build from this type.
	#[serde(skip)]
	pub subtraces: u32,
	/// Sends funds to the (payable) function
	pub value: U256,
	/// Remaining gas in the runtime.
	pub gas: U256,
	/// Gas used by this context.
	pub gas_used: U256,
	#[serde(flatten)]
	pub inner: ExplorerCallInner,
}

#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, Serialize)]
#[serde(untagged)]
pub enum CallTracerInner {
	Call {
		#[serde(rename = "type", serialize_with = "serialize_opcode")]
		call_type: Vec<u8>,
		to: H160,
		#[serde(serialize_with = "serialize_bytes_0x")]
		input: Vec<u8>,
		/// "output" or "error" field
		#[serde(flatten)]
		res: CallResult,

		#[serde(skip_serializing_if = "Option::is_none")]
		value: Option<U256>,
	},
	Create {
		#[serde(rename = "type", serialize_with = "serialize_opcode")]
		call_type: Vec<u8>,
		#[serde(serialize_with = "serialize_bytes_0x")]
		input: Vec<u8>,
		#[serde(skip_serializing_if = "Option::is_none")]
		to: Option<H160>,
		#[serde(
			skip_serializing_if = "Option::is_none",
			serialize_with = "serialize_option_bytes_0x"
		)]
		output: Option<Vec<u8>>,
		#[serde(
			skip_serializing_if = "Option::is_none",
			serialize_with = "serialize_option_string"
		)]
		error: Option<Vec<u8>>,
		value: U256,
	},
	SelfDestruct {
		#[serde(rename = "type", serialize_with = "serialize_opcode")]
		call_type: Vec<u8>,
		to: H160,
		value: U256,
	},
}

#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallTracerCall {
	pub from: H160,

	/// Indices of parent calls. Used to build the Etherscan nested response.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub trace_address: Option<Vec<u32>>,

	/// Remaining gas in the runtime.
	pub gas: U256,
	/// Gas used by this context.
	pub gas_used: U256,

	#[serde(flatten)]
	pub inner: CallTracerInner,

	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub calls: Vec<Call>,
}
