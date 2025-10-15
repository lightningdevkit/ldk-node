// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::Arc;

use bdk_chain::bdk_core::{BlockId, ConfirmationBlockTime};
use bdk_chain::indexer::keychain_txout::ChangeSet as BdkIndexerChangeSet;
use bdk_chain::local_chain::ChangeSet as BdkLocalChainChangeSet;
use bdk_chain::tx_graph::ChangeSet as BdkTxGraphChangeSet;
use bdk_chain::DescriptorId;
use bdk_wallet::descriptor::Descriptor;
use bdk_wallet::keys::DescriptorPublicKey;
use bitcoin::hashes::sha256::Hash as Sha256Hash;
use bitcoin::p2p::Magic;
use bitcoin::{BlockHash, Network, OutPoint, Transaction, TxOut, Txid};
use lightning::ln::msgs::DecodeError;
use lightning::util::ser::{BigSize, Readable, RequiredWrapper, Writeable, Writer};
use lightning::{decode_tlv_stream, encode_tlv_stream, read_tlv_fields, write_tlv_fields};

const CHANGESET_SERIALIZATION_VERSION: u8 = 1;

pub(crate) struct ChangeSetSerWrapper<'a, T>(pub &'a T);
pub(crate) struct ChangeSetDeserWrapper<T>(pub T);

impl<'a> Writeable for ChangeSetSerWrapper<'a, Descriptor<DescriptorPublicKey>> {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		CHANGESET_SERIALIZATION_VERSION.write(writer)?;

		self.0.to_string().write(writer)
	}
}

impl Readable for ChangeSetDeserWrapper<Descriptor<DescriptorPublicKey>> {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let version: u8 = Readable::read(reader)?;
		if version != CHANGESET_SERIALIZATION_VERSION {
			return Err(DecodeError::UnknownVersion);
		}

		let descriptor_str: String = Readable::read(reader)?;
		let descriptor = Descriptor::<DescriptorPublicKey>::from_str(&descriptor_str)
			.map_err(|_| DecodeError::InvalidValue)?;
		Ok(Self(descriptor))
	}
}

impl<'a> Writeable for ChangeSetSerWrapper<'a, Network> {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		CHANGESET_SERIALIZATION_VERSION.write(writer)?;

		self.0.magic().to_bytes().write(writer)
	}
}

impl Readable for ChangeSetDeserWrapper<Network> {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let version: u8 = Readable::read(reader)?;
		if version != CHANGESET_SERIALIZATION_VERSION {
			return Err(DecodeError::UnknownVersion);
		}

		let buf: [u8; 4] = Readable::read(reader)?;
		let magic = Magic::from_bytes(buf);
		let network = Network::from_magic(magic).ok_or(DecodeError::InvalidValue)?;
		Ok(Self(network))
	}
}

impl<'a> Writeable for ChangeSetSerWrapper<'a, BdkLocalChainChangeSet> {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		CHANGESET_SERIALIZATION_VERSION.write(writer)?;

		encode_tlv_stream!(writer, {
			(0, self.0.blocks, required),
		});
		Ok(())
	}
}

impl Readable for ChangeSetDeserWrapper<BdkLocalChainChangeSet> {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let version: u8 = Readable::read(reader)?;
		if version != CHANGESET_SERIALIZATION_VERSION {
			return Err(DecodeError::UnknownVersion);
		}

		let mut blocks = RequiredWrapper(None);
		decode_tlv_stream!(reader, {
			(0, blocks, required),
		});
		Ok(Self(BdkLocalChainChangeSet { blocks: blocks.0.unwrap() }))
	}
}

impl<'a> Writeable for ChangeSetSerWrapper<'a, BdkTxGraphChangeSet<ConfirmationBlockTime>> {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		CHANGESET_SERIALIZATION_VERSION.write(writer)?;

		encode_tlv_stream!(writer, {
			(0, ChangeSetSerWrapper(&self.0.txs), required),
			(1, Some(&self.0.first_seen), option),
			(2, self.0.txouts, required),
			(3, Some(&self.0.last_evicted), option),
			(4, ChangeSetSerWrapper(&self.0.anchors), required),
			(6, self.0.last_seen, required),
		});
		Ok(())
	}
}

