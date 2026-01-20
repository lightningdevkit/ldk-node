// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! CLI-specific type wrappers for API responses.
//!
//! This file contains wrapper types that customize the serialization format
//! of API responses for CLI output. These wrappers ensure that the CLI's output
//! format matches what users expect and what the CLI can parse back as input.

use ldk_server_client::ldk_server_protos::types::{ForwardedPayment, PageToken, Payment};
use serde::Serialize;

/// CLI-specific wrapper for paginated responses that formats the page token
/// as "token:idx" instead of a JSON object.
#[derive(Debug, Clone, Serialize)]
pub struct CliPaginatedResponse<T> {
	/// List of items.
	pub list: Vec<T>,
	/// Next page token formatted as "token:idx", or None if no more pages.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub next_page_token: Option<String>,
}

impl<T> CliPaginatedResponse<T> {
	pub fn new(list: Vec<T>, next_page_token: Option<PageToken>) -> Self {
		Self { list, next_page_token: next_page_token.map(format_page_token) }
	}
}

pub type CliListPaymentsResponse = CliPaginatedResponse<Payment>;
pub type CliListForwardedPaymentsResponse = CliPaginatedResponse<ForwardedPayment>;

fn format_page_token(token: PageToken) -> String {
	format!("{}:{}", token.token, token.index)
}
