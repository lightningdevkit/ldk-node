use bdk::blockchain::esplora;
use lightning::ln::msgs;
use std::fmt;
use std::io;
use std::sync::mpsc;
use std::time;

#[derive(Debug)]
/// An error that possibly needs to be handled by the user.
pub enum LdkLiteError {
	/// Returned when trying to start LdkLite while it is already running.
	AlreadyRunning,
	/// Returned when trying to stop LdkLite while it is not running.
	NotRunning,
	/// An input of the funding transaction tried spending a non-SegWit output. This should never happen, but
	/// better safe than sorry..
	FundingTxNonWitnessOuputSpend,
	/// The funding transaction could not be finalized.
	FundingTxNotFinalized,
	/// TODO
	ChainStateMismatch,
	/// A network connection has been closed.
	ConnectionClosed,
	/// A wrapped LDK `DecodeError`
	Decode(msgs::DecodeError),
	/// A wrapped BDK error
	Bdk(bdk::Error),
	/// A wrapped `Bip32` error
	Bip32(bitcoin::util::bip32::Error),
	/// A wrapped `std::io::Error`
	Io(io::Error),
	/// A wrapped `SystemTimeError`
	Time(time::SystemTimeError),
	/// A wrapped `EsploraError`
	Esplora(esplora::EsploraError),
	/// A wrapped `mpsc::RecvError`
	ChannelRecv(mpsc::RecvError),
}

impl fmt::Display for LdkLiteError {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match *self {
			LdkLiteError::Decode(ref e) => write!(f, "LDK decode error: {}", e),
			LdkLiteError::Bdk(ref e) => write!(f, "BDK error: {}", e),
			LdkLiteError::Bip32(ref e) => write!(f, "Bitcoin error: {}", e),
			LdkLiteError::Io(ref e) => write!(f, "IO error: {}", e),
			LdkLiteError::Time(ref e) => write!(f, "time error: {}", e),
			LdkLiteError::Esplora(ref e) => write!(f, "Esplora error: {}", e),
			LdkLiteError::ChannelRecv(ref e) => write!(f, "channel recv error: {}", e),
			LdkLiteError::AlreadyRunning => write!(f, "LDKLite is already running."),
			LdkLiteError::NotRunning => write!(f, "LDKLite is not running."),
			LdkLiteError::FundingTxNonWitnessOuputSpend => write!(f, "an input of the funding transaction tried spending a non-SegWit output, which is insecure"),
			LdkLiteError::FundingTxNotFinalized => write!(f, "the funding transaction could not be finalized"),
			LdkLiteError::ChainStateMismatch => write!(f, "ChainStateMismatch"),
			LdkLiteError::ConnectionClosed => write!(f, "network connection closed"),
		}
	}
}

impl From<msgs::DecodeError> for LdkLiteError {
	fn from(e: msgs::DecodeError) -> Self {
		Self::Decode(e)
	}
}

impl From<bdk::Error> for LdkLiteError {
	fn from(e: bdk::Error) -> Self {
		Self::Bdk(e)
	}
}

impl From<bitcoin::util::bip32::Error> for LdkLiteError {
	fn from(e: bitcoin::util::bip32::Error) -> Self {
		Self::Bip32(e)
	}
}

impl From<bdk::electrum_client::Error> for LdkLiteError {
	fn from(e: bdk::electrum_client::Error) -> Self {
		Self::Bdk(bdk::Error::Electrum(e))
	}
}

impl From<io::Error> for LdkLiteError {
	fn from(e: io::Error) -> Self {
		Self::Io(e)
	}
}

impl From<time::SystemTimeError> for LdkLiteError {
	fn from(e: time::SystemTimeError) -> Self {
		Self::Time(e)
	}
}

impl From<mpsc::RecvError> for LdkLiteError {
	fn from(e: mpsc::RecvError) -> Self {
		Self::ChannelRecv(e)
	}
}

impl From<esplora::EsploraError> for LdkLiteError {
	fn from(e: esplora::EsploraError) -> Self {
		Self::Esplora(e)
	}
}
