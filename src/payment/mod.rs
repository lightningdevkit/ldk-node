//! Objects for different types of payments.

mod bolt11;
mod bolt12;
mod onchain;
mod spontaneous;
pub(crate) mod store;
mod unified_qr;

pub use bolt11::Bolt11Payment;
pub use bolt12::Bolt12Payment;
pub use onchain::OnchainPayment;
pub use spontaneous::SpontaneousPayment;
pub use store::{LSPFeeLimits, PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus};
pub use unified_qr::{QrPaymentResult, UnifiedQrPayment};

/// Represents information used to route a payment.
#[derive(Clone, Debug, PartialEq)]
pub struct SendingParameters {
	/// The maximum total fees, in millisatoshi, that may accrue during route finding.
	///
	/// This limit also applies to the total fees that may arise while retrying failed payment
	/// paths.
	///
	/// Note that values below a few sats may result in some paths being spuriously ignored.
	pub max_total_routing_fee_msat: Option<u64>,

	/// The maximum total CLTV delta we accept for the route.
	///
	/// Defaults to [`DEFAULT_MAX_TOTAL_CLTV_EXPIRY_DELTA`].
	///
	/// [`DEFAULT_MAX_TOTAL_CLTV_EXPIRY_DELTA`]: lightning::routing::router::DEFAULT_MAX_TOTAL_CLTV_EXPIRY_DELTA
	pub max_total_cltv_expiry_delta: Option<u32>,

	/// The maximum number of paths that may be used by (MPP) payments.
	///
	/// Defaults to [`DEFAULT_MAX_PATH_COUNT`].
	///
	/// [`DEFAULT_MAX_PATH_COUNT`]: lightning::routing::router::DEFAULT_MAX_PATH_COUNT
	pub max_path_count: Option<u8>,

	/// Selects the maximum share of a channel's total capacity which will be sent over a channel,
	/// as a power of 1/2.
	///
	/// A higher value prefers to send the payment using more MPP parts whereas
	/// a lower value prefers to send larger MPP parts, potentially saturating channels and
	/// increasing failure probability for those paths.
	///
	/// Note that this restriction will be relaxed during pathfinding after paths which meet this
	/// restriction have been found. While paths which meet this criteria will be searched for, it
	/// is ultimately up to the scorer to select them over other paths.
	///
	/// Examples:
	///
	/// | Value | Max Proportion of Channel Capacity Used |
	/// |-------|-----------------------------------------|
	/// | 0     | Up to 100% of the channel’s capacity    |
	/// | 1     | Up to 50% of the channel’s capacity     |
	/// | 2     | Up to 25% of the channel’s capacity     |
	/// | 3     | Up to 12.5% of the channel’s capacity   |
	///
	/// Default value: 2
	pub max_channel_saturation_power_of_half: Option<u8>,
}
