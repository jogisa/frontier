//! Environmental-aware externalities for EVM tracing in Wasm runtime. This enables
//! capturing the - potentially large - trace output data in the host and keep
//! a low memory footprint in `--execution=wasm`.
//!
//! - The original trace Runtime Api call is wrapped `using` environmental (thread local).
//! - Arguments are scale-encoded known types in the host.
//! - Host functions will decode the input and emit an event `with` environmental.

#![cfg_attr(not(feature = "std"), no_std)]

use sp_runtime_interface::runtime_interface;

use parity_scale_codec::Decode;
use sp_std::vec::Vec;

use evm::{
	events::{Event, EvmEvent, StepEventFilter},
	runtime::RuntimeEvent,
};
use evm_gasometer::events::GasometerEvent;

#[runtime_interface]
/// Extension trait for Phron functionality.
pub trait PhronExt {
	/// Perform a raw step operation with the given data.
	fn raw_step(&mut self, _data: Vec<u8>) {}

	/// Perform a raw gas operation with the given data.
	fn raw_gas(&mut self, _data: Vec<u8>) {}

	/// Handle the return value from a raw operation.
	fn raw_return_value(&mut self, _data: Vec<u8>) {}

	/// Update the entry in the call list at the specified index with the given value.
	fn call_list_entry(&mut self, _index: u32, _value: Vec<u8>) {}

	/// Create a new call list.
	fn call_list_new(&mut self) {}

	// New design, proxy events.

	/// Process an Evm event proxied by the PHRON runtime to this host function.
	fn evm_event(&mut self, event: Vec<u8>) {
		if let Ok(event) = EvmEvent::decode(&mut &event[..]) {
			Event::Evm(event).emit();
		}
	}

	/// Process a Gasometer event proxied by the PHRON runtime to this host function.
	fn gasometer_event(&mut self, event: Vec<u8>) {
		if let Ok(event) = GasometerEvent::decode(&mut &event[..]) {
			Event::Gasometer(event).emit();
		}
	}

	/// Process a Runtime event proxied by the PHRON runtime to this host function.
	fn runtime_event(&mut self, event: Vec<u8>) {
		if let Ok(event) = RuntimeEvent::decode(&mut &event[..]) {
			Event::Runtime(event).emit();
		}
	}

	/// Provide a filter for Step events to be used by the tracing module in the runtime.
	fn step_event_filter(&self) -> StepEventFilter {
		evm::events::step_event_filter().unwrap_or_default()
	}

	/// Emit an event to create a new CallList (currently a new transaction when tracing a block).
	#[version(2)]
	fn call_list_new(&mut self) {
		Event::CallListNew().emit();
	}
}
