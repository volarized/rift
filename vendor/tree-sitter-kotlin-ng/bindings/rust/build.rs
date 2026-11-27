use std::env;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;

const PARSER_BYTES: u64 = 29_199_164;

fn main() -> io::Result<()> {
    let source = Path::new("src");
    let archive = source.join("parser.c.gz");
    let scanner = source.join("scanner.c");
    let header = source.join("tree_sitter/parser.h");
    for path in [&archive, &scanner, &header] {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let output = env::var_os("OUT_DIR").expect("Cargo build output directory");
    let parser = PathBuf::from(output).join("parser.c");
    let mut input = GzDecoder::new(File::open(archive)?).take(PARSER_BYTES + 1);
    if io::copy(&mut input, &mut File::create(&parser)?)? != PARSER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Kotlin parser size differs",
        ));
    }

    let mut compiler = cc::Build::new();
    compiler
        .std("c11")
        .include(source)
        .flag_if_supported("-Wno-unused-parameter")
        .file(parser)
        .file(scanner);
    if env::var_os("CARGO_CFG_TARGET_ENV").is_some_and(|target| target == "msvc") {
        compiler.flag("-utf-8");
    }
    compiler.compile("tree-sitter-kotlin");
    Ok(())
}
