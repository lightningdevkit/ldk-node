// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use std::fs;

use base64::Engine;
use rcgen::{generate_simple_self_signed, CertifiedKey};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;

use crate::util::config::TlsConfig;

// PEM markers
const PEM_CERT_BEGIN: &str = "-----BEGIN CERTIFICATE-----";
const PEM_CERT_END: &str = "-----END CERTIFICATE-----";
const PEM_KEY_BEGIN: &str = "-----BEGIN PRIVATE KEY-----";
const PEM_KEY_END: &str = "-----END PRIVATE KEY-----";

/// Gets or generates TLS configuration. If custom paths are provided, uses those.
/// Otherwise, generates a self-signed certificate in the storage directory.
pub fn get_or_generate_tls_config(
	tls_config: Option<TlsConfig>, storage_dir: &str,
) -> Result<ServerConfig, String> {
	if let Some(config) = tls_config {
		let cert_path = config.cert_path.unwrap_or(format!("{storage_dir}/tls.crt"));
		let key_path = config.key_path.unwrap_or(format!("{storage_dir}/tls.key"));
		if !fs::exists(&cert_path).unwrap_or(false) || !fs::exists(&key_path).unwrap_or(false) {
			generate_self_signed_cert(&cert_path, &key_path, &config.hosts)?;
		}
		load_tls_config(&cert_path, &key_path)
	} else {
		// Check if we already have generated certs, if we don't, generate new ones
		let cert_path = format!("{storage_dir}/tls.crt");
		let key_path = format!("{storage_dir}/tls.key");
		if !fs::exists(&cert_path).unwrap_or(false) || !fs::exists(&key_path).unwrap_or(false) {
			generate_self_signed_cert(&cert_path, &key_path, &[])?;
		}

		load_tls_config(&cert_path, &key_path)
	}
}

/// Parses a PEM-encoded certificate file and returns the DER-encoded certificates.
fn parse_pem_certs(pem_data: &str) -> Result<Vec<CertificateDer<'static>>, String> {
	let mut certs = Vec::new();

	for block in pem_data.split(PEM_CERT_END) {
		if let Some(start) = block.find(PEM_CERT_BEGIN) {
			let base64_content: String = block[start + PEM_CERT_BEGIN.len()..]
				.lines()
				.filter(|line| !line.starts_with("-----") && !line.is_empty())
				.collect();

			let der = base64::engine::general_purpose::STANDARD
				.decode(&base64_content)
				.map_err(|e| format!("Failed to decode certificate base64: {e}"))?;

			certs.push(CertificateDer::from(der));
		}
	}

	Ok(certs)
}

/// Parses a PEM-encoded PKCS#8 private key file and returns the DER-encoded key.
fn parse_pem_private_key(pem_data: &str) -> Result<PrivateKeyDer<'static>, String> {
	let start = pem_data.find(PEM_KEY_BEGIN).ok_or("Missing BEGIN PRIVATE KEY marker")?;
	let end = pem_data.find(PEM_KEY_END).ok_or("Missing END PRIVATE KEY marker")?;

	let base64_content: String = pem_data[start + PEM_KEY_BEGIN.len()..end]
		.lines()
		.filter(|line| !line.starts_with("-----") && !line.is_empty())
		.collect();

	let der = base64::engine::general_purpose::STANDARD
		.decode(&base64_content)
		.map_err(|e| format!("Failed to decode private key base64: {e}"))?;

	Ok(PrivateKeyDer::Pkcs8(der.into()))
}

