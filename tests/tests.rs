extern crate clap;
extern crate diff;
extern crate bindgen;
extern crate shlex;

use bindgen::Builder;
use std::fs;
use std::io::{BufRead, BufReader, Error, ErrorKind, Read, Write};
use std::path::PathBuf;

#[path="../src/options.rs"]
mod options;
use options::builder_from_flags;

fn compare_generated_header(header: &PathBuf,
                            builder: Builder)
                            -> Result<(), Error> {
    let file_name = try!(header.file_name()
        .ok_or(Error::new(ErrorKind::Other, "spawn_bindgen expects a file")));

    let mut expected = PathBuf::from(header);
    expected.pop();
    expected.pop();
    expected.push("expectations");
    expected.push("tests");
    expected.push(file_name);
    expected.set_extension("rs");

    // We skip the generate() error here so we get a full diff below
    let output = match builder.generate() {
        Ok(bindings) => bindings.to_string(),
        Err(_) => "".to_string(),
    };

    let mut buffer = String::new();
    {
        if let Ok(expected_file) = fs::File::open(&expected) {
            try!(BufReader::new(expected_file).read_to_string(&mut buffer));
        }
    }

    if output == buffer {
        if !output.is_empty() {
            return Ok(());
        }
        return Err(Error::new(ErrorKind::Other,
                              "Something's gone really wrong!"));
    }

    println!("diff expected generated");
    println!("--- expected: {:?}", expected);
    println!("+++ generated from: {:?}", header);

    for diff in diff::lines(&buffer, &output) {
        match diff {
            diff::Result::Left(l) => println!("-{}", l),
            diff::Result::Both(l, _) => println!(" {}", l),
            diff::Result::Right(r) => println!("+{}", r),
        }
    }

    // Override the diff.
    {
        let mut expected_file = try!(fs::File::create(&expected));
        try!(expected_file.write_all(output.as_bytes()));
    }

    Err(Error::new(ErrorKind::Other, "Header and binding differ!"))
}

fn create_bindgen_builder(header: &PathBuf) -> Result<Option<Builder>, Error> {
    let source = try!(fs::File::open(header));
    let reader = BufReader::new(source);

    // Scoop up bindgen-flags from test header
    let mut flags = Vec::with_capacity(2);

    for line in reader.lines().take(3) {
        let line = try!(line);
        if line.contains("bindgen-flags: ") {
            let extra_flags = line.split("bindgen-flags: ")
                .last()
                .and_then(shlex::split)
                .unwrap();
            flags.extend(extra_flags.into_iter());
        } else if line.contains("bindgen-unstable") &&
                  cfg!(feature = "llvm_stable") {
            return Ok(None);
        } else if line.contains("bindgen-osx-only") {
            let prepend_flags = ["--raw-line", "#![cfg(target_os=\"macos\")]"];
            flags = prepend_flags.into_iter()
                .map(ToString::to_string)
                .chain(flags)
                .collect();
        }
    }

    // Fool builder_from_flags() into believing it has real env::args_os...
    // - add "bindgen" as executable name 0th element
    // - add header filename as 1st element
    // - prepend raw lines so they're in the right order for expected output
    // - append the test header's bindgen flags
    let header_str = try!(header.to_str()
        .ok_or(Error::new(ErrorKind::Other, "Invalid header file name")));

    let prepend = ["bindgen",
                   header_str,
                   "--raw-line",
                   "",
                   "--raw-line",
                   "#![allow(non_snake_case)]",
                   "--raw-line",
                   ""];

    let args = prepend.into_iter()
        .map(ToString::to_string)
        .chain(flags.into_iter());

    builder_from_flags(args)
        .map(|(builder, _, _)| Some(builder.no_unstable_rust()))
}

macro_rules! test_header {
    ($function:ident, $header:expr) => (
        #[test]
        fn $function() {
            let header = PathBuf::from($header);
            let result = create_bindgen_builder(&header)
                .and_then(|builder| {
                    if let Some(builder) = builder {
                        compare_generated_header(&header, builder)
                    } else {
                        Ok(())
                    }
                });

            if let Err(err) = result {
                panic!("{}", err);
            }
        }
    )
}

// This file is generated by build.rs
include!(concat!(env!("OUT_DIR"), "/tests.rs"));
