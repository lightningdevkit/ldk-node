// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! Holds a payment handler allowing to create [BIP 21] URIs with an on-chain and [BOLT 11] payment
//! option
//!
//! [BIP 21]: https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki
//! [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md

use crate::error::Error;
use crate::logger::{log_error, FilesystemLogger, Logger};
use crate::payment::{Bolt11Payment, Bolt12Payment, OnchainPayment};

use lightning::ln::channelmanager::PaymentId;
use lightning_invoice::Bolt11Invoice;

use bitcoin::address::{NetworkChecked, NetworkUnchecked};
use bitcoin::{Amount, Network, Txid};

use bip21::de::ParamKind;
use bip21::{DeserializationError, DeserializeParams, Param, SerializeParams};
use lightning::offers::offer::Offer;
use std::sync::Arc;
use std::vec::IntoIter;

type LnUri<'a> = bip21::Uri<'a, NetworkChecked, Extras>;

#[derive(Debug, Clone)]
struct Extras {
	bolt11_invoice: Option<Bolt11Invoice>,
	bolt12_offer: Option<Offer>,
}

/// A payment handler allowing to create [BIP 21] URIs with an on-chain and [BOLT 11] payment
/// option
///
/// Should be retrieved by calling [`Node::unified_qr_payment`]
///
/// [BIP 21]: https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki
/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
/// [`Node::unified_qr_payment`]: crate::Node::unified_qr_payment
pub struct UnifiedQrPayment {
	onchain_payment: Arc<OnchainPayment>,
	bolt11_invoice: Arc<Bolt11Payment>,
	bolt12_payment: Arc<Bolt12Payment>,
	logger: Arc<FilesystemLogger>,
}

impl UnifiedQrPayment {
	pub(crate) fn new(
		onchain_payment: Arc<OnchainPayment>, bolt11_invoice: Arc<Bolt11Payment>,
		bolt12_payment: Arc<Bolt12Payment>, logger: Arc<FilesystemLogger>,
	) -> Self {
		Self { onchain_payment, bolt11_invoice, bolt12_payment, logger }
	}

	/// Generates a URI with an on-chain address and [BOLT 11] invoice.
	///
	/// The URI allows users to send the payment request allowing the wallet to decide
	/// which payment method to use. This enables a fallback mechanism: older wallets
	/// can always pay using the provided on-chain address, while newer wallets will
	/// typically opt to use the provided invoice.
	///
	/// Returns a URI `String` if successful or an `Error` if there was an issue generating
	/// the on-chain address or lightning invoice.
	///
	/// The error types that may be returned are:
	/// - `Error::WalletOperationFailed` if there is an issue generating the on-chain address.
	/// - `Error::InvoiceCreationFailed` if there is an issue generating the BOLT 11 invoice.
	///
	/// The generated URI can then be given to a QR code library.
	///
	/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
	pub fn receive(
		&self, amount_sats: u64, message: Option<String>, expiry_sec: u32,
	) -> Result<String, Error> {
		let onchain_address = self.onchain_payment.new_address()?;

		let amount_msats = amount_sats * 1_000;

		let bolt11_invoice = match self.bolt11_invoice.receive(
			amount_msats,
			message.clone().unwrap().as_str(),
			expiry_sec,
		) {
			Ok(invoice) => Some(invoice),
			Err(e) => {
				log_error!(self.logger, "Failed to create invoice {:?}", e);
				None
			},
		};

		let extras = Extras { bolt11_invoice, bolt12_offer: None };

		let mut uri = LnUri::with_extras(onchain_address, extras);
		uri.amount = Some(Amount::from_sat(amount_sats));
		uri.message = message.map(|m| m.into());

		let mut uri_string = format!("{:#}", uri);

		if let Some(start) = uri_string.find("lightning=") {
			let end = uri_string[start..].find('&').map_or(uri_string.len(), |i| start + i);
			let lighting_value = &uri_string[start + "lightning=".len()..end];
			let uppercase_lighting_invoice = lighting_value.to_uppercase();
			uri_string.replace_range(start + "lightning=".len()..end, &uppercase_lighting_invoice);
		}

		Ok(uri_string)
	}

