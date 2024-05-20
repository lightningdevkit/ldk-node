use crate::config::{PAYJOIN_REQUEST_TIMEOUT, PAYJOIN_RETRY_INTERVAL};
use crate::io::utils::ohttp_headers;
use crate::logger::FilesystemLogger;

use lightning::util::logger::Logger;
use lightning::{log_error, log_info};

use std::sync::Arc;

pub(crate) struct PayjoinSender {
	logger: Arc<FilesystemLogger>,
	payjoin_relay: payjoin::Url,
}

impl PayjoinSender {
	pub(crate) fn new(logger: Arc<FilesystemLogger>, payjoin_relay: payjoin::Url) -> Self {
		Self { logger, payjoin_relay }
	}

	pub(crate) fn payjoin_relay(&self) -> &payjoin::Url {
		&self.payjoin_relay
	}

	pub(crate) async fn send_request(&self, request: &payjoin::Request) -> Option<Vec<u8>> {
		let response = match reqwest::Client::new()
			.post(request.url.clone())
			.body(request.body.clone())
			.timeout(PAYJOIN_REQUEST_TIMEOUT)
			.headers(ohttp_headers())
			.send()
			.await
		{
			Ok(response) => response,
			Err(e) => {
				log_error!(
					self.logger,
					"Error trying to poll Payjoin response: {}, retrying in {} seconds",
					e,
					PAYJOIN_RETRY_INTERVAL.as_secs()
				);
				return None;
			},
		};
		if response.status() == reqwest::StatusCode::OK {
			match response.bytes().await.and_then(|r| Ok(r.to_vec())) {
				Ok(response) => {
					if response.is_empty() {
						log_info!(
						self.logger,
						"Got empty response while polling Payjoin response, retrying in {} seconds", PAYJOIN_RETRY_INTERVAL.as_secs()
				);
						return None;
					}
					return Some(response);
				},
				Err(e) => {
					log_error!(
						self.logger,
						"Error reading polling Payjoin response: {}, retrying in {} seconds",
						e,
						PAYJOIN_RETRY_INTERVAL.as_secs()
					);
					return None;
				},
			};
		} else {
			log_info!(
				self.logger,
				"Got status code {} while polling Payjoin response, retrying in {} seconds",
				response.status(),
				PAYJOIN_RETRY_INTERVAL.as_secs()
			);
			return None;
		}
	}
}
