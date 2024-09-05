use prost::Message;

use crate::error::LdkNodeServerError;
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;

const APPLICATION_OCTET_STREAM: &str = "application/octet-stream";

/// Client to access a hosted instance of LDK Node Server.
#[derive(Clone)]
pub struct LdkNodeServerClient {
	base_url: String,
	client: Client,
}

impl LdkNodeServerClient {
	/// Constructs a [`LdkNodeServerClient`] using `base_url` as the server endpoint.
	pub fn new(base_url: String) -> Self {
		Self { base_url, client: Client::new() }
	}

	async fn post_request<Rq: Message, Rs: Message + Default>(
		&self, request: &Rq, url: &str,
	) -> Result<Rs, LdkNodeServerError> {
		let request_body = request.encode_to_vec();
		let response_raw = match self
			.client
			.post(url)
			.header(CONTENT_TYPE, APPLICATION_OCTET_STREAM)
			.body(request_body)
			.send()
			.await
		{
			Ok(response) => response,
			Err(e) => {
				return Err(LdkNodeServerError::InternalError(e.to_string()));
			},
		};
		let status = response_raw.status();
		let payload = response_raw.bytes().await?;

		if status.is_success() {
			Ok(Rs::decode(&payload[..])?)
		} else {
			//TODO: Error handling and error response parsing.
			Err(LdkNodeServerError::InternalError("Unknown Error".to_string()))
		}
	}
}
