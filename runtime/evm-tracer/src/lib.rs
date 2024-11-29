//! Substrate EVM tracing.
//!
//! The purpose of this crate is enable tracing the EVM opcode execution and will be used by
//! both Dapp developers - to get a granular view on their transactions - and indexers to access
//! the EVM callstack (internal transactions).
//!
//! Proxies EVM messages to the host functions.

#![cfg_attr(not(feature = "std"), no_std)]

pub mod tracer {
	use parity_scale_codec::Encode;

	use evm::events::StepEventFilter;
	pub use evm::{events::EvmEvent, runtime::RuntimeEvent};
	pub use evm_gasometer::events::GasometerEvent;

	use evm::tracing::{using as evm_using, EventListener as EvmListener};
	use evm_gasometer::tracing::{using as gasometer_using, EventListener as GasometerListener};
	use evm_runtime::tracing::{using as runtime_using, EventListener as RuntimeListener};
	use sp_std::{cell::RefCell, rc::Rc};

	struct ListenerProxy<T>(pub Rc<RefCell<T>>);
	impl<T: GasometerListener> GasometerListener for ListenerProxy<T> {
		fn event(&mut self, event: evm_gasometer::tracing::Event) {
			self.0.borrow_mut().event(event);
		}
	}

	impl<T: RuntimeListener> RuntimeListener for ListenerProxy<T> {
		fn event(&mut self, event: evm_runtime::tracing::Event) {
			self.0.borrow_mut().event(event);
		}
	}

	impl<T: EvmListener> EvmListener for ListenerProxy<T> {
		fn event(&mut self, event: evm::tracing::Event) {
			self.0.borrow_mut().event(event);
		}
	}

	pub struct EvmTracer {
		step_event_filter: StepEventFilter,
	}
	impl EvmTracer {
		pub fn new() -> Self {
			Self {
				step_event_filter: fp_ext::phron_ext::step_event_filter(),
			}
		}

		/// Setup event listeners and execute the provided closure.
		///
		/// Consume the tracer and return it alongside the return value of
		/// the closure.
		pub fn trace<R, F: FnOnce() -> R>(self, closure: F) {
			let tracer_ref = Rc::new(RefCell::new(self));

			let mut gas_listener = ListenerProxy(Rc::clone(&tracer_ref));
			let mut runtime_listener = ListenerProxy(Rc::clone(&tracer_ref));
			let mut evm_listener = ListenerProxy(Rc::clone(&tracer_ref));

			// Each line wraps the previous `closure` into a `using` call.
			// Listening to new events results in adding one new line.
			// The order is irrelevant when registering listeners.
			let closure = || runtime_using(&mut runtime_listener, closure);
			let closure = || gasometer_using(&mut gas_listener, closure);
			let closure = || evm_using(&mut evm_listener, closure);
			closure();
		}

		pub fn emit_new() {
			fp_ext::phron_ext::call_list_new();
		}
	}

	impl EvmListener for EvmTracer {
		/// Proxies `evm::tracing::Event` to the host.
		fn event(&mut self, event: evm::tracing::Event) {
			let event: EvmEvent = event.into();
			let message = event.encode();
			fp_ext::phron_ext::evm_event(message);
		}
	}

	impl GasometerListener for EvmTracer {
		/// Proxies `evm_gasometer::tracing::Event` to the host.
		fn event(&mut self, event: evm_gasometer::tracing::Event) {
			let event: GasometerEvent = event.into();
			let message = event.encode();
			fp_ext::phron_ext::gasometer_event(message);
		}
	}

	impl RuntimeListener for EvmTracer {
		/// Proxies `evm_runtime::tracing::Event` to the host.
		fn event(&mut self, event: evm_runtime::tracing::Event) {
			let event = RuntimeEvent::from_evm_event(event, self.step_event_filter);
			let message = event.encode();
			fp_ext::phron_ext::runtime_event(message);
		}
	}
}