	/// Sends a payment given a URI.
	///
	/// This method parses the provided URI string and attempts to send the payment. If the URI
	/// has an offer and or invoice, it will try to pay the offer first followed by the invoice.
	/// If they both fail, the on-chain payment will be attempted.
	///
	/// Returns a `PaymentId` if the offer or invoice is paid, and a `Txid` if the on-chain
	/// transaction is paid, or an `Error` if there was an issue with parsing the URI,
	/// determining the network, or sending the payment.
	pub fn send(&self, uri_str: &str) -> Result<PaymentResult, Error> {
		let uri: bip21::Uri<NetworkUnchecked, Extras> =
			uri_str.parse().map_err(|_| Error::InvalidUri)?;

		let uri = uri
			.clone()
			.require_network(Network::Bitcoin)
			.or_else(|_| uri.clone().require_network(Network::Testnet))
			.or_else(|_| uri.clone().require_network(Network::Regtest))
			.or_else(|_| uri.clone().require_network(Network::Signet))
			.map_err(|_| Error::InvalidNetwork)?;

		if let Some(offer) = uri.extras.bolt12_offer {
			match self.bolt12_payment.send(&offer, None) {
				Ok(payment_id) => return Ok(PaymentResult::Bolt12 { payment_id }),
				Err(e) => log_error!(self.logger, "Failed to generate Bolt12 offer: {:?}", e),
			}
		}

		if let Some(invoice) = uri.extras.bolt11_invoice {
			match self.bolt11_invoice.send(&invoice) {
				Ok(payment_id) => return Ok(PaymentResult::Bolt11 { payment_id }),
				Err(e) => log_error!(self.logger, "Failed to send BOLT11 invoice: {:?}", e),
			}
		}

		let txid = self
			.onchain_payment
			.send_to_address(&uri.address, uri.amount.unwrap_or_default().to_sat())?;
		Ok(PaymentResult::Onchain { txid })
	}
}

/// Represents the PaymentId or Txid while using a bip21 QR code.
pub enum PaymentResult {
	/// * txid - The transaction ID of the on-chain payment.
	Onchain {
		///
		txid: Txid,
	},
	/// * payment_id - The payment ID of the BOLT11 invoice.
	Bolt11 {
		///
		payment_id: PaymentId,
	},
	/// *payment_id - The payment ID of the BOLT12 offer.
	Bolt12 {
		///
		payment_id: PaymentId,
	},
}

impl<'a> SerializeParams for &'a Extras {
	type Key = &'static str;
	type Value = String;
	type Iterator = IntoIter<(Self::Key, Self::Value)>;

	fn serialize_params(self) -> Self::Iterator {
		let mut params = Vec::new();

		if let Some(bolt11_invoice) = &self.bolt11_invoice {
			params.push(("lightning", bolt11_invoice.to_string()));
		}

		params.into_iter()
	}
}

impl<'a> DeserializeParams<'a> for Extras {
	type DeserializationState = DeserializationState;
}

#[derive(Default)]
struct DeserializationState {
	bolt11_invoice: Option<Bolt11Invoice>,
	bolt12_offer: Option<Offer>,
}

impl<'a> bip21::de::DeserializationState<'a> for DeserializationState {
	type Value = Extras;

	fn is_param_known(&self, key: &str) -> bool {
		key == "lightning"
	}

	fn deserialize_temp(
		&mut self, key: &str, value: Param<'_>,
	) -> Result<ParamKind, <Self::Value as DeserializationError>::Error> {
		if key == "lightning" {
			let lighting_str = String::try_from(value).map_err(|_| Error::UriParameterFailed)?;

			for prefix in lighting_str.split('&') {
				let prefix_lowercase = prefix.to_lowercase();
				if prefix_lowercase.starts_with("lnbc")
					|| prefix_lowercase.starts_with("lntb")
					|| prefix_lowercase.starts_with("lnbcrt")
					|| prefix_lowercase.starts_with("lnsb")
				{
					let invoice =
						prefix.parse::<Bolt11Invoice>().map_err(|_| Error::InvalidInvoice)?;
					self.bolt11_invoice = Some(invoice)
				} else if prefix_lowercase.starts_with("lno") {
					let offer = prefix.parse::<Offer>().map_err(|_| Error::InvalidOffer)?;
					self.bolt12_offer = Some(offer)
				}
			}
			Ok(bip21::de::ParamKind::Known)
		} else {
			Ok(bip21::de::ParamKind::Unknown)
		}
	}

