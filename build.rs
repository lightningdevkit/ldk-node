fn main() {
	uniffi::generate_scaffolding("bindings/ldk_node.udl").unwrap();
}
