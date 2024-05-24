use bitcoin::{secp256k1::PublicKey, Network, ScriptBuf, TxOut};

#[derive(Clone)]
pub struct PayjoinChannelScheduler {
	channels: Vec<PayjoinChannel>,
}

impl PayjoinChannelScheduler {
	pub(crate) fn new() -> Self {
		Self { channels: vec![] }
	}

	pub(crate) fn schedule(
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

	pub(crate) fn set_channel_accepted(
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

	pub(crate) fn set_funding_tx_created(
		&mut self, channel_id: u128, url: &payjoin::Url, body: Vec<u8>,
	) -> bool {
		for channel in &mut self.channels {
			if channel.channel_id() == channel_id {
				return channel.state.set_channel_funding_tx_created(url.clone(), body);
			}
		}
		false
	}

	pub(crate) fn set_funding_tx_signed(
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
	/// The channel must be in accepted state.
	///
	/// If more than one channel matches the given channel amount, the channel with the oldest
	/// creation date will be returned.
	pub(crate) fn get_next_channel(
		&self, channel_amount: bitcoin::Amount, network: Network,
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
			let address = bitcoin::Address::from_script(&channel.output_script().unwrap(), network);
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

	fn internal_find_by_tx_out(&self, txout: &TxOut) -> Option<PayjoinChannel> {
		let channel = self.channels.iter().find(|channel| {
			return Some(&txout.script_pubkey) == channel.output_script();
		});
		channel.cloned()
	}
}

#[derive(Clone, Debug)]
pub(crate) struct PayjoinChannel {
	state: ScheduledChannelState,
	channel_value_satoshi: bitcoin::Amount,
	channel_id: u128,
	counterparty_node_id: PublicKey,
	created_at: u64,
}

impl PayjoinChannel {
	pub(crate) fn new(
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

	pub(crate) fn channel_value_satoshi(&self) -> bitcoin::Amount {
		self.channel_value_satoshi
	}

	pub(crate) fn channel_id(&self) -> u128 {
		self.channel_id
	}

	pub(crate) fn counterparty_node_id(&self) -> PublicKey {
		self.counterparty_node_id
	}

	pub(crate) fn output_script(&self) -> Option<&ScriptBuf> {
		self.state.output_script()
	}

	pub(crate) fn temporary_channel_id(&self) -> Option<[u8; 32]> {
		self.state.temporary_channel_id()
	}

	pub(crate) fn request_info(&self) -> Option<(payjoin::Url, Vec<u8>)> {
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
	FundingTxSigned(FundingTxParams, ()),
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
			assert_eq!(funding_tx_params.output_script, output.script_pubkey);
			*self = ScheduledChannelState::FundingTxSigned(funding_tx_params.clone(), ());
			res = true;
		}
		return res;
	}
}
