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
		.compile_protos(
			&["src/proto/api.proto", "src/proto/types.proto", "src/proto/error.proto"],
			&["src/proto/"],
		)
		.expect("protobuf compilation failed");
	println!("OUT_DIR: {}", &env::var("OUT_DIR").unwrap());
	let from_path = Path::new(&env::var("OUT_DIR").unwrap()).join("api.rs");
	fs::copy(from_path, "src/api.rs").unwrap();
	let from_path = Path::new(&env::var("OUT_DIR").unwrap()).join("types.rs");
	fs::copy(from_path, "src/types.rs").unwrap();
	let from_path = Path::new(&env::var("OUT_DIR").unwrap()).join("error.rs");
	fs::copy(from_path, "src/error.rs").unwrap();
}
