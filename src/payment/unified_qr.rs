// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Holds a payment handler allowing to create [BIP 21] URIs with an on-chain, [BOLT 11], and [BOLT 12] payment
//! options.
//!
//! [BIP 21]: https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki
//! [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
//! [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
use crate::error::Error;
use crate::ffi::maybe_wrap;
use crate::logger::{log_error, LdkLogger, Logger};
use crate::payment::{Bolt11Payment, Bolt12Payment, OnchainPayment};
use crate::Config;

use lightning::ln::channelmanager::PaymentId;
use lightning::offers::offer::Offer;
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};

use bip21::de::ParamKind;
use bip21::{DeserializationError, DeserializeParams, Param, SerializeParams};
use bitcoin::address::{NetworkChecked, NetworkUnchecked};
use bitcoin::{Amount, Txid};

use std::sync::Arc;
use std::vec::IntoIter;

type Uri<'a> = bip21::Uri<'a, NetworkChecked, Extras>;

#[derive(Debug, Clone)]
struct Extras {
	bolt11_invoice: Option<Bolt11Invoice>,
	bolt12_offer: Option<Offer>,
}

/// A payment handler allowing to create [BIP 21] URIs with an on-chain, [BOLT 11], and [BOLT 12] payment
/// option.
///
/// Should be retrieved by calling [`Node::unified_qr_payment`]
///
/// [BIP 21]: https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki
/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
/// [`Node::unified_qr_payment`]: crate::Node::unified_qr_payment
pub struct UnifiedQrPayment {
	onchain_payment: Arc<OnchainPayment>,
	bolt11_invoice: Arc<Bolt11Payment>,
	bolt12_payment: Arc<Bolt12Payment>,
	config: Arc<Config>,
	logger: Arc<Logger>,
}

impl UnifiedQrPayment {
	pub(crate) fn new(
		onchain_payment: Arc<OnchainPayment>, bolt11_invoice: Arc<Bolt11Payment>,
		bolt12_payment: Arc<Bolt12Payment>, config: Arc<Config>, logger: Arc<Logger>,
	) -> Self {
		Self { onchain_payment, bolt11_invoice, bolt12_payment, config, logger }
	}

	/// Generates a URI with an on-chain address, [BOLT 11] invoice and [BOLT 12] offer.
	///
	/// The URI allows users to send the payment request allowing the wallet to decide
	/// which payment method to use. This enables a fallback mechanism: older wallets
	/// can always pay using the provided on-chain address, while newer wallets will
	/// typically opt to use the provided BOLT11 invoice or BOLT12 offer.
	///
	/// The URI will always include an on-chain address. A BOLT11 invoice will be included
	/// unless invoice generation fails, while a BOLT12 offer will only be included when
	/// the node has suitable channels for routing.
	///
	/// # Parameters
	/// - `amount_sats`: The amount to be received, specified in satoshis.
	/// - `description`: A description or note associated with the payment.
	///   This message is visible to the payer and can provide context or details about the payment.
	/// - `expiry_sec`: The expiration time for the payment, specified in seconds.
	///
	/// Returns a payable URI that can be used to request and receive a payment of the amount
	/// given. Failure to generate the on-chain address will result in an error return
	/// (`Error::WalletOperationFailed`), while failures in invoice or offer generation will
	/// result in those components being omitted from the URI.
	///
	/// The generated URI can then be given to a QR code library.
	///
	/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
	/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
	pub fn receive(
		&self, amount_sats: u64, description: &str, expiry_sec: u32,
	) -> Result<String, Error> {
		let onchain_address = self.onchain_payment.new_address()?;

		let amount_msats = amount_sats * 1_000;

		let bolt12_offer =
			match self.bolt12_payment.receive_inner(amount_msats, description, None, None) {
				Ok(offer) => Some(offer),
				Err(e) => {
					log_error!(self.logger, "Failed to create offer: {}", e);
					None
				},
			};

		let invoice_description = Bolt11InvoiceDescription::Direct(
			Description::new(description.to_string()).map_err(|_| Error::InvoiceCreationFailed)?,
		);
		let bolt11_invoice = match self.bolt11_invoice.receive_inner(
			Some(amount_msats),
			&invoice_description,
			expiry_sec,
			None,
		) {
			Ok(invoice) => Some(invoice),
			Err(e) => {
				log_error!(self.logger, "Failed to create invoice {}", e);
				None
			},
		};

		let extras = Extras { bolt11_invoice, bolt12_offer };

		let mut uri = Uri::with_extras(onchain_address, extras);
		uri.amount = Some(Amount::from_sat(amount_sats));
		uri.message = Some(description.into());

		Ok(format_uri(uri))
	}

