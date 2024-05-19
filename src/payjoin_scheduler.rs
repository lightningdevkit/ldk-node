use bitcoin::{secp256k1::PublicKey, ScriptBuf, TxOut};

#[derive(Clone)]
pub struct PayjoinScheduler {
	channels: Vec<PayjoinChannel>,
}

impl PayjoinScheduler {
	/// Create a new empty channel scheduler.
	pub fn new() -> Self {
		Self { channels: vec![] }
	}
	/// Schedule a new channel.
	///
	/// The channel will be created with `ScheduledChannelState::ChannelCreated` state.
	pub fn schedule(
		&mut self, channel_value_satoshi: bitcoin::Amount, counterparty_node_id: PublicKey,
		channel_id: u128,
	) {
		let channel = PayjoinChannel::new(channel_value_satoshi, counterparty_node_id, channel_id);
		match channel.state {
			ScheduledChannelState::ChannelCreated => {
				self.channels.push(channel);
			},
			_ => {},
		}
	}
	/// Mark a channel as accepted.
	///
	/// The channel will be updated to `ScheduledChannelState::ChannelAccepted` state.
	pub fn set_channel_accepted(
		&mut self, channel_id: u128, output_script: &ScriptBuf, temporary_channel_id: [u8; 32],
	) -> bool {
		for channel in &mut self.channels {
			if channel.channel_id() == channel_id {
				channel.state.set_channel_accepted(output_script, temporary_channel_id);
				return true;
			}
		}
		false
	}
	/// Mark a channel as funding tx created.
	///
	/// The channel will be updated to `ScheduledChannelState::FundingTxCreated` state.
	pub fn set_funding_tx_created(
		&mut self, channel_id: u128, url: &payjoin::Url, body: Vec<u8>,
	) -> bool {
		for channel in &mut self.channels {
			if channel.channel_id() == channel_id {
				return channel.state.set_channel_funding_tx_created(url.clone(), body);
			}
		}
		false
	}
	/// Mark a channel as funding tx signed.
	///
	/// The channel will be updated to `ScheduledChannelState::FundingTxSigned` state.
	pub fn set_funding_tx_signed(
		&mut self, tx: bitcoin::Transaction,
	) -> Option<(payjoin::Url, Vec<u8>)> {
		for output in tx.output.iter() {
			if let Some(mut channel) = self.internal_find_by_tx_out(&output.clone()) {
				let info = channel.request_info();
				if info.is_some() && channel.state.set_channel_funding_tx_signed(output.clone()) {
					return info;
				}
			}
		}
		None
	}
	/// Get the next channel matching the given channel amount.
	///
	/// The channel must be in the accepted state.
	///
	/// If more than one channel matches the given channel amount, the channel with the oldest
	/// creation date will be returned.
	pub fn get_next_channel(
		&self, channel_amount: bitcoin::Amount,
	) -> Option<(u128, bitcoin::Address, [u8; 32], bitcoin::Amount, bitcoin::secp256k1::PublicKey)>
	{
		let channel = self
			.channels
			.iter()
			.filter(|channel| {
				channel.channel_value_satoshi() == channel_amount
					&& channel.is_channel_accepted()
					&& channel.output_script().is_some()
					&& channel.temporary_channel_id().is_some()
			})
			.min_by_key(|channel| channel.created_at());

		if let Some(channel) = channel {
			let address = bitcoin::Address::from_script(
				&channel.output_script().unwrap(),
				bitcoin::Network::Regtest, // fixme
			);
			if let Ok(address) = address {
				return Some((
					channel.channel_id(),
					address,
					channel.temporary_channel_id().unwrap(),
					channel.channel_value_satoshi(),
					channel.counterparty_node_id(),
				));
			}
		};
		None
	}

	/// List all channels.
	pub fn list_channels(&self) -> &Vec<PayjoinChannel> {
		&self.channels
	}

	pub fn in_progress(&self) -> bool {
		self.channels.iter().any(|channel| !channel.is_channel_accepted())
	}
	fn internal_find_by_tx_out(&self, txout: &TxOut) -> Option<PayjoinChannel> {
		let channel = self.channels.iter().find(|channel| {
			return Some(&txout.script_pubkey) == channel.output_script();
		});
		channel.cloned()
	}
}

