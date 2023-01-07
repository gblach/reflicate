fn main() {
	cc::Build::new()
		.file("src/reflink.c")
		.compile("reflink");
}