	/// Sends a payment given a [BIP 21] URI.
	///
	/// This method parses the provided URI string and attempts to send the payment. If the URI
	/// has an offer and or invoice, it will try to pay the offer first followed by the invoice.
	/// If they both fail, the on-chain payment will be paid.
	///
	/// Returns a `QrPaymentResult` indicating the outcome of the payment. If an error
	/// occurs, an `Error` is returned detailing the issue encountered.
	///
	/// [BIP 21]: https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki
	pub fn send(&self, uri_str: &str) -> Result<QrPaymentResult, Error> {
		let uri: bip21::Uri<NetworkUnchecked, Extras> =
			uri_str.parse().map_err(|_| Error::InvalidUri)?;

		let uri_network_checked =
			uri.clone().require_network(self.config.network).map_err(|_| Error::InvalidNetwork)?;

		if let Some(offer) = uri_network_checked.extras.bolt12_offer {
			let offer = maybe_wrap(offer);
			match self.bolt12_payment.send(&offer, None, None) {
				Ok(payment_id) => return Ok(QrPaymentResult::Bolt12 { payment_id }),
				Err(e) => log_error!(self.logger, "Failed to send BOLT12 offer: {:?}. This is part of a unified QR code payment. Falling back to the BOLT11 invoice.", e),
			}
		}

		if let Some(invoice) = uri_network_checked.extras.bolt11_invoice {
			let invoice = maybe_wrap(invoice);
			match self.bolt11_invoice.send(&invoice, None) {
				Ok(payment_id) => return Ok(QrPaymentResult::Bolt11 { payment_id }),
				Err(e) => log_error!(self.logger, "Failed to send BOLT11 invoice: {:?}. This is part of a unified QR code payment. Falling back to the on-chain transaction.", e),
			}
		}

		let amount = match uri_network_checked.amount {
			Some(amount) => amount,
			None => {
				log_error!(self.logger, "No amount specified in the URI. Aborting the payment.");
				return Err(Error::InvalidAmount);
			},
		};

		let txid = self.onchain_payment.send_to_address(
			&uri_network_checked.address,
			amount.to_sat(),
			None,
		)?;

		Ok(QrPaymentResult::Onchain { txid })
	}
}

/// Represents the result of a payment made using a [BIP 21] QR code.
///
/// After a successful on-chain transaction, the transaction ID ([`Txid`]) is returned.
/// For BOLT11 and BOLT12 payments, the corresponding [`PaymentId`] is returned.
///
/// [BIP 21]: https://github.com/bitcoin/bips/blob/master/bip-0021.mediawiki
/// [`PaymentId`]: lightning::ln::channelmanager::PaymentId
/// [`Txid`]: bitcoin::hash_types::Txid
#[derive(Debug)]
pub enum QrPaymentResult {
	/// An on-chain payment.
	Onchain {
		/// The transaction ID (txid) of the on-chain payment.
		txid: Txid,
	},
	/// A [BOLT 11] payment.
	///
	/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
	Bolt11 {
		/// The payment ID for the BOLT11 invoice.
		payment_id: PaymentId,
	},
	/// A [BOLT 12] offer payment, i.e., a payment for an [`Offer`].
	///
	/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
	/// [`Offer`]: crate::lightning::offers::offer::Offer
	Bolt12 {
		/// The payment ID for the BOLT12 offer.
		payment_id: PaymentId,
	},
}