/// Generates a self-signed TLS certificate and saves it to the storage directory.
/// Returns the paths to the generated cert and key files.
fn generate_self_signed_cert(
	cert_path: &str, key_path: &str, configure_hosts: &[String],
) -> Result<(), String> {
	let mut hosts = vec!["localhost".to_string(), "127.0.0.1".to_string()];
	hosts.extend_from_slice(configure_hosts);

	let CertifiedKey { cert, key_pair } = generate_simple_self_signed(hosts)
		.map_err(|e| format!("Failed to generate self-signed certificate: {e}"))?;

	// Convert DER to PEM format
	let cert_der = cert.der();
	let key_der = key_pair.serialize_der();

	let cert_pem = format!(
		"{PEM_CERT_BEGIN}\n{}\n{PEM_CERT_END}\n",
		base64::engine::general_purpose::STANDARD
			.encode(cert_der)
			.as_bytes()
			.chunks(64)
			.map(|chunk| std::str::from_utf8(chunk).unwrap())
			.collect::<Vec<_>>()
			.join("\n")
	);

	let key_pem = format!(
		"{PEM_KEY_BEGIN}\n{}\n{PEM_KEY_END}\n",
		base64::engine::general_purpose::STANDARD
			.encode(&key_der)
			.as_bytes()
			.chunks(64)
			.map(|chunk| std::str::from_utf8(chunk).unwrap())
			.collect::<Vec<_>>()
			.join("\n")
	);

	fs::write(cert_path, &cert_pem)
		.map_err(|e| format!("Failed to write TLS certificate to '{cert_path}': {e}"))?;
	fs::write(key_path, &key_pem)
		.map_err(|e| format!("Failed to write TLS key to '{key_path}': {e}"))?;

	Ok(())
}