	fn finalize(self) -> Result<Self::Value, <Self::Value as DeserializationError>::Error> {
		Ok(Extras { bolt11_invoice: self.bolt11_invoice, bolt12_offer: self.bolt12_offer })
	}
}

impl DeserializationError for Extras {
	type Error = Error;
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::builder::NodeBuilder;
	use crate::config::Config;
	use crate::payment::unified_qr::Extras;
	use bitcoin::{Address, Network};
	use std::str::FromStr;
	use std::sync::{Arc, RwLock};
	use tokio::runtime::Runtime;

	fn unified_qr_payment_handler() -> UnifiedQrPayment {
		let mut config = Config::default();
		config.network = Network::Bitcoin;

		let builder = NodeBuilder::from_config(config);
		let node = builder.build().unwrap();

		let liquidity_source = &node.liquidity_source;
		let peer_store = &node.peer_store;
		let payment_store = &node.payment_store;
		let runtime = Arc::new(RwLock::new(Some(Runtime::new().unwrap())));
		let channel_mgr = &node.channel_manager;
		let connection_mgr = &node.connection_manager;
		let key_mgr = &node.keys_manager;
		let config = &node.config;
		let logger = &node.logger;

		let bolt11_invoice = Bolt11Payment::new(
			runtime.clone(),
			channel_mgr.clone(),
			connection_mgr.clone(),
			key_mgr.clone(),
			liquidity_source.clone(),
			payment_store.clone(),
			peer_store.clone(),
			config.clone(),
			logger.clone(),
		);

		let bolt12_offer = Bolt12Payment::new(
			runtime.clone(),
			channel_mgr.clone(),
			payment_store.clone(),
			logger.clone(),
		);

		let wallet = &node.wallet;
		let onchain_payment = OnchainPayment::new(
			runtime.clone(),
			wallet.clone(),
			channel_mgr.clone(),
			config.clone(),
			logger.clone(),
		);

		let unified_qr_payment = UnifiedQrPayment::new(
			Arc::new(onchain_payment),
			Arc::new(bolt11_invoice),
			Arc::new(bolt12_offer),
			logger.clone(),
		);
		unified_qr_payment
	}

	#[test]
	fn create_uri() {
		let unified_qr_payment = unified_qr_payment_handler();

		let amount_sats = 100_000_000;
		let message = Some("Test Message".to_string());
		let expiry_sec = 4000;

		let uqr_payment = unified_qr_payment.receive(amount_sats, message.clone(), expiry_sec);
		match uqr_payment.clone() {
			Ok(ref uri) => {
				assert!(uri.contains("BITCOIN:"));
				assert!(uri.contains("lightning="));
				println!("Generated URI: {}\n", uri);
			},
			Err(e) => panic!("Failed to generate URI: {:?}", e),
		}
	}