impl Readable for ChangeSetDeserWrapper<BdkTxGraphChangeSet<ConfirmationBlockTime>> {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let version: u8 = Readable::read(reader)?;
		if version != CHANGESET_SERIALIZATION_VERSION {
			return Err(DecodeError::UnknownVersion);
		}

		let mut txs: RequiredWrapper<ChangeSetDeserWrapper<BTreeSet<Arc<Transaction>>>> =
			RequiredWrapper(None);
		let mut txouts: RequiredWrapper<BTreeMap<OutPoint, TxOut>> = RequiredWrapper(None);
		let mut anchors: RequiredWrapper<
			ChangeSetDeserWrapper<BTreeSet<(ConfirmationBlockTime, Txid)>>,
		> = RequiredWrapper(None);
		let mut last_seen: RequiredWrapper<BTreeMap<Txid, u64>> = RequiredWrapper(None);
		let mut first_seen = None;
		let mut last_evicted = None;

		decode_tlv_stream!(reader, {
			(0, txs, required),
			(1, first_seen, option),
			(2, txouts, required),
			(3, last_evicted, option),
			(4, anchors, required),
			(6, last_seen, required),
		});

		Ok(Self(BdkTxGraphChangeSet {
			txs: txs.0.unwrap().0,
			txouts: txouts.0.unwrap(),
			anchors: anchors.0.unwrap().0,
			last_seen: last_seen.0.unwrap(),
			first_seen: first_seen.unwrap_or_default(),
			last_evicted: last_evicted.unwrap_or_default(),
		}))
	}
}

impl<'a> Writeable for ChangeSetSerWrapper<'a, BTreeSet<(ConfirmationBlockTime, Txid)>> {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		let len = BigSize(self.0.len() as u64);
		len.write(writer)?;
		for (time, txid) in self.0.iter() {
			write_tlv_fields!(writer, {
				(0, ChangeSetSerWrapper(time), required),
				(2, txid, required),
			});
		}
		Ok(())
	}
}

impl Readable for ChangeSetDeserWrapper<BTreeSet<(ConfirmationBlockTime, Txid)>> {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let len: BigSize = Readable::read(reader)?;
		let mut set = BTreeSet::new();
		for _ in 0..len.0 {
			let mut time: RequiredWrapper<ChangeSetDeserWrapper<ConfirmationBlockTime>> =
				RequiredWrapper(None);
			let mut txid: RequiredWrapper<Txid> = RequiredWrapper(None);
			read_tlv_fields!(reader, {
				(0, time, required),
				(2, txid, required),
			});
			set.insert((time.0.unwrap().0, txid.0.unwrap()));
		}
		Ok(Self(set))
	}
}

impl<'a> Writeable for ChangeSetSerWrapper<'a, BTreeSet<Arc<Transaction>>> {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		let len = BigSize(self.0.len() as u64);
		len.write(writer)?;
		for tx in self.0.iter() {
			write_tlv_fields!(writer, {
				(0, tx, required),
			});
		}
		Ok(())
	}
}

impl Readable for ChangeSetDeserWrapper<BTreeSet<Arc<Transaction>>> {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let len: BigSize = Readable::read(reader)?;
		let mut set = BTreeSet::new();
		for _ in 0..len.0 {
			let mut tx: RequiredWrapper<Transaction> = RequiredWrapper(None);
			read_tlv_fields!(reader, {
				(0, tx, required),
			});
			set.insert(Arc::new(tx.0.unwrap()));
		}
		Ok(Self(set))
	}
}

impl<'a> Writeable for ChangeSetSerWrapper<'a, ConfirmationBlockTime> {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		encode_tlv_stream!(writer, {
			(0, ChangeSetSerWrapper(&self.0.block_id), required),
			(2, self.0.confirmation_time, required),
		});
		Ok(())
	}
}

impl Readable for ChangeSetDeserWrapper<ConfirmationBlockTime> {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let mut block_id: RequiredWrapper<ChangeSetDeserWrapper<BlockId>> = RequiredWrapper(None);
		let mut confirmation_time: RequiredWrapper<u64> = RequiredWrapper(None);