/// Loads TLS configuration from provided paths.
fn load_tls_config(cert_path: &str, key_path: &str) -> Result<ServerConfig, String> {
	let cert_pem = fs::read_to_string(cert_path)
		.map_err(|e| format!("Failed to read TLS certificate file '{cert_path}': {e}"))?;
	let key_pem = fs::read_to_string(key_path)
		.map_err(|e| format!("Failed to read TLS key file '{key_path}': {e}"))?;

	let certs = parse_pem_certs(&cert_pem)?;

	if certs.is_empty() {
		return Err("No certificates found in certificate file".to_string());
	}

	let key = parse_pem_private_key(&key_pem)?;

	ServerConfig::builder()
		.with_no_client_auth()
		.with_single_cert(certs, key)
		.map_err(|e| format!("Failed to build TLS server config: {e}"))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_parse_pem_certs() {
		let pem = "-----BEGIN CERTIFICATE-----\nMIIBkTCB+wIJAKHBfpegPjMCMA0GCSqGSIb3DQEBCwUAMBExDzANBgNVBAMMBnVu\ndXNlZDAeFw0yMzAxMDEwMDAwMDBaFw0yNDAxMDEwMDAwMDBaMBExDzANBgNVBAMM\nBnVudXNlZDBcMA0GCSqGSIb3DQEBAQUAA0sAMEgCQQC7o96FCEcJsggt0c0dSfEB\nmm6vv1LdCoxXnhOSCutoJgJgmCPBjU1doFFKwAtXjfOv0eSLZ3NHLu0LRKmVvOsP\nAgMBAAGjUzBRMB0GA1UdDgQWBBQK3fc0myO0psd71FJd8v7VCmDJOzAfBgNVHSME\nGDAWgBQK3fc0myO0psd71FJd8v7VCmDJOzAPBgNVHRMBAf8EBTADAQH/MA0GCSqG\nSIb3DQEBCwUAA0EAhJg0cx2pFfVfGBfbJQNFa+A4ynJBMqKYlbUnJBfWPwg13RhC\nivLjYyhKzEbnOug0TuFfVaUBGfBYbPgaJQ4BAg==\n-----END CERTIFICATE-----\n";

		let certs = parse_pem_certs(pem).unwrap();
		assert_eq!(certs.len(), 1);
		assert!(!certs[0].is_empty());
	}

	#[test]
	fn test_parse_pem_certs_multiple() {
		let pem = "-----BEGIN CERTIFICATE-----\nMIIBkTCB+wIJAKHBfpegPjMCMA0GCSqGSIb3DQEBCwUAMBExDzANBgNVBAMMBnVu\ndXNlZDAeFw0yMzAxMDEwMDAwMDBaFw0yNDAxMDEwMDAwMDBaMBExDzANBgNVBAMM\nBnVudXNlZDBcMA0GCSqGSIb3DQEBAQUAA0sAMEgCQQC7o96FCEcJsggt0c0dSfEB\nmm6vv1LdCoxXnhOSCutoJgJgmCPBjU1doFFKwAtXjfOv0eSLZ3NHLu0LRKmVvOsP\nAgMBAAGjUzBRMB0GA1UdDgQWBBQK3fc0myO0psd71FJd8v7VCmDJOzAfBgNVHSME\nGDAWgBQK3fc0myO0psd71FJd8v7VCmDJOzAPBgNVHRMBAf8EBTADAQH/MA0GCSqG\nSIb3DQEBCwUAA0EAhJg0cx2pFfVfGBfbJQNFa+A4ynJBMqKYlbUnJBfWPwg13RhC\nivLjYyhKzEbnOug0TuFfVaUBGfBYbPgaJQ4BAg==\n-----END CERTIFICATE-----\n-----BEGIN CERTIFICATE-----\nMIIBkTCB+wIJAKHBfpegPjMCMA0GCSqGSIb3DQEBCwUAMBExDzANBgNVBAMMBnVu\ndXNlZDAeFw0yMzAxMDEwMDAwMDBaFw0yNDAxMDEwMDAwMDBaMBExDzANBgNVBAMM\nBnVudXNlZDBcMA0GCSqGSIb3DQEBAQUAA0sAMEgCQQC7o96FCEcJsggt0c0dSfEB\nmm6vv1LdCoxXnhOSCutoJgJgmCPBjU1doFFKwAtXjfOv0eSLZ3NHLu0LRKmVvOsP\nAgMBAAGjUzBRMB0GA1UdDgQWBBQK3fc0myO0psd71FJd8v7VCmDJOzAfBgNVHSME\nGDAWgBQK3fc0myO0psd71FJd8v7VCmDJOzAPBgNVHRMBAf8EBTADAQH/MA0GCSqG\nSIb3DQEBCwUAA0EAhJg0cx2pFfVfGBfbJQNFa+A4ynJBMqKYlbUnJBfWPwg13RhC\nivLjYyhKzEbnOug0TuFfVaUBGfBYbPgaJQ4BAg==\n-----END CERTIFICATE-----\n";

		let certs = parse_pem_certs(pem).unwrap();
		assert_eq!(certs.len(), 2);
	}

	#[test]
	fn test_parse_pem_certs_empty() {
		let certs = parse_pem_certs("").unwrap();
		assert!(certs.is_empty());

		let certs = parse_pem_certs("not a cert").unwrap();
		assert!(certs.is_empty());
	}

	#[test]
	fn test_parse_pem_private_key_pkcs8() {
		let pem = "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg2a2rwplBQLzHPDvn\nsaw8HKDP6WYBSF684gcz+D7zeVShRANCAAQq8R/E45tTNWMEpK8abYM7VzuJxpPS\nhJCi6bzjOPGHawEO8safLOWFaV7GqLJM0OdM3eu/qcz8HwgI3T8EVHQK\n-----END PRIVATE KEY-----\n";

		let key = parse_pem_private_key(pem).unwrap();
		assert!(matches!(key, PrivateKeyDer::Pkcs8(_)));
	}

	#[test]
	fn test_parse_pem_private_key_invalid() {
		let result = parse_pem_private_key("");
		assert!(result.is_err());

		let result = parse_pem_private_key("not a key");
		assert!(result.is_err());
	}

	#[test]
	fn test_generate_and_load_roundtrip() {
		let temp_dir = std::env::temp_dir();
		let suffix: u64 = rand::random();
		let cert_path = temp_dir.join(format!("test_tls_cert_{suffix}.pem"));
		let key_path = temp_dir.join(format!("test_tls_key_{suffix}.pem"));

		// Clean up any existing files to be safe
		let _ = fs::remove_file(&cert_path);
		let _ = fs::remove_file(&key_path);

		// Generate cert
		generate_self_signed_cert(cert_path.to_str().unwrap(), key_path.to_str().unwrap(), &[])
			.unwrap();

		// Verify files exist
		assert!(cert_path.exists());
		assert!(key_path.exists());

		// Load config
		let res = load_tls_config(cert_path.to_str().unwrap(), key_path.to_str().unwrap());
		assert!(res.is_ok());

		// Clean up
		let _ = fs::remove_file(&cert_path);
		let _ = fs::remove_file(&key_path);
	}
}