	#[test]
	fn parse_uri() {
		let uri_test1 = "BITCOIN:TB1QRSCD05XNY6QZ63TF9GJELGVK6D3UDJFEKK62VU?amount=1&message=Test%20message&lightning=LNTB1000M1PNXWM7MDQ523JHXAPQD4JHXUMPVAJSNP4QWP9QD2JFP8DUZ46JQG5LTKVDH04YG52G6UF2YAXP8H7YZPZM3DM5PP5KUP7YT429UP9Z4ACPA60R7WETSTL66549MG05P0JN0C4L2NCC40SSP5R0LH86DJCL0NK8HZHNZHX92VVUAAVNE48Z5RVKVY5DKTRQ0DMP7S9QYYSGQCQPCXQRRAQYR59FGN2VVC5R6DS0AZMETH493ZU56H0WSVMGYCW9LEPZ032PGQNZMQ6XKVEH90Z02C0NH3J5QGDAWCS2YC2ZNP22J0ZD0PPF78N4QQQEXTYS2";
		let parsed_uri = uri_test1
			.parse::<bip21::Uri<NetworkUnchecked, Extras>>()
			.expect("Failed Parsing")
			.require_network(Network::Testnet)
			.expect("Invalid Network");

		assert_eq!(
			parsed_uri.address,
			bitcoin::Address::from_str("TB1QRSCD05XNY6QZ63TF9GJELGVK6D3UDJFEKK62VU")
				.unwrap()
				.require_network(Network::Testnet)
				.unwrap()
		);

		assert_eq!(Amount::from_sat(100000000), Amount::from(parsed_uri.amount.unwrap()));

		if let Some(invoice) = parsed_uri.extras.bolt11_invoice {
			assert_eq!(invoice, Bolt11Invoice::from_str("LNTB1000M1PNXWM7MDQ523JHXAPQD4JHXUMPVAJSNP4QWP9QD2JFP8DUZ46JQG5LTKVDH04YG52G6UF2YAXP8H7YZPZM3DM5PP5KUP7YT429UP9Z4ACPA60R7WETSTL66549MG05P0JN0C4L2NCC40SSP5R0LH86DJCL0NK8HZHNZHX92VVUAAVNE48Z5RVKVY5DKTRQ0DMP7S9QYYSGQCQPCXQRRAQYR59FGN2VVC5R6DS0AZMETH493ZU56H0WSVMGYCW9LEPZ032PGQNZMQ6XKVEH90Z02C0NH3J5QGDAWCS2YC2ZNP22J0ZD0PPF78N4QQQEXTYS2").unwrap());
		} else {
			panic!("No Lightning invoice found");
		}

		let zeus_test = "bitcoin:TB1QQ32G6LM2XKT0U2UGASH5DC4CFT3JTPEW65PZZ5?lightning=LNTB500U1PN89HH6PP5MA7K6DRM5SYVD05NTXMGSRNM728J7EHM8KV6VC96YNLKN7G7VDYQDQQCQZRCXQR8Q7SP5HU30L0EEXKYYPQSQYEZELZWUPT62HLJ0KV2662CALGPAML50QPXQ9QXPQYSGQDKTVFXEC8H2DG2GY3C95ETAJ0QKX50XAUCU304PPFV2SQVGFHZ6RMZWJV8MC3M0LXF3GW852C5VSK0DELK0JHLYUTYZDF7QKNAMT4PQQQN24WM&amount=0.0005";
		let uri_test2 = zeus_test
			.parse::<bip21::Uri<NetworkUnchecked, Extras>>()
			.expect("Failed Parsing")
			.require_network(Network::Testnet)
			.expect("Invalid Network");

		assert_eq!(
			uri_test2.address,
			bitcoin::Address::from_str("TB1QQ32G6LM2XKT0U2UGASH5DC4CFT3JTPEW65PZZ5")
				.unwrap()
				.require_network(Network::Testnet)
				.unwrap()
		);

		if let Some(invoice) = uri_test2.extras.bolt11_invoice {
			assert_eq!(invoice, Bolt11Invoice::from_str("LNTB500U1PN89HH6PP5MA7K6DRM5SYVD05NTXMGSRNM728J7EHM8KV6VC96YNLKN7G7VDYQDQQCQZRCXQR8Q7SP5HU30L0EEXKYYPQSQYEZELZWUPT62HLJ0KV2662CALGPAML50QPXQ9QXPQYSGQDKTVFXEC8H2DG2GY3C95ETAJ0QKX50XAUCU304PPFV2SQVGFHZ6RMZWJV8MC3M0LXF3GW852C5VSK0DELK0JHLYUTYZDF7QKNAMT4PQQQN24WM").unwrap());
		} else {
			panic!("No offer found.");
		}
		assert_eq!(Amount::from(uri_test2.amount.unwrap()), Amount::from_sat(50000));

		let muun_test = "bitcoin:bc1q6fmtam67h8wxfwtpumhazhtwyrh3uf039n058zke9xt5hr4ljzwsdcm2pj?amount=0.01&lightning=lnbc10m1pn8g2j4pp575tg4wt8jwgu2lvtk3aj6hy7mc6tnupw07wwkxcvyhtt3wlzw0zsdqqcqzzgxqyz5vqrzjqwnvuc0u4txn35cafc7w94gxvq5p3cu9dd95f7hlrh0fvs46wpvhdv6dzdeg0ww2eyqqqqryqqqqthqqpysp5fkd3k2rzvwdt2av068p58evf6eg50q0eftfhrpugaxkuyje4d25q9qrsgqqkfmnn67s5g6hadrcvf5h0l7p92rtlkwrfqdvc7uuf6lew0czxksvqhyux3zjrl3tlakwhtvezwl24zshnfumukwh0yntqsng9z6glcquvw7kc";
		let uri_test3 = muun_test
			.parse::<bip21::Uri<NetworkUnchecked, Extras>>()
			.expect("Failed Parsing")
			.require_network(Network::Bitcoin)
			.expect("Invalid Network");
		assert_eq!(
			uri_test3.address,
			bitcoin::Address::from_str(
				"bc1q6fmtam67h8wxfwtpumhazhtwyrh3uf039n058zke9xt5hr4ljzwsdcm2pj"
			)
			.unwrap()
			.require_network(Network::Bitcoin)
			.unwrap()
		);

		if let Some(invoice) = uri_test3.extras.bolt11_invoice {
			assert_eq!(invoice, Bolt11Invoice::from_str("lnbc10m1pn8g2j4pp575tg4wt8jwgu2lvtk3aj6hy7mc6tnupw07wwkxcvyhtt3wlzw0zsdqqcqzzgxqyz5vqrzjqwnvuc0u4txn35cafc7w94gxvq5p3cu9dd95f7hlrh0fvs46wpvhdv6dzdeg0ww2eyqqqqryqqqqthqqpysp5fkd3k2rzvwdt2av068p58evf6eg50q0eftfhrpugaxkuyje4d25q9qrsgqqkfmnn67s5g6hadrcvf5h0l7p92rtlkwrfqdvc7uuf6lew0czxksvqhyux3zjrl3tlakwhtvezwl24zshnfumukwh0yntqsng9z6glcquvw7kc").unwrap());
		} else {
			panic!("No invoice found");
		}
		assert_eq!(Amount::from(uri_test3.amount.unwrap()), Amount::from_sat(1_000_000));

		let muun_test_no_amount = "bitcoin:bc1qwe94y974pjl9kg5afg8tmsc0nz4hct04u78hdhukxvnnphgu48hs9lx3k5?lightning=lnbc1pn8g249pp5f6ytj32ty90jhvw69enf30hwfgdhyymjewywcmfjevflg6s4z86qdqqcqzzgxqyz5vqrzjqwnvuc0u4txn35cafc7w94gxvq5p3cu9dd95f7hlrh0fvs46wpvhdfjjzh2j9f7ye5qqqqryqqqqthqqpysp5mm832athgcal3m7h35sc29j63lmgzvwc5smfjh2es65elc2ns7dq9qrsgqu2xcje2gsnjp0wn97aknyd3h58an7sjj6nhcrm40846jxphv47958c6th76whmec8ttr2wmg6sxwchvxmsc00kqrzqcga6lvsf9jtqgqy5yexa";
		let uri_test4 = muun_test_no_amount
			.parse::<bip21::Uri<NetworkUnchecked, Extras>>()
			.expect("Failed Parsing")
			.require_network(Network::Bitcoin)
			.expect("Invalid Network");
		assert_eq!(
			uri_test4.address,
			Address::from_str("bc1qwe94y974pjl9kg5afg8tmsc0nz4hct04u78hdhukxvnnphgu48hs9lx3k5")
				.unwrap()
				.require_network(Network::Bitcoin)
				.unwrap()
		);
		if let Some(invoice) = uri_test4.extras.bolt11_invoice {
			assert_eq!(invoice, Bolt11Invoice::from_str("lnbc1pn8g249pp5f6ytj32ty90jhvw69enf30hwfgdhyymjewywcmfjevflg6s4z86qdqqcqzzgxqyz5vqrzjqwnvuc0u4txn35cafc7w94gxvq5p3cu9dd95f7hlrh0fvs46wpvhdfjjzh2j9f7ye5qqqqryqqqqthqqpysp5mm832athgcal3m7h35sc29j63lmgzvwc5smfjh2es65elc2ns7dq9qrsgqu2xcje2gsnjp0wn97aknyd3h58an7sjj6nhcrm40846jxphv47958c6th76whmec8ttr2wmg6sxwchvxmsc00kqrzqcga6lvsf9jtqgqy5yexa").unwrap());
		} else {
			panic!("No Invoice found");
		}
	}
}
