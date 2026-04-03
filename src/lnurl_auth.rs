// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::sync::Arc;

use bitcoin::bech32;
use bitcoin::bip32::{ChildNumber, Xpriv};
use bitcoin::hashes::hex::FromHex;
use bitcoin::hashes::{sha256, Hash, HashEngine, Hmac, HmacEngine};
use bitcoin::secp256k1::{All, Message, Secp256k1, SecretKey};
use bitreq::Client;
use lightning::util::logger::Logger as LdkLogger;
use serde::{Deserialize, Serialize};

use crate::logger::{log_debug, log_error, Logger};
use crate::Error;

/// The BIP-32 hardened child index used for LNURL-auth key derivation as defined by LUD-05.
pub(crate) const LNURL_AUTH_HARDENED_CHILD_INDEX: u32 = 138;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LnurlAuthResponse {
	status: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	reason: Option<String>,
}

/// An LNURL-auth handler providing authentication with LNURL-auth compatible services.
///
/// LNURL-auth allows secure, privacy-preserving authentication using domain-specific keys
/// derived from the node's master key via [LUD-05] BIP-32 key derivation. Each domain gets
/// a unique key, ensuring privacy while allowing consistent authentication across sessions.
///
/// [LUD-05]: https://github.com/lnurl/luds/blob/luds/05.md
#[derive(Clone)]
pub struct LnurlAuth {
	/// The xpriv at m/138', used for deriving domain-specific linking keys.
	lnurl_auth_base_xprv: Xpriv,
	/// The private key at m/138'/0, used as the HMAC key for derivation material.
	hashing_key: [u8; 32],
	client: Client,
	secp: Secp256k1<All>,
	logger: Arc<Logger>,
}

impl LnurlAuth {
	pub(crate) fn new(xprv: Xpriv, logger: Arc<Logger>) -> Self {
		let secp = Secp256k1::new();
		let lnurl_auth_base_xprv = xprv
			.derive_priv(&secp, &[ChildNumber::Hardened { index: LNURL_AUTH_HARDENED_CHILD_INDEX }])
			.expect("BIP-32 derivation of m/138' should not fail");
		let hashing_key_xprv = lnurl_auth_base_xprv
			.derive_priv(&secp, &[ChildNumber::Normal { index: 0 }])
			.expect("BIP-32 derivation of m/138'/0 should not fail");
		let hashing_key = hashing_key_xprv.private_key.secret_bytes();
		let client = Client::new(2);
		Self { lnurl_auth_base_xprv, hashing_key, client, secp, logger }
	}