		decode_tlv_stream!(reader, {
			(0, block_id, required),
			(2, confirmation_time, required),
		});

		Ok(Self(ConfirmationBlockTime {
			block_id: block_id.0.unwrap().0,
			confirmation_time: confirmation_time.0.unwrap(),
		}))
	}
}

impl<'a> Writeable for ChangeSetSerWrapper<'a, BlockId> {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		encode_tlv_stream!(writer, {
			(0, self.0.height, required),
			(2, self.0.hash, required),
		});
		Ok(())
	}
}

impl Readable for ChangeSetDeserWrapper<BlockId> {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let mut height: RequiredWrapper<u32> = RequiredWrapper(None);
		let mut hash: RequiredWrapper<BlockHash> = RequiredWrapper(None);
		decode_tlv_stream!(reader, {
			(0, height, required),
			(2, hash, required),
		});

		Ok(Self(BlockId { height: height.0.unwrap(), hash: hash.0.unwrap() }))
	}
}

impl<'a> Writeable for ChangeSetSerWrapper<'a, BdkIndexerChangeSet> {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		CHANGESET_SERIALIZATION_VERSION.write(writer)?;

		// Note we don't persist/use the optional spk_cache currently.
		encode_tlv_stream!(writer, { (0, ChangeSetSerWrapper(&self.0.last_revealed), required) });
		Ok(())
	}
}

impl Readable for ChangeSetDeserWrapper<BdkIndexerChangeSet> {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let version: u8 = Readable::read(reader)?;
		if version != CHANGESET_SERIALIZATION_VERSION {
			return Err(DecodeError::UnknownVersion);
		}

		let mut last_revealed: RequiredWrapper<ChangeSetDeserWrapper<BTreeMap<DescriptorId, u32>>> =
			RequiredWrapper(None);

		// Note we don't persist/use the optional spk_cache currently.
		decode_tlv_stream!(reader, { (0, last_revealed, required) });

		Ok(Self(BdkIndexerChangeSet {
			last_revealed: last_revealed.0.unwrap().0,
			spk_cache: Default::default(),
		}))
	}
}

impl<'a> Writeable for ChangeSetSerWrapper<'a, BTreeMap<DescriptorId, u32>> {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		let len = BigSize(self.0.len() as u64);
		len.write(writer)?;
		for (descriptor_id, last_index) in self.0.iter() {
			write_tlv_fields!(writer, {
				(0, ChangeSetSerWrapper(descriptor_id), required),
				(2, last_index, required),
			});
		}
		Ok(())
	}
}

impl Readable for ChangeSetDeserWrapper<BTreeMap<DescriptorId, u32>> {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let len: BigSize = Readable::read(reader)?;
		let mut set = BTreeMap::new();
		for _ in 0..len.0 {
			let mut descriptor_id: RequiredWrapper<ChangeSetDeserWrapper<DescriptorId>> =
				RequiredWrapper(None);
			let mut last_index: RequiredWrapper<u32> = RequiredWrapper(None);
			read_tlv_fields!(reader, {
				(0, descriptor_id, required),
				(2, last_index, required),
			});
			set.insert(descriptor_id.0.unwrap().0, last_index.0.unwrap());
		}
		Ok(Self(set))
	}
}

impl<'a> Writeable for ChangeSetSerWrapper<'a, DescriptorId> {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		encode_tlv_stream!(writer, { (0, ChangeSetSerWrapper(&self.0 .0), required) });
		Ok(())
	}
}

impl Readable for ChangeSetDeserWrapper<DescriptorId> {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let mut hash: RequiredWrapper<ChangeSetDeserWrapper<Sha256Hash>> = RequiredWrapper(None);

		decode_tlv_stream!(reader, { (0, hash, required) });

		Ok(Self(DescriptorId(hash.0.unwrap().0)))
	}
}

impl<'a> Writeable for ChangeSetSerWrapper<'a, Sha256Hash> {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		writer.write_all(&self.0[..])
	}
}

impl Readable for ChangeSetDeserWrapper<Sha256Hash> {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		use bitcoin::hashes::Hash;

		let buf: [u8; 32] = Readable::read(reader)?;
		Ok(Self(Sha256Hash::from_slice(&buf[..]).unwrap()))
	}
}