/// A struct representing a scheduled channel.
#[derive(Clone, Debug)]
pub struct PayjoinChannel {
	state: ScheduledChannelState,
	channel_value_satoshi: bitcoin::Amount,
	channel_id: u128,
	counterparty_node_id: PublicKey,
	created_at: u64,
}

impl PayjoinChannel {
	pub fn new(
		channel_value_satoshi: bitcoin::Amount, counterparty_node_id: PublicKey, channel_id: u128,
	) -> Self {
		Self {
			state: ScheduledChannelState::ChannelCreated,
			channel_value_satoshi,
			channel_id,
			counterparty_node_id,
			created_at: 0,
		}
	}

	fn is_channel_accepted(&self) -> bool {
		match self.state {
			ScheduledChannelState::ChannelAccepted(..) => true,
			_ => false,
		}
	}

	pub fn channel_value_satoshi(&self) -> bitcoin::Amount {
		self.channel_value_satoshi
	}

	/// Get the user channel id.
	pub fn channel_id(&self) -> u128 {
		self.channel_id
	}

	/// Get the counterparty node id.
	pub fn counterparty_node_id(&self) -> PublicKey {
		self.counterparty_node_id
	}

	/// Get the output script.
	pub fn output_script(&self) -> Option<&ScriptBuf> {
		self.state.output_script()
	}

	/// Get the temporary channel id.
	pub fn temporary_channel_id(&self) -> Option<[u8; 32]> {
		self.state.temporary_channel_id()
	}

	/// Get the temporary channel id.
	pub fn tx_out(&self) -> Option<&TxOut> {
		match &self.state {
			ScheduledChannelState::FundingTxSigned(_, txout) => Some(txout),
			_ => None,
		}
	}

	pub fn request_info(&self) -> Option<(payjoin::Url, Vec<u8>)> {
		match &self.state {
			ScheduledChannelState::FundingTxCreated(_, url, body) => {
				Some((url.clone(), body.clone()))
			},
			_ => None,
		}
	}

	fn created_at(&self) -> u64 {
		self.created_at
	}
}

#[derive(Clone, Debug)]
struct FundingTxParams {
	output_script: ScriptBuf,
	temporary_channel_id: [u8; 32],
}

impl FundingTxParams {
	fn new(output_script: ScriptBuf, temporary_channel_id: [u8; 32]) -> Self {
		Self { output_script, temporary_channel_id }
	}
}

#[derive(Clone, Debug)]
enum ScheduledChannelState {
	ChannelCreated,
	ChannelAccepted(FundingTxParams),
	FundingTxCreated(FundingTxParams, payjoin::Url, Vec<u8>),
	FundingTxSigned(FundingTxParams, TxOut),
}

impl ScheduledChannelState {
	fn output_script(&self) -> Option<&ScriptBuf> {
		match self {
			ScheduledChannelState::ChannelAccepted(funding_tx_params) => {
				Some(&funding_tx_params.output_script)
			},
			ScheduledChannelState::FundingTxCreated(funding_tx_params, _, _) => {
				Some(&funding_tx_params.output_script)
			},
			ScheduledChannelState::FundingTxSigned(funding_tx_params, _) => {
				Some(&funding_tx_params.output_script)
			},
			_ => None,
		}
	}

	fn temporary_channel_id(&self) -> Option<[u8; 32]> {
		match self {
			ScheduledChannelState::ChannelAccepted(funding_tx_params) => {
				Some(funding_tx_params.temporary_channel_id)
			},
			ScheduledChannelState::FundingTxCreated(funding_tx_params, _, _) => {
				Some(funding_tx_params.temporary_channel_id)
			},
			ScheduledChannelState::FundingTxSigned(funding_tx_params, _) => {
				Some(funding_tx_params.temporary_channel_id)
			},
			_ => None,
		}
	}