	/// Authenticates with an LNURL-auth compatible service using the provided URL.
	///
	/// The authentication process involves:
	/// 1. Fetching the challenge from the service
	/// 2. Deriving a domain-specific linking key
	/// 3. Signing the challenge with the linking key
	/// 4. Submitting the signed response to complete authentication
	///
	/// Returns `Ok(())` if authentication succeeds, or an error if the process fails.
	pub async fn authenticate(&self, lnurl: &str) -> Result<(), Error> {
		let (hrp, bytes) = bech32::decode(lnurl).map_err(|e| {
			log_error!(self.logger, "Failed to decode LNURL: {e}");
			Error::InvalidLnurl
		})?;

		if hrp.to_lowercase() != "lnurl" {
			log_error!(self.logger, "Invalid LNURL prefix: {hrp}");
			return Err(Error::InvalidLnurl);
		}

		let lnurl_auth_url = String::from_utf8(bytes).map_err(|e| {
			log_error!(self.logger, "Failed to convert LNURL bytes to string: {e}");
			Error::InvalidLnurl
		})?;

		log_debug!(self.logger, "Starting LNURL-auth process for URL: {lnurl_auth_url}");

		// Parse the URL to extract domain and parameters
		let url = bitreq::Url::parse(&lnurl_auth_url).map_err(|e| {
			log_error!(self.logger, "Invalid LNURL-auth URL: {e}");
			Error::InvalidLnurl
		})?;

		let domain = url.base_url();

		// Enforce HTTPS for non-localhost URLs per LNURL spec.
		let is_localhost = domain == "localhost" || domain == "127.0.0.1" || domain == "[::1]";
		if url.scheme() != "https" && !is_localhost {
			log_error!(self.logger, "LNURL-auth URL must use HTTPS for non-localhost domains");
			return Err(Error::InvalidLnurl);
		}

		// get query parameters for k1 and tag
		let query_params: std::collections::HashMap<_, _> = url.query_pairs().collect();

		let tag = query_params.get("tag").ok_or_else(|| {
			log_error!(self.logger, "No tag parameter found in LNURL-auth URL");
			Error::InvalidLnurl
		})?;

		if tag != "login" {
			log_error!(self.logger, "Invalid tag parameter in LNURL-auth URL: {tag}");
			return Err(Error::InvalidLnurl);
		}

		let k1 = query_params.get("k1").ok_or_else(|| {
			log_error!(self.logger, "No k1 parameter found in LNURL-auth URL");
			Error::InvalidLnurl
		})?;

		let k1_bytes: [u8; 32] = FromHex::from_hex(k1).map_err(|e| {
			log_error!(self.logger, "Invalid k1 hex in challenge: {e}");
			Error::LnurlAuthFailed
		})?;

		// Derive domain-specific linking key
		let linking_secret_key = self.derive_linking_key(&self.secp, domain)?;
		let linking_public_key = linking_secret_key.public_key(&self.secp);

		// Sign the challenge
		let message = Message::from_digest_slice(&k1_bytes).map_err(|e| {
			log_error!(self.logger, "Failed to create message from k1: {e}");
			Error::LnurlAuthFailed
		})?;

		let signature = self.secp.sign_ecdsa(&message, &linking_secret_key);

		// Submit authentication response
		let auth_url = format!("{lnurl_auth_url}&sig={signature}&key={linking_public_key}");

		log_debug!(self.logger, "Submitting LNURL-auth response");
		let request = bitreq::get(&auth_url).with_max_redirects(0);
		let auth_response = self.client.send_async(request).await.map_err(|e| {
			log_error!(self.logger, "Failed to submit LNURL-auth response: {e}");
			Error::LnurlAuthFailed
		})?;

		let response: LnurlAuthResponse = auth_response.json().map_err(|e| {
			log_error!(self.logger, "Failed to parse LNURL-auth response: {e}");
			Error::LnurlAuthFailed
		})?;

		if response.status == "OK" {
			log_debug!(self.logger, "LNURL-auth authentication successful");
			Ok(())
		} else {
			let reason = response.reason.unwrap_or_else(|| "Unknown error".to_string());
			log_error!(self.logger, "LNURL-auth authentication failed: {reason}");
			Err(Error::LnurlAuthFailed)
		}
	}

	fn derive_linking_key(&self, secp: &Secp256k1<All>, domain: &str) -> Result<SecretKey, Error> {
		let path_indices = linking_key_path(&self.hashing_key, domain);

		// Derive the linking key at m/138'/[idx1]/[idx2]/[idx3]/[idx4]
		let linking_xprv =
			self.lnurl_auth_base_xprv.derive_priv(&secp, &path_indices).map_err(|e| {
				log_error!(self.logger, "Failed to derive linking key: {e}");
				Error::LnurlAuthFailed
			})?;

		Ok(SecretKey::from(linking_xprv.private_key))
	}
}

/// Computes the LUD-05 linking key path for a given domain.
///
/// Takes the hashing key (private key at `m/138'/0`) and a domain name, and returns the 4
/// `ChildNumber` path components derived from `HMAC-SHA256(hashing_key, domain)`.
fn linking_key_path(hashing_key: &[u8; 32], domain_name: &str) -> Vec<ChildNumber> {
	let mut engine = HmacEngine::<sha256::Hash>::new(&hashing_key[..]);
	engine.input(domain_name.as_bytes());
	let result = Hmac::<sha256::Hash>::from_engine(engine).to_byte_array();
	// unwrap safety: We take 4-byte chunks, so TryInto for [u8; 4] never fails.
	result
		.chunks_exact(4)
		.take(4)
		.map(|i| {
			u32::from_be_bytes(i.try_into().expect("chunks_exact(4) should yield 4-byte slices"))
		})
		.map(ChildNumber::from)
		.collect()
}

#[cfg(test)]
mod tests {
	use bitcoin::Network;

	use super::*;

	fn build_auth(seed: [u8; 32]) -> LnurlAuth {
		let logger = Arc::new(Logger::new_log_facade());
		let xprv = Xpriv::new_master(Network::Bitcoin, &seed).unwrap();
		LnurlAuth::new(xprv, logger)
	}

