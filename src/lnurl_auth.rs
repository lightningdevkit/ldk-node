// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::logger::{log_debug, log_error, Logger};
use crate::types::KeysManager;
use crate::Error;

use bitcoin::hashes::{hex::FromHex, sha256, Hash, HashEngine, Hmac, HmacEngine};
use bitcoin::secp256k1::{Message, Secp256k1, SecretKey};
use lightning::util::logger::Logger as LdkLogger;

use bitcoin::bech32;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const LUD13_MESSAGE: &str = "DO NOT EVER SIGN THIS TEXT WITH YOUR PRIVATE KEYS! IT IS ONLY USED FOR DERIVATION OF LNURL-AUTH HASHING-KEY, DISCLOSING ITS SIGNATURE WILL COMPROMISE YOUR LNURL-AUTH IDENTITY AND MAY LEAD TO LOSS OF FUNDS!";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LnurlAuthResponse {
	status: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	reason: Option<String>,
}

/// An LNURL-auth handler providing authentication with LNURL-auth compatible services.
///
/// LNURL-auth allows secure, privacy-preserving authentication using domain-specific keys
/// derived from the node's master key. Each domain gets a unique key, ensuring privacy
/// while allowing consistent authentication across sessions.
#[derive(Clone)]
pub struct LnurlAuth {
	hashing_key: SecretKey,
	client: Client,
	logger: Arc<Logger>,
}

impl LnurlAuth {
	pub(crate) fn new(keys_manager: &KeysManager, logger: Arc<Logger>) -> Self {
		let hash = sha256::Hash::hash(LUD13_MESSAGE.as_bytes());
		let sig = keys_manager.sign_message(hash.as_byte_array());
		let hashed_sig = sha256::Hash::hash(sig.as_bytes());
		let hashing_key = SecretKey::from_slice(hashed_sig.as_byte_array())
			.expect("32 bytes, within curve order");
		let client = Client::new();
		Self { hashing_key, client, logger }
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
		let url = reqwest::Url::parse(&lnurl_auth_url).map_err(|e| {
			log_error!(self.logger, "Invalid LNURL-auth URL: {e}");
			Error::InvalidLnurl
		})?;

		let domain = url.host_str().ok_or_else(|| {
			log_error!(self.logger, "No domain found in LNURL-auth URL");
			Error::InvalidLnurl
		})?;

		// get query parameters for k1 and tag
		let query_params: std::collections::HashMap<_, _> =
			url.query_pairs().into_owned().collect();

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
		let linking_secret_key = self.derive_linking_key(domain)?;
		let secp = Secp256k1::signing_only();
		let linking_public_key = linking_secret_key.public_key(&secp);

		// Sign the challenge
		let message = Message::from_digest_slice(&k1_bytes).map_err(|e| {
			log_error!(self.logger, "Failed to create message from k1: {e}");
			Error::LnurlAuthFailed
		})?;

		let signature = secp.sign_ecdsa(&message, &linking_secret_key);

		// Submit authentication response
		let auth_url = format!("{lnurl_auth_url}&sig={signature}&key={linking_public_key}");

		log_debug!(self.logger, "Submitting LNURL-auth response");
		let auth_response = self.client.get(&auth_url).send().await.map_err(|e| {
			log_error!(self.logger, "Failed to submit LNURL-auth response: {e}");
			Error::LnurlAuthFailed
		})?;

		let response: LnurlAuthResponse = auth_response.json().await.map_err(|e| {
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

	fn derive_linking_key(&self, domain: &str) -> Result<SecretKey, Error> {
		// Create HMAC-SHA256 of the domain using node secret as key
		let mut hmac_engine = HmacEngine::<sha256::Hash>::new(&self.hashing_key[..]);
		hmac_engine.input(domain.as_bytes());
		let hmac_result = Hmac::from_engine(hmac_engine);

		// Use HMAC result as the linking private key
		SecretKey::from_slice(hmac_result.as_byte_array()).map_err(|e| {
			log_error!(self.logger, "Failed to derive linking key: {e}");
			Error::LnurlAuthFailed
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn build_auth(hashing_key: [u8; 32]) -> LnurlAuth {
		let hashing_key = SecretKey::from_slice(&hashing_key).unwrap();
		let logger = Arc::new(Logger::new_log_facade());
		LnurlAuth::new(hashing_key, logger)
	}

	#[test]
	fn test_deterministic_key_derivation() {
		let auth = build_auth([42u8; 32]);
		let domain = "example.com";

		// Keys should be identical for the same inputs
		let key1 = auth.derive_linking_key(domain).unwrap();
		let key2 = auth.derive_linking_key(domain).unwrap();
		assert_eq!(key1, key2);

		// Keys should be different for different domains
		let key3 = auth.derive_linking_key("different.com").unwrap();
		assert_ne!(key1, key3);

		// Keys should be different for different master keys
		let different_master = build_auth([24u8; 32]);
		let key4 = different_master.derive_linking_key(domain).unwrap();
		assert_ne!(key1, key4);
	}

	#[test]
	fn test_domain_isolation() {
		let auth = build_auth([42u8; 32]);
		let domains = ["example.com", "test.org", "service.net"];
		let mut keys = Vec::with_capacity(domains.len());

		for domain in &domains {
			keys.push(auth.derive_linking_key(domain).unwrap());
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
}
