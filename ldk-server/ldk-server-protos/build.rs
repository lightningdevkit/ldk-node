// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

#[cfg(genproto)]
extern crate prost_build;

#[cfg(genproto)]
use std::{env, fs, path::Path};

/// To generate updated proto objects, run `RUSTFLAGS="--cfg genproto" cargo build`
fn main() {
	#[cfg(genproto)]
	generate_protos();
}

#[cfg(genproto)]
fn generate_protos() {
	prost_build::Config::new()
		.bytes(&["."])
		.type_attribute(
			".",
			"#[cfg_attr(feature = \"serde\", derive(serde::Serialize, serde::Deserialize))]",
		)
		.type_attribute(".", "#[cfg_attr(feature = \"serde\", serde(rename_all = \"snake_case\"))]")
		.compile_protos(
			&[
				"src/proto/api.proto",
				"src/proto/types.proto",
				"src/proto/events.proto",
				"src/proto/error.proto",
			],
			&["src/proto/"],
		)
		.expect("protobuf compilation failed");
	println!("OUT_DIR: {}", &env::var("OUT_DIR").unwrap());
	let from_path = Path::new(&env::var("OUT_DIR").unwrap()).join("api.rs");
	fs::copy(from_path, "src/api.rs").unwrap();
	let from_path = Path::new(&env::var("OUT_DIR").unwrap()).join("types.rs");
	fs::copy(from_path, "src/types.rs").unwrap();
	let from_path = Path::new(&env::var("OUT_DIR").unwrap()).join("events.rs");
	fs::copy(from_path, "src/events.rs").unwrap();
	let from_path = Path::new(&env::var("OUT_DIR").unwrap()).join("error.rs");
	fs::copy(from_path, "src/error.rs").unwrap();
}
