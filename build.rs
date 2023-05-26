fn main() {
	#[cfg(feature = "uniffi")]
	uniffi::generate_scaffolding("bindings/ldk_node.udl").unwrap();
}