	/// Build an LnurlAuth from a base xprv that is already at m/138'.
	fn build_auth_from_base_xprv(base_xprv: Xpriv) -> LnurlAuth {
		let secp = Secp256k1::new();
		let hashing_key_xprv =
			base_xprv.derive_priv(&secp, &[ChildNumber::Normal { index: 0 }]).unwrap();
		let hashing_key = hashing_key_xprv.private_key.secret_bytes();
		let logger = Arc::new(Logger::new_log_facade());
		let client = Client::new(2);
		LnurlAuth { lnurl_auth_base_xprv: base_xprv, hashing_key, client, logger, secp }
	}

	#[test]
	fn test_deterministic_key_derivation() {
		let secp = Secp256k1::new();
		let auth = build_auth([42u8; 32]);
		let domain = "example.com";

		// Keys should be identical for the same inputs
		let key1 = auth.derive_linking_key(&secp, domain).unwrap();
		let key2 = auth.derive_linking_key(&secp, domain).unwrap();
		assert_eq!(key1, key2);

		// Keys should be different for different domains
		let key3 = auth.derive_linking_key(&secp, "different.com").unwrap();
		assert_ne!(key1, key3);

		// Keys should be different for different master keys
		let different_master = build_auth([24u8; 32]);
		let key4 = different_master.derive_linking_key(&secp, domain).unwrap();
		assert_ne!(key1, key4);
	}

	#[test]
	fn test_domain_isolation() {
		let secp = Secp256k1::new();
		let auth = build_auth([42u8; 32]);
		let domains = ["example.com", "test.org", "service.net"];
		let mut keys = Vec::with_capacity(domains.len());

		for domain in &domains {
			keys.push(auth.derive_linking_key(&secp, domain).unwrap());
		}

		for i in 0..keys.len() {
			for j in 0..keys.len() {
				if i == j {
					continue;
				}
				assert_ne!(
					keys[i], keys[j],
					"Keys for {} and {} should be different",
					domains[i], domains[j]
				);
			}
		}
	}

	/// Test vector from LUD-05 specification.
	/// https://github.com/lnurl/luds/blob/luds/05.md
	#[test]
	fn test_lud05_linking_key_path_vector() {
		let hashing_key: [u8; 32] =
			FromHex::from_hex("7d417a6a5e9a6a4a879aeaba11a11838764c8fa2b959c242d43dea682b3e409b")
				.unwrap();
		let path = linking_key_path(&hashing_key, "site.com");
		let numbers: Vec<u32> = path.iter().map(|c| u32::from(*c)).collect();
		assert_eq!(numbers, vec![1588488367, 2659270754, 38110259, 4136336762]);
	}

	/// Test vector matching vss-client's sign_lnurl test to ensure compatible derivation.
	#[test]
	fn test_sign_lnurl_vector() {
		let secp = Secp256k1::new();
		let parent_key_bytes: [u8; 32] =
			FromHex::from_hex("abababababababababababababababababababababababababababababababab")
				.unwrap();
		let base_xprv = Xpriv::new_master(Network::Testnet, &parent_key_bytes).unwrap();
		let auth = build_auth_from_base_xprv(base_xprv);

		let domain = "example.com";
		let k1_hex = "e2af6254a8df433264fa23f67eb8188635d15ce883e8fc020989d5f82ae6f11e";
		let k1_bytes: [u8; 32] = FromHex::from_hex(k1_hex).unwrap();

		let linking_secret_key = auth.derive_linking_key(&secp, domain).unwrap();
		let linking_public_key = linking_secret_key.public_key(&secp);

		let message = Message::from_digest_slice(&k1_bytes).unwrap();
		let signature = secp.sign_ecdsa(&message, &linking_secret_key);

		assert_eq!(
			format!("{linking_public_key}"),
			"02d9eb1b467517d685e3b5439082c14bb1a2c9ae672df4d9046d208c193a5846e0"
		);
		assert_eq!(
			format!("{signature}"),
			"3045022100a75df468de452e618edb8030016eb0894204655c7d93ece1be007fcf36843522022048bc2f00a0a5a30601d274b49cfaf9ef4c76176e5401d0dfb195f5d6ab8ab4c4"
		);
	}
}