	fn set_channel_accepted(
		&mut self, output_script: &ScriptBuf, temporary_channel_id: [u8; 32],
	) -> bool {
		if let ScheduledChannelState::ChannelCreated = self {
			*self = ScheduledChannelState::ChannelAccepted(FundingTxParams::new(
				output_script.clone(),
				temporary_channel_id,
			));
			return true;
		}
		return false;
	}

	fn set_channel_funding_tx_created(&mut self, url: payjoin::Url, body: Vec<u8>) -> bool {
		if let ScheduledChannelState::ChannelAccepted(funding_tx_params) = self {
			*self = ScheduledChannelState::FundingTxCreated(funding_tx_params.clone(), url, body);
			return true;
		}
		return false;
	}

	fn set_channel_funding_tx_signed(&mut self, output: TxOut) -> bool {
		let mut res = false;
		if let ScheduledChannelState::FundingTxCreated(funding_tx_params, _, _) = self {
			*self =
				ScheduledChannelState::FundingTxSigned(funding_tx_params.clone(), output.clone());
			res = true;
		}
		return res;
	}
}

// #[cfg(test)]
// mod tests {
// 	use std::str::FromStr;

// 	use super::*;
// 	use bitcoin::{
// 		psbt::Psbt,
// 		secp256k1::{self, Secp256k1},
// 	};

// 	#[ignore]
// 	#[test]
// 	fn test_channel_scheduler() {
// 		let create_pubkey = || -> PublicKey {
// 			let secp = Secp256k1::new();
// 			PublicKey::from_secret_key(&secp, &secp256k1::SecretKey::from_slice(&[1; 32]).unwrap())
// 		};
// 		let channel_value_satoshi = 100;
// 		let node_id = create_pubkey();
// 		let channel_id: u128 = 0;
// 		let mut channel_scheduler = PayjoinScheduler::new();
// 		channel_scheduler.schedule(
// 			bitcoin::Amount::from_sat(channel_value_satoshi),
// 			node_id,
// 			channel_id,
// 		);
// 		assert_eq!(channel_scheduler.channels.len(), 1);
// 		assert_eq!(channel_scheduler.is_channel_created(channel_id), true);
// 		channel_scheduler.set_channel_accepted(
// 			channel_id,
// 			&ScriptBuf::from(vec![1, 2, 3]),
// 			[0; 32],
// 		);
// 		assert_eq!(channel_scheduler.is_channel_accepted(channel_id), true);
// 		let str_psbt = "cHNidP8BAHMCAAAAAY8nutGgJdyYGXWiBEb45Hoe9lWGbkxh/6bNiOJdCDuDAAAAAAD+////AtyVuAUAAAAAF6kUHehJ8GnSdBUOOv6ujXLrWmsJRDCHgIQeAAAAAAAXqRR3QJbbz0hnQ8IvQ0fptGn+votneofTAAAAAAEBIKgb1wUAAAAAF6kU3k4ekGHKWRNbA1rV5tR5kEVDVNCHAQcXFgAUx4pFclNVgo1WWAdN1SYNX8tphTABCGsCRzBEAiB8Q+A6dep+Rz92vhy26lT0AjZn4PRLi8Bf9qoB/CMk0wIgP/Rj2PWZ3gEjUkTlhDRNAQ0gXwTO7t9n+V14pZ6oljUBIQMVmsAaoNWHVMS02LfTSe0e388LNitPa1UQZyOihY+FFgABABYAFEb2Giu6c4KO5YW0pfw3lGp9jMUUAAA=";
// 		let mock_transaction = Psbt::from_str(str_psbt).unwrap();
// 		let _our_txout = mock_transaction.clone().extract_tx().output[0].clone();
// 		// channel_scheduler.set_funding_tx_created(channel_id, mock_transaction.clone());
// 		// let tx_id = mock_transaction.extract_tx().txid();
// 		// assert_eq!(channel_scheduler.is_funding_tx_created(&tx_id), true);
// 		// channel_scheduler.set_funding_tx_signed(tx_id);
// 		// assert_eq!(channel_scheduler.is_funding_tx_signed(&tx_id), true);
// 	}
// }