fn format_uri(uri: bip21::Uri<NetworkChecked, Extras>) -> String {
	let mut uri = format!("{:#}", uri);

	fn value_to_uppercase(uri: &mut String, key: &str) {
		let mut start = 0;
		while let Some(index) = uri[start..].find(key) {
			let start_index = start + index;
			let end_index = uri[start_index..].find('&').map_or(uri.len(), |i| start_index + i);
			let lightning_value = &uri[start_index + key.len()..end_index];
			let uppercase_lighting_value = lightning_value.to_uppercase();
			uri.replace_range(start_index + key.len()..end_index, &uppercase_lighting_value);
			start = end_index
		}
	}
	value_to_uppercase(&mut uri, "lightning=");
	value_to_uppercase(&mut uri, "lno=");
	uri
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
		if let Some(bolt12_offer) = &self.bolt12_offer {
			params.push(("lno", bolt12_offer.to_string()));
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
		key == "lightning" || key == "lno"
	}

	fn deserialize_temp(
		&mut self, key: &str, value: Param<'_>,
	) -> Result<ParamKind, <Self::Value as DeserializationError>::Error> {
		match key {
			"lightning" => {
				let bolt11_value =
					String::try_from(value).map_err(|_| Error::UriParameterParsingFailed)?;
				let invoice = bolt11_value
					.parse::<Bolt11Invoice>()
					.map_err(|_| Error::UriParameterParsingFailed)?;
				self.bolt11_invoice = Some(invoice);
				Ok(bip21::de::ParamKind::Known)
			},
			"lno" => {
				let bolt12_value =
					String::try_from(value).map_err(|_| Error::UriParameterParsingFailed)?;
				let offer =
					bolt12_value.parse::<Offer>().map_err(|_| Error::UriParameterParsingFailed)?;
				self.bolt12_offer = Some(offer);
				Ok(bip21::de::ParamKind::Known)
			},
			_ => Ok(bip21::de::ParamKind::Unknown),
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
	use crate::payment::unified_qr::Extras;
	use bitcoin::{Address, Network};
	use std::str::FromStr;

	#[test]
	fn parse_uri() {
		let uri_test1 = "BITCOIN:TB1QRSCD05XNY6QZ63TF9GJELGVK6D3UDJFEKK62VU?amount=1&message=Test%20message&lightning=LNTB1000M1PNXWM7MDQ523JHXAPQD4JHXUMPVAJSNP4QWP9QD2JFP8DUZ46JQG5LTKVDH04YG52G6UF2YAXP8H7YZPZM3DM5PP5KUP7YT429UP9Z4ACPA60R7WETSTL66549MG05P0JN0C4L2NCC40SSP5R0LH86DJCL0NK8HZHNZHX92VVUAAVNE48Z5RVKVY5DKTRQ0DMP7S9QYYSGQCQPCXQRRAQYR59FGN2VVC5R6DS0AZMETH493ZU56H0WSVMGYCW9LEPZ032PGQNZMQ6XKVEH90Z02C0NH3J5QGDAWCS2YC2ZNP22J0ZD0PPF78N4QQQEXTYS2";
		let expected_bolt11_invoice_1 = "LNTB1000M1PNXWM7MDQ523JHXAPQD4JHXUMPVAJSNP4QWP9QD2JFP8DUZ46JQG5LTKVDH04YG52G6UF2YAXP8H7YZPZM3DM5PP5KUP7YT429UP9Z4ACPA60R7WETSTL66549MG05P0JN0C4L2NCC40SSP5R0LH86DJCL0NK8HZHNZHX92VVUAAVNE48Z5RVKVY5DKTRQ0DMP7S9QYYSGQCQPCXQRRAQYR59FGN2VVC5R6DS0AZMETH493ZU56H0WSVMGYCW9LEPZ032PGQNZMQ6XKVEH90Z02C0NH3J5QGDAWCS2YC2ZNP22J0ZD0PPF78N4QQQEXTYS2";
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

		assert_eq!(Amount::from_sat(100_000_000), Amount::from(parsed_uri.amount.unwrap()));

		if let Some(invoice) = parsed_uri.extras.bolt11_invoice {
			assert_eq!(invoice, Bolt11Invoice::from_str(expected_bolt11_invoice_1).unwrap());
		} else {
			panic!("No Lightning invoice found");
		}

		let uri_with_offer =  "BITCOIN:BCRT1QM0NW9S05QDPGC6F52FPKA9U6Q6VWTT5WVS30R2?amount=0.001&message=asdf&lightning=LNBCRT1M1PNGMY98DQ8V9EKGESNP4QDH5SL00QK4842UZMZVJVX2NLUZT4E6P2ZC2DLAGCU565TP42AUDYPP5XD0PRS5CRDLZVU8DNQQU08W9F4YP0XRXW06ZSHCLCHZU9X28HSSSSP5ES30JG9J4VK2CRW80YXTLRJU2M097TXMFTHR00VC5V0LGKVMURRQ9QYYSGQCQPCXQRRAQRZJQ0Q0K9CDYFSVZAJ5V3PDWYWDMHLEYCVD7TG0SVMY4AM4P6GQZJZ5XQQQQYQQX2QQQUQQQQLGQQQQQQQQFQWDQZX24PSHN68A9D4X4HD89F3XVC7DGGRDTFCA5WH4KZ546GSRTJVACA34QQ3DZ9W4JHLJD3XZRW44RA0RET6RDSRJCEZQC6AXANX6QPHZKHJK&lno=LNO1QGSQVGNWGCG35Z6EE2H3YCZRADDM72XRFUA9UVE2RLRM9DEU7XYFZRCYZPGTGRDWMGU44QPYUXLHLLMLWN4QSPQ97HSSQZSYV9EKGESSWCPK7JRAAUZ6574TSTVFJFSE20LSFWH8G9GTPFHL4RRJN23VX4TH35SRWKCNQ6S8R9ZW9HU5RXMPXVYCJVK2KY3NTEA8VXZTMWJF4NAJCCAQZQ7YZ7KDDZ600LAW2S2E7Q6XDYLPSMLMV4YAY0QXX5NC8QH05JRNUYQPQCAHK8Y5KQ8H9X624LS6A9GWFTGKYYPUZVUKKM93DWETTL8A7NE84L7SNHCSGR006EACQRQP8YWY6WPS0TS";
		let expected_bolt11_invoice_2 = "LNBCRT1M1PNGMY98DQ8V9EKGESNP4QDH5SL00QK4842UZMZVJVX2NLUZT4E6P2ZC2DLAGCU565TP42AUDYPP5XD0PRS5CRDLZVU8DNQQU08W9F4YP0XRXW06ZSHCLCHZU9X28HSSSSP5ES30JG9J4VK2CRW80YXTLRJU2M097TXMFTHR00VC5V0LGKVMURRQ9QYYSGQCQPCXQRRAQRZJQ0Q0K9CDYFSVZAJ5V3PDWYWDMHLEYCVD7TG0SVMY4AM4P6GQZJZ5XQQQQYQQX2QQQUQQQQLGQQQQQQQQFQWDQZX24PSHN68A9D4X4HD89F3XVC7DGGRDTFCA5WH4KZ546GSRTJVACA34QQ3DZ9W4JHLJD3XZRW44RA0RET6RDSRJCEZQC6AXANX6QPHZKHJK";
		let expected_bolt12_offer_2 = "LNO1QGSQVGNWGCG35Z6EE2H3YCZRADDM72XRFUA9UVE2RLRM9DEU7XYFZRCYZPGTGRDWMGU44QPYUXLHLLMLWN4QSPQ97HSSQZSYV9EKGESSWCPK7JRAAUZ6574TSTVFJFSE20LSFWH8G9GTPFHL4RRJN23VX4TH35SRWKCNQ6S8R9ZW9HU5RXMPXVYCJVK2KY3NTEA8VXZTMWJF4NAJCCAQZQ7YZ7KDDZ600LAW2S2E7Q6XDYLPSMLMV4YAY0QXX5NC8QH05JRNUYQPQCAHK8Y5KQ8H9X624LS6A9GWFTGKYYPUZVUKKM93DWETTL8A7NE84L7SNHCSGR006EACQRQP8YWY6WPS0TS";
		let parsed_uri_with_offer = uri_with_offer
			.parse::<bip21::Uri<NetworkUnchecked, Extras>>()
			.expect("Failed Parsing")
			.require_network(Network::Regtest)
			.expect("Invalid Network");

		assert_eq!(Amount::from_sat(100_000), Amount::from(parsed_uri_with_offer.amount.unwrap()));

		assert_eq!(
			parsed_uri_with_offer.address,
			bitcoin::Address::from_str("BCRT1QM0NW9S05QDPGC6F52FPKA9U6Q6VWTT5WVS30R2")
				.unwrap()
				.require_network(Network::Regtest)
				.unwrap()
		);

		if let Some(invoice) = parsed_uri_with_offer.extras.bolt11_invoice {
			assert_eq!(invoice, Bolt11Invoice::from_str(expected_bolt11_invoice_2).unwrap());
		} else {
			panic!("No invoice found.")
		}

		if let Some(offer) = parsed_uri_with_offer.extras.bolt12_offer {
			assert_eq!(offer, Offer::from_str(expected_bolt12_offer_2).unwrap());
		} else {
			panic!("No offer found.");
		}

		let zeus_test = "bitcoin:TB1QQ32G6LM2XKT0U2UGASH5DC4CFT3JTPEW65PZZ5?lightning=LNTB500U1PN89HH6PP5MA7K6DRM5SYVD05NTXMGSRNM728J7EHM8KV6VC96YNLKN7G7VDYQDQQCQZRCXQR8Q7SP5HU30L0EEXKYYPQSQYEZELZWUPT62HLJ0KV2662CALGPAML50QPXQ9QXPQYSGQDKTVFXEC8H2DG2GY3C95ETAJ0QKX50XAUCU304PPFV2SQVGFHZ6RMZWJV8MC3M0LXF3GW852C5VSK0DELK0JHLYUTYZDF7QKNAMT4PQQQN24WM&amount=0.0005";
		let expected_bolt11_invoice_3 = "LNTB500U1PN89HH6PP5MA7K6DRM5SYVD05NTXMGSRNM728J7EHM8KV6VC96YNLKN7G7VDYQDQQCQZRCXQR8Q7SP5HU30L0EEXKYYPQSQYEZELZWUPT62HLJ0KV2662CALGPAML50QPXQ9QXPQYSGQDKTVFXEC8H2DG2GY3C95ETAJ0QKX50XAUCU304PPFV2SQVGFHZ6RMZWJV8MC3M0LXF3GW852C5VSK0DELK0JHLYUTYZDF7QKNAMT4PQQQN24WM";
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
			assert_eq!(invoice, Bolt11Invoice::from_str(expected_bolt11_invoice_3).unwrap());
		} else {
			panic!("No invoice found.");
		}
		assert_eq!(Amount::from(uri_test2.amount.unwrap()), Amount::from_sat(50000));

		let muun_test = "bitcoin:bc1q6fmtam67h8wxfwtpumhazhtwyrh3uf039n058zke9xt5hr4ljzwsdcm2pj?amount=0.01&lightning=lnbc10m1pn8g2j4pp575tg4wt8jwgu2lvtk3aj6hy7mc6tnupw07wwkxcvyhtt3wlzw0zsdqqcqzzgxqyz5vqrzjqwnvuc0u4txn35cafc7w94gxvq5p3cu9dd95f7hlrh0fvs46wpvhdv6dzdeg0ww2eyqqqqryqqqqthqqpysp5fkd3k2rzvwdt2av068p58evf6eg50q0eftfhrpugaxkuyje4d25q9qrsgqqkfmnn67s5g6hadrcvf5h0l7p92rtlkwrfqdvc7uuf6lew0czxksvqhyux3zjrl3tlakwhtvezwl24zshnfumukwh0yntqsng9z6glcquvw7kc";
		let expected_bolt11_invoice_4 = "lnbc10m1pn8g2j4pp575tg4wt8jwgu2lvtk3aj6hy7mc6tnupw07wwkxcvyhtt3wlzw0zsdqqcqzzgxqyz5vqrzjqwnvuc0u4txn35cafc7w94gxvq5p3cu9dd95f7hlrh0fvs46wpvhdv6dzdeg0ww2eyqqqqryqqqqthqqpysp5fkd3k2rzvwdt2av068p58evf6eg50q0eftfhrpugaxkuyje4d25q9qrsgqqkfmnn67s5g6hadrcvf5h0l7p92rtlkwrfqdvc7uuf6lew0czxksvqhyux3zjrl3tlakwhtvezwl24zshnfumukwh0yntqsng9z6glcquvw7kc";
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
			assert_eq!(invoice, Bolt11Invoice::from_str(expected_bolt11_invoice_4).unwrap());
		} else {
			panic!("No invoice found");
		}
		assert_eq!(Amount::from(uri_test3.amount.unwrap()), Amount::from_sat(1_000_000));

		let muun_test_no_amount = "bitcoin:bc1qwe94y974pjl9kg5afg8tmsc0nz4hct04u78hdhukxvnnphgu48hs9lx3k5?lightning=lnbc1pn8g249pp5f6ytj32ty90jhvw69enf30hwfgdhyymjewywcmfjevflg6s4z86qdqqcqzzgxqyz5vqrzjqwnvuc0u4txn35cafc7w94gxvq5p3cu9dd95f7hlrh0fvs46wpvhdfjjzh2j9f7ye5qqqqryqqqqthqqpysp5mm832athgcal3m7h35sc29j63lmgzvwc5smfjh2es65elc2ns7dq9qrsgqu2xcje2gsnjp0wn97aknyd3h58an7sjj6nhcrm40846jxphv47958c6th76whmec8ttr2wmg6sxwchvxmsc00kqrzqcga6lvsf9jtqgqy5yexa";
		let expected_bolt11_invoice_5 = "lnbc1pn8g249pp5f6ytj32ty90jhvw69enf30hwfgdhyymjewywcmfjevflg6s4z86qdqqcqzzgxqyz5vqrzjqwnvuc0u4txn35cafc7w94gxvq5p3cu9dd95f7hlrh0fvs46wpvhdfjjzh2j9f7ye5qqqqryqqqqthqqpysp5mm832athgcal3m7h35sc29j63lmgzvwc5smfjh2es65elc2ns7dq9qrsgqu2xcje2gsnjp0wn97aknyd3h58an7sjj6nhcrm40846jxphv47958c6th76whmec8ttr2wmg6sxwchvxmsc00kqrzqcga6lvsf9jtqgqy5yexa";
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
			assert_eq!(invoice, Bolt11Invoice::from_str(expected_bolt11_invoice_5).unwrap());
		} else {
			panic!("No invoice found");
		}
	}
}
