//! Generate Rust bindings for C and C++ libraries.
//!
//! Provide a C/C++ header file, receive Rust FFI code to call into C/C++
//! functions and use types defined in the header.
//!
//! See the [`Builder`](./struct.Builder.html) struct for usage.
//!
//! See the [Users Guide](https://rust-lang.github.io/rust-bindgen/) for
//! additional documentation.
#![deny(missing_docs)]
#![deny(unused_extern_crates)]
#![deny(clippy::disallowed_methods)]
// To avoid rather annoying warnings when matching with CXCursor_xxx as a
// constant.
#![allow(non_upper_case_globals)]
// `quote!` nests quite deeply.
#![recursion_limit = "128"]

#[macro_use]
extern crate bitflags;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate quote;

#[cfg(feature = "logging")]
#[macro_use]
extern crate log;

#[cfg(not(feature = "logging"))]
#[macro_use]
mod log_stubs;

#[macro_use]
mod extra_assertions;

macro_rules! fn_with_regex_arg {
    ($(#[$attrs:meta])* pub fn $($tokens:tt)*) => {
        $(#[$attrs])*
        /// Check the [regular expression arguments] section and the [regex] crate
        /// documentation for further information.
        ///
        /// [regular expression arguments]: ./struct.Builder.html#regular-expression-arguments
        /// [regex]: <https://docs.rs/regex>
        pub fn $($tokens)*
    };
}

mod codegen;
mod deps;
mod time;

pub mod callbacks;

mod clang;
mod features;
mod ir;
mod parse;
mod regex_set;

use codegen::CodegenError;
use ir::comment;

pub use crate::codegen::{
    AliasVariation, EnumVariation, MacroTypeVariation, NonCopyUnionStyle,
};
use crate::features::RustFeatures;
pub use crate::features::{
    RustTarget, LATEST_STABLE_RUST, RUST_TARGET_STRINGS,
};
use crate::ir::context::{BindgenContext, ItemId};
pub use crate::ir::function::Abi;
use crate::ir::item::Item;
use crate::parse::ParseError;
pub use crate::regex_set::RegexSet;

use std::borrow::Cow;
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;

// Some convenient typedefs for a fast hash map and hash set.
type HashMap<K, V> = ::rustc_hash::FxHashMap<K, V>;
type HashSet<K> = ::rustc_hash::FxHashSet<K>;
pub(crate) use std::collections::hash_map::Entry;

/// Default prefix for the anon fields.
pub const DEFAULT_ANON_FIELDS_PREFIX: &str = "__bindgen_anon_";
const DEFAULT_NON_EXTERN_FNS_SUFFIX: &str = "__extern";

fn file_is_cpp(name_file: &str) -> bool {
    name_file.ends_with(".hpp") ||
        name_file.ends_with(".hxx") ||
        name_file.ends_with(".hh") ||
        name_file.ends_with(".h++")
}

fn args_are_cpp(clang_args: &[String]) -> bool {
    for w in clang_args.windows(2) {
        if w[0] == "-xc++" || w[1] == "-xc++" {
            return true;
        }
        if w[0] == "-x" && w[1] == "c++" {
            return true;
        }
        if w[0] == "-include" && file_is_cpp(&w[1]) {
            return true;
        }
    }
    false
}

bitflags! {
    /// A type used to indicate which kind of items we have to generate.
    pub struct CodegenConfig: u32 {
        /// Whether to generate functions.
        const FUNCTIONS = 1 << 0;
        /// Whether to generate types.
        const TYPES = 1 << 1;
        /// Whether to generate constants.
        const VARS = 1 << 2;
        /// Whether to generate methods.
        const METHODS = 1 << 3;
        /// Whether to generate constructors
        const CONSTRUCTORS = 1 << 4;
        /// Whether to generate destructors.
        const DESTRUCTORS = 1 << 5;
    }
}

impl CodegenConfig {
    /// Returns true if functions should be generated.
    pub fn functions(self) -> bool {
        self.contains(CodegenConfig::FUNCTIONS)
    }

    /// Returns true if types should be generated.
    pub fn types(self) -> bool {
        self.contains(CodegenConfig::TYPES)
    }

    /// Returns true if constants should be generated.
    pub fn vars(self) -> bool {
        self.contains(CodegenConfig::VARS)
    }

    /// Returns true if methds should be generated.
    pub fn methods(self) -> bool {
        self.contains(CodegenConfig::METHODS)
    }

    /// Returns true if constructors should be generated.
    pub fn constructors(self) -> bool {
        self.contains(CodegenConfig::CONSTRUCTORS)
    }

    /// Returns true if destructors should be generated.
    pub fn destructors(self) -> bool {
        self.contains(CodegenConfig::DESTRUCTORS)
    }
}

impl Default for CodegenConfig {
    fn default() -> Self {
        CodegenConfig::all()
    }
}

/// Configure and generate Rust bindings for a C/C++ header.
///
/// This is the main entry point to the library.
///
/// ```ignore
/// use bindgen::builder;
///
/// // Configure and generate bindings.
/// let bindings = builder().header("path/to/input/header")
///     .allowlist_type("SomeCoolClass")
///     .allowlist_function("do_some_cool_thing")
///     .generate()?;
///
/// // Write the generated bindings to an output file.
/// bindings.write_to_file("path/to/output.rs")?;
/// ```
///
/// # Enums
///
/// Bindgen can map C/C++ enums into Rust in different ways. The way bindgen maps enums depends on
/// the pattern passed to several methods:
///
/// 1. [`constified_enum_module()`](#method.constified_enum_module)
/// 2. [`bitfield_enum()`](#method.bitfield_enum)
/// 3. [`newtype_enum()`](#method.newtype_enum)
/// 4. [`rustified_enum()`](#method.rustified_enum)
///
/// For each C enum, bindgen tries to match the pattern in the following order:
///
/// 1. Constified enum module
/// 2. Bitfield enum
/// 3. Newtype enum
/// 4. Rustified enum
///
/// If none of the above patterns match, then bindgen will generate a set of Rust constants.
///
/// # Clang arguments
///
/// Extra arguments can be passed to with clang:
/// 1. [`clang_arg()`](#method.clang_arg): takes a single argument
/// 2. [`clang_args()`](#method.clang_args): takes an iterator of arguments
/// 3. `BINDGEN_EXTRA_CLANG_ARGS` environment variable: whitespace separate
///    environment variable of arguments
///
/// Clang arguments specific to your crate should be added via the
/// `clang_arg()`/`clang_args()` methods.
///
/// End-users of the crate may need to set the `BINDGEN_EXTRA_CLANG_ARGS` environment variable to
/// add additional arguments. For example, to build against a different sysroot a user could set
/// `BINDGEN_EXTRA_CLANG_ARGS` to `--sysroot=/path/to/sysroot`.
///
/// # Regular expression arguments
///
/// Some [`Builder`] methods like the `allowlist_*` and `blocklist_*` family of methods allow
/// regular expressions as arguments. These regular expressions will be parenthesized and wrapped
/// in `^` and `$`. So if `<regex>` is passed as argument, the regular expression to be stored will
/// be `^(<regex>)$`.
///
/// Releases of `bindgen` with a version lesser or equal to `0.62.0` used to accept the wildcard
/// pattern `*` as a valid regular expression. This behavior has been deprecated and the `.*`
/// pattern must be used instead.
#[derive(Debug, Default, Clone)]
pub struct Builder {
    options: BindgenOptions,
}

/// Construct a new [`Builder`](./struct.Builder.html).
pub fn builder() -> Builder {
    Default::default()
}

fn get_extra_clang_args() -> Vec<String> {
    // Add any extra arguments from the environment to the clang command line.
    let extra_clang_args =
        match get_target_dependent_env_var("BINDGEN_EXTRA_CLANG_ARGS") {
            None => return vec![],
            Some(s) => s,
        };
    // Try to parse it with shell quoting. If we fail, make it one single big argument.
    if let Some(strings) = shlex::split(&extra_clang_args) {
        return strings;
    }
    vec![extra_clang_args]
}

impl Builder {
    /// Generates the command line flags use for creating `Builder`.
    pub fn command_line_flags(&self) -> Vec<String> {
        let mut output_vector: Vec<String> = Vec::new();

        if let Some(header) = self.options.input_headers.last().cloned() {
            // Positional argument 'header'
            output_vector.push(header);
        }

        output_vector.push("--rust-target".into());
        output_vector.push(self.options.rust_target.into());

        // FIXME(emilio): This is a bit hacky, maybe we should stop re-using the
        // RustFeatures to store the "disable_untagged_union" call, and make it
        // a different flag that we check elsewhere / in generate().
        if !self.options.rust_features.untagged_union &&
            RustFeatures::from(self.options.rust_target).untagged_union
        {
            output_vector.push("--disable-untagged-union".into());
        }

        if self.options.default_enum_style != Default::default() {
            output_vector.push("--default-enum-style".into());
            output_vector.push(
                match self.options.default_enum_style {
                    codegen::EnumVariation::Rust {
                        non_exhaustive: false,
                    } => "rust",
                    codegen::EnumVariation::Rust {
                        non_exhaustive: true,
                    } => "rust_non_exhaustive",
                    codegen::EnumVariation::NewType {
                        is_bitfield: true,
                        ..
                    } => "bitfield",
                    codegen::EnumVariation::NewType {
                        is_bitfield: false,
                        is_global,
                    } => {
                        if is_global {
                            "newtype_global"
                        } else {
                            "newtype"
                        }
                    }
                    codegen::EnumVariation::Consts => "consts",
                    codegen::EnumVariation::ModuleConsts => "moduleconsts",
                }
                .into(),
            )
        }

        if self.options.default_macro_constant_type != Default::default() {
            output_vector.push("--default-macro-constant-type".into());
            output_vector
                .push(self.options.default_macro_constant_type.as_str().into());
        }

        if self.options.default_alias_style != Default::default() {
            output_vector.push("--default-alias-style".into());
            output_vector
                .push(self.options.default_alias_style.as_str().into());
        }

        if self.options.default_non_copy_union_style != Default::default() {
            output_vector.push("--default-non-copy-union-style".into());
            output_vector.push(
                self.options.default_non_copy_union_style.as_str().into(),
            );
        }

        let regex_sets = &[
            (&self.options.bitfield_enums, "--bitfield-enum"),
            (&self.options.newtype_enums, "--newtype-enum"),
            (&self.options.newtype_global_enums, "--newtype-global-enum"),
            (&self.options.rustified_enums, "--rustified-enum"),
            (
                &self.options.rustified_non_exhaustive_enums,
                "--rustified-enum-non-exhaustive",
            ),
            (
                &self.options.constified_enum_modules,
                "--constified-enum-module",
            ),
            (&self.options.constified_enums, "--constified-enum"),
            (&self.options.type_alias, "--type-alias"),
            (&self.options.new_type_alias, "--new-type-alias"),
            (&self.options.new_type_alias_deref, "--new-type-alias-deref"),
            (
                &self.options.bindgen_wrapper_union,
                "--bindgen-wrapper-union",
            ),
            (&self.options.manually_drop_union, "--manually-drop-union"),
            (&self.options.blocklisted_types, "--blocklist-type"),
            (&self.options.blocklisted_functions, "--blocklist-function"),
            (&self.options.blocklisted_items, "--blocklist-item"),
            (&self.options.blocklisted_files, "--blocklist-file"),
            (&self.options.opaque_types, "--opaque-type"),
            (&self.options.allowlisted_functions, "--allowlist-function"),
            (&self.options.allowlisted_types, "--allowlist-type"),
            (&self.options.allowlisted_vars, "--allowlist-var"),
            (&self.options.allowlisted_files, "--allowlist-file"),
            (&self.options.no_partialeq_types, "--no-partialeq"),
            (&self.options.no_copy_types, "--no-copy"),
            (&self.options.no_debug_types, "--no-debug"),
            (&self.options.no_default_types, "--no-default"),
            (&self.options.no_hash_types, "--no-hash"),
            (&self.options.must_use_types, "--must-use-type"),
        ];

        for (set, flag) in regex_sets {
            for item in set.get_items() {
                output_vector.push((*flag).to_owned());
                output_vector.push(item.to_owned());
            }
        }

        for (abi, set) in &self.options.abi_overrides {
            for item in set.get_items() {
                output_vector.push("--override-abi".to_owned());
                output_vector.push(format!("{}={}", item, abi));
            }
        }

        if !self.options.layout_tests {
            output_vector.push("--no-layout-tests".into());
        }

        if self.options.impl_debug {
            output_vector.push("--impl-debug".into());
        }

        if self.options.impl_partialeq {
            output_vector.push("--impl-partialeq".into());
        }

        if !self.options.derive_copy {
            output_vector.push("--no-derive-copy".into());
        }

        if !self.options.derive_debug {
            output_vector.push("--no-derive-debug".into());
        }

        if !self.options.derive_default {
            output_vector.push("--no-derive-default".into());
        } else {
            output_vector.push("--with-derive-default".into());
        }

        if self.options.derive_hash {
            output_vector.push("--with-derive-hash".into());
        }

        if self.options.derive_partialord {
            output_vector.push("--with-derive-partialord".into());
        }

        if self.options.derive_ord {
            output_vector.push("--with-derive-ord".into());
        }

        if self.options.derive_partialeq {
            output_vector.push("--with-derive-partialeq".into());
        }

        if self.options.derive_eq {
            output_vector.push("--with-derive-eq".into());
        }

        if self.options.time_phases {
            output_vector.push("--time-phases".into());
        }

        if !self.options.generate_comments {
            output_vector.push("--no-doc-comments".into());
        }

        if !self.options.allowlist_recursively {
            output_vector.push("--no-recursive-allowlist".into());
        }

        if self.options.objc_extern_crate {
            output_vector.push("--objc-extern-crate".into());
        }

        if self.options.generate_block {
            output_vector.push("--generate-block".into());
        }

        if self.options.block_extern_crate {
            output_vector.push("--block-extern-crate".into());
        }

        if self.options.builtins {
            output_vector.push("--builtins".into());
        }

        if let Some(ref prefix) = self.options.ctypes_prefix {
            output_vector.push("--ctypes-prefix".into());
            output_vector.push(prefix.clone());
        }

        if self.options.anon_fields_prefix != DEFAULT_ANON_FIELDS_PREFIX {
            output_vector.push("--anon-fields-prefix".into());
            output_vector.push(self.options.anon_fields_prefix.clone());
        }

        if self.options.emit_ast {
            output_vector.push("--emit-clang-ast".into());
        }

        if self.options.emit_ir {
            output_vector.push("--emit-ir".into());
        }
        if let Some(ref graph) = self.options.emit_ir_graphviz {
            output_vector.push("--emit-ir-graphviz".into());
            output_vector.push(graph.clone())
        }
        if self.options.enable_cxx_namespaces {
            output_vector.push("--enable-cxx-namespaces".into());
        }
        if self.options.enable_function_attribute_detection {
            output_vector.push("--enable-function-attribute-detection".into());
        }
        if self.options.disable_name_namespacing {
            output_vector.push("--disable-name-namespacing".into());
        }
        if self.options.disable_nested_struct_naming {
            output_vector.push("--disable-nested-struct-naming".into());
        }

        if self.options.disable_header_comment {
            output_vector.push("--disable-header-comment".into());
        }

        if !self.options.codegen_config.functions() {
            output_vector.push("--ignore-functions".into());
        }

        output_vector.push("--generate".into());

        //Temporary placeholder for below 4 options
        let mut options: Vec<String> = Vec::new();
        if self.options.codegen_config.functions() {
            options.push("functions".into());
        }
        if self.options.codegen_config.types() {
            options.push("types".into());
        }
        if self.options.codegen_config.vars() {
            options.push("vars".into());
        }
        if self.options.codegen_config.methods() {
            options.push("methods".into());
        }
        if self.options.codegen_config.constructors() {
            options.push("constructors".into());
        }
        if self.options.codegen_config.destructors() {
            options.push("destructors".into());
        }

        output_vector.push(options.join(","));

        if !self.options.codegen_config.methods() {
            output_vector.push("--ignore-methods".into());
        }

        if !self.options.convert_floats {
            output_vector.push("--no-convert-floats".into());
        }

        if !self.options.prepend_enum_name {
            output_vector.push("--no-prepend-enum-name".into());
        }

        if self.options.fit_macro_constants {
            output_vector.push("--fit-macro-constant-types".into());
        }

        if self.options.array_pointers_in_arguments {
            output_vector.push("--use-array-pointers-in-arguments".into());
        }

        if let Some(ref wasm_import_module_name) =
            self.options.wasm_import_module_name
        {
            output_vector.push("--wasm-import-module-name".into());
            output_vector.push(wasm_import_module_name.clone());
        }

        for line in &self.options.raw_lines {
            output_vector.push("--raw-line".into());
            output_vector.push(line.clone());
        }

        for (module, lines) in &self.options.module_lines {
            for line in lines.iter() {
                output_vector.push("--module-raw-line".into());
                output_vector.push(module.clone());
                output_vector.push(line.clone());
            }
        }

        if self.options.use_core {
            output_vector.push("--use-core".into());
        }

        if self.options.conservative_inline_namespaces {
            output_vector.push("--conservative-inline-namespaces".into());
        }

        if self.options.generate_inline_functions {
            output_vector.push("--generate-inline-functions".into());
        }

        if !self.options.record_matches {
            output_vector.push("--no-record-matches".into());
        }

        if !self.options.size_t_is_usize {
            output_vector.push("--no-size_t-is-usize".into());
        }

        if !self.options.rustfmt_bindings {
            output_vector.push("--no-rustfmt-bindings".into());
        }

        if let Some(path) = self
            .options
            .rustfmt_configuration_file
            .as_ref()
            .and_then(|f| f.to_str())
        {
            output_vector.push("--rustfmt-configuration-file".into());
            output_vector.push(path.into());
        }

        if let Some(ref name) = self.options.dynamic_library_name {
            output_vector.push("--dynamic-loading".into());
            output_vector.push(name.clone());
        }

        if self.options.dynamic_link_require_all {
            output_vector.push("--dynamic-link-require-all".into());
        }

        if self.options.respect_cxx_access_specs {
            output_vector.push("--respect-cxx-access-specs".into());
        }

        if self.options.translate_enum_integer_types {
            output_vector.push("--translate-enum-integer-types".into());
        }

        if self.options.c_naming {
            output_vector.push("--c-naming".into());
        }

        if self.options.force_explicit_padding {
            output_vector.push("--explicit-padding".into());
        }

        if self.options.vtable_generation {
            output_vector.push("--vtable-generation".into());
        }

        if self.options.sort_semantically {
            output_vector.push("--sort-semantically".into());
        }

        if self.options.merge_extern_blocks {
            output_vector.push("--merge-extern-blocks".into());
        }

        if self.options.wrap_unsafe_ops {
            output_vector.push("--wrap-unsafe-ops".into());
        }

        #[cfg(feature = "cli")]
        for callbacks in &self.options.parse_callbacks {
            output_vector.extend(callbacks.cli_args());
        }
        if self.options.wrap_static_fns {
            output_vector.push("--wrap-static-fns".into())
        }

        if let Some(ref path) = self.options.wrap_static_fns_path {
            output_vector.push("--wrap-static-fns-path".into());
            output_vector.push(path.display().to_string());
        }

        if let Some(ref suffix) = self.options.wrap_static_fns_suffix {
            output_vector.push("--wrap-static-fns-suffix".into());
            output_vector.push(suffix.clone());
        }

        if cfg!(feature = "experimental") {
            output_vector.push("--experimental".into());
        }

        // Add clang arguments

        output_vector.push("--".into());

        if !self.options.clang_args.is_empty() {
            output_vector.extend(self.options.clang_args.iter().cloned());
        }

        // To pass more than one header, we need to pass all but the last
        // header via the `-include` clang arg
        for header in &self.options.input_headers
            [..self.options.input_headers.len().saturating_sub(1)]
        {
            output_vector.push("-include".to_string());
            output_vector.push(header.clone());
        }

        output_vector
    }

    /// Add an input C/C++ header to generate bindings for.
    ///
    /// This can be used to generate bindings to a single header:
    ///
    /// ```ignore
    /// let bindings = bindgen::Builder::default()
    ///     .header("input.h")
    ///     .generate()
    ///     .unwrap();
    /// ```
    ///
    /// Or you can invoke it multiple times to generate bindings to multiple
    /// headers:
    ///
    /// ```ignore
    /// let bindings = bindgen::Builder::default()
    ///     .header("first.h")
    ///     .header("second.h")
    ///     .header("third.h")
    ///     .generate()
    ///     .unwrap();
    /// ```
    pub fn header<T: Into<String>>(mut self, header: T) -> Builder {
        self.options.input_headers.push(header.into());
        self
    }

    /// Add a depfile output which will be written alongside the generated bindings.
    pub fn depfile<H: Into<String>, D: Into<PathBuf>>(
        mut self,
        output_module: H,
        depfile: D,
    ) -> Builder {
        self.options.depfile = Some(deps::DepfileSpec {
            output_module: output_module.into(),
            depfile_path: depfile.into(),
        });
        self
    }

    /// Add `contents` as an input C/C++ header named `name`.
    ///
    /// The file `name` will be added to the clang arguments.
    pub fn header_contents(mut self, name: &str, contents: &str) -> Builder {
        // Apparently clang relies on having virtual FS correspondent to
        // the real one, so we need absolute paths here
        let absolute_path = env::current_dir()
            .expect("Cannot retrieve current directory")
            .join(name)
            .to_str()
            .expect("Cannot convert current directory name to string")
            .to_owned();
        self.options
            .input_header_contents
            .push((absolute_path, contents.into()));
        self
    }

    /// Specify the rust target
    ///
    /// The default is the latest stable Rust version
    pub fn rust_target(mut self, rust_target: RustTarget) -> Self {
        #[allow(deprecated)]
        if rust_target <= RustTarget::Stable_1_30 {
            warn!(
                "The {} rust target is deprecated. If you have a good reason to use this target please report it at https://github.com/rust-lang/rust-bindgen/issues",
                String::from(rust_target)
            );
        }
        self.options.set_rust_target(rust_target);
        self
    }

    /// Disable support for native Rust unions, if supported.
    pub fn disable_untagged_union(mut self) -> Self {
        self.options.rust_features.untagged_union = false;
        self
    }

    /// Disable insertion of bindgen's version identifier into generated
    /// bindings.
    pub fn disable_header_comment(mut self) -> Self {
        self.options.disable_header_comment = true;
        self
    }

    /// Set the output graphviz file.
    pub fn emit_ir_graphviz<T: Into<String>>(mut self, path: T) -> Builder {
        let path = path.into();
        self.options.emit_ir_graphviz = Some(path);
        self
    }

    /// Whether the generated bindings should contain documentation comments
    /// (docstrings) or not. This is set to true by default.
    ///
    /// Note that clang by default excludes comments from system headers, pass
    /// `-fretain-comments-from-system-headers` as
    /// [`clang_arg`][Builder::clang_arg] to include them. It can also be told
    /// to process all comments (not just documentation ones) using the
    /// `-fparse-all-comments` flag. See [slides on clang comment parsing](
    /// https://llvm.org/devmtg/2012-11/Gribenko_CommentParsing.pdf) for
    /// background and examples.
    pub fn generate_comments(mut self, doit: bool) -> Self {
        self.options.generate_comments = doit;
        self
    }

    /// Whether to allowlist recursively or not. Defaults to true.
    ///
    /// Given that we have explicitly allowlisted the "initiate_dance_party"
    /// function in this C header:
    ///
    /// ```c
    /// typedef struct MoonBoots {
    ///     int bouncy_level;
    /// } MoonBoots;
    ///
    /// void initiate_dance_party(MoonBoots* boots);
    /// ```
    ///
    /// We would normally generate bindings to both the `initiate_dance_party`
    /// function and the `MoonBoots` struct that it transitively references. By
    /// configuring with `allowlist_recursively(false)`, `bindgen` will not emit
    /// bindings for anything except the explicitly allowlisted items, and there
    /// would be no emitted struct definition for `MoonBoots`. However, the
    /// `initiate_dance_party` function would still reference `MoonBoots`!
    ///
    /// **Disabling this feature will almost certainly cause `bindgen` to emit
    /// bindings that will not compile!** If you disable this feature, then it
    /// is *your* responsibility to provide definitions for every type that is
    /// referenced from an explicitly allowlisted item. One way to provide the
    /// definitions is by using the [`Builder::raw_line`](#method.raw_line)
    /// method, another would be to define them in Rust and then `include!(...)`
    /// the bindings immediately afterwards.
    pub fn allowlist_recursively(mut self, doit: bool) -> Self {
        self.options.allowlist_recursively = doit;
        self
    }

    /// Generate `#[macro_use] extern crate objc;` instead of `use objc;`
    /// in the prologue of the files generated from objective-c files
    pub fn objc_extern_crate(mut self, doit: bool) -> Self {
        self.options.objc_extern_crate = doit;
        self
    }

    /// Generate proper block signatures instead of void pointers.
    pub fn generate_block(mut self, doit: bool) -> Self {
        self.options.generate_block = doit;
        self
    }

    /// Generate `#[macro_use] extern crate block;` instead of `use block;`
    /// in the prologue of the files generated from apple block files
    pub fn block_extern_crate(mut self, doit: bool) -> Self {
        self.options.block_extern_crate = doit;
        self
    }

    /// Whether to use the clang-provided name mangling. This is true by default
    /// and probably needed for C++ features.
    ///
    /// However, some old libclang versions seem to return incorrect results in
    /// some cases for non-mangled functions, see [1], so we allow disabling it.
    ///
    /// [1]: https://github.com/rust-lang/rust-bindgen/issues/528
    pub fn trust_clang_mangling(mut self, doit: bool) -> Self {
        self.options.enable_mangling = doit;
        self
    }

    fn_with_regex_arg! {
        /// Hide the given type from the generated bindings. Regular expressions are
        /// supported.
        ///
        /// To blocklist types prefixed with "mylib" use `"mylib_.*"`.
        pub fn blocklist_type<T: AsRef<str>>(mut self, arg: T) -> Builder {
            self.options.blocklisted_types.insert(arg);
            self
        }
    }

    fn_with_regex_arg! {
        /// Hide the given function from the generated bindings. Regular expressions
        /// are supported.
        ///
        /// Methods can be blocklisted by prefixing the name of the type implementing
        /// them followed by an underscore. So if `Foo` has a method `bar`, it can
        /// be blocklisted as `Foo_bar`.
        ///
        /// To blocklist functions prefixed with "mylib" use `"mylib_.*"`.
        pub fn blocklist_function<T: AsRef<str>>(mut self, arg: T) -> Builder {
            self.options.blocklisted_functions.insert(arg);
            self
        }
    }

    fn_with_regex_arg! {
        /// Hide the given item from the generated bindings, regardless of
        /// whether it's a type, function, module, etc. Regular
        /// expressions are supported.
        ///
        /// To blocklist items prefixed with "mylib" use `"mylib_.*"`.
        pub fn blocklist_item<T: AsRef<str>>(mut self, arg: T) -> Builder {
            self.options.blocklisted_items.insert(arg);
            self
        }
    }

    fn_with_regex_arg! {
        /// Hide any contents of the given file from the generated bindings,
        /// regardless of whether it's a type, function, module etc.
        pub fn blocklist_file<T: AsRef<str>>(mut self, arg: T) -> Builder {
            self.options.blocklisted_files.insert(arg);
            self
        }
    }

    fn_with_regex_arg! {
        /// Treat the given type as opaque in the generated bindings. Regular
        /// expressions are supported.
        ///
        /// To change types prefixed with "mylib" into opaque, use `"mylib_.*"`.
        pub fn opaque_type<T: AsRef<str>>(mut self, arg: T) -> Builder {
            self.options.opaque_types.insert(arg);
            self
        }
    }

    fn_with_regex_arg! {
        /// Allowlist the given type so that it (and all types that it transitively
        /// refers to) appears in the generated bindings. Regular expressions are
        /// supported.
        ///
        /// To allowlist types prefixed with "mylib" use `"mylib_.*"`.
        pub fn allowlist_type<T: AsRef<str>>(mut self, arg: T) -> Builder {
            self.options.allowlisted_types.insert(arg);
            self
        }
    }

    fn_with_regex_arg! {
        /// Allowlist the given function so that it (and all types that it
        /// transitively refers to) appears in the generated bindings. Regular
        /// expressions are supported.
        ///
        /// Methods can be allowlisted by prefixing the name of the type
        /// implementing them followed by an underscore. So if `Foo` has a method
        /// `bar`, it can be allowlisted as `Foo_bar`.
        ///
        /// To allowlist functions prefixed with "mylib" use `"mylib_.*"`.
        pub fn allowlist_function<T: AsRef<str>>(mut self, arg: T) -> Builder {
            self.options.allowlisted_functions.insert(arg);
            self
        }
    }

    fn_with_regex_arg! {
        /// Allowlist the given variable so that it (and all types that it
        /// transitively refers to) appears in the generated bindings. Regular
        /// expressions are supported.
        ///
        /// To allowlist variables prefixed with "mylib" use `"mylib_.*"`.
        pub fn allowlist_var<T: AsRef<str>>(mut self, arg: T) -> Builder {
            self.options.allowlisted_vars.insert(arg);
            self
        }
    }

    fn_with_regex_arg! {
        /// Allowlist the given file so that its contents appear in the generated bindings.
        pub fn allowlist_file<T: AsRef<str>>(mut self, arg: T) -> Builder {
            self.options.allowlisted_files.insert(arg);
            self
        }
    }

    /// Set the default style of code to generate for enums
    pub fn default_enum_style(
        mut self,
        arg: codegen::EnumVariation,
    ) -> Builder {
        self.options.default_enum_style = arg;
        self
    }

    fn_with_regex_arg! {
        /// Mark the given enum (or set of enums, if using a pattern) as being
        /// bitfield-like. Regular expressions are supported.
        ///
        /// This makes bindgen generate a type that isn't a rust `enum`. Regular
        /// expressions are supported.
        ///
        /// This is similar to the newtype enum style, but with the bitwise
        /// operators implemented.
        pub fn bitfield_enum<T: AsRef<str>>(mut self, arg: T) -> Builder {
            self.options.bitfield_enums.insert(arg);
            self
        }
    }

    fn_with_regex_arg! {
        /// Mark the given enum (or set of enums, if using a pattern) as a newtype.
        /// Regular expressions are supported.
        ///
        /// This makes bindgen generate a type that isn't a Rust `enum`. Regular
        /// expressions are supported.
        pub fn newtype_enum<T: AsRef<str>>(mut self, arg: T) -> Builder {
            self.options.newtype_enums.insert(arg);
            self
        }
    }

    fn_with_regex_arg! {
        /// Mark the given enum (or set of enums, if using a pattern) as a newtype
        /// whose variants are exposed as global constants.
        ///
        /// Regular expressions are supported.
        ///
        /// This makes bindgen generate a type that isn't a Rust `enum`. Regular
        /// expressions are supported.
        pub fn newtype_global_enum<T: AsRef<str>>(mut self, arg: T) -> Builder {
            self.options.newtype_global_enums.insert(arg);
            self
        }
    }

    fn_with_regex_arg! {
        /// Mark the given enum (or set of enums, if using a pattern) as a Rust
        /// enum.
        ///
        /// This makes bindgen generate enums instead of constants. Regular
        /// expressions are supported.
        ///
        /// **Use this with caution**, creating this in unsafe code
        /// (including FFI) with an invalid value will invoke undefined behaviour.
        /// You may want to use the newtype enum style instead.
        pub fn rustified_enum<T: AsRef<str>>(mut self, arg: T) -> Builder {
            self.options.rustified_enums.insert(arg);
            self
        }
    }

    fn_with_regex_arg! {
        /// Mark the given enum (or set of enums, if using a pattern) as a Rust
        /// enum with the `#[non_exhaustive]` attribute.
        ///
        /// This makes bindgen generate enums instead of constants. Regular
        /// expressions are supported.
        ///
        /// **Use this with caution**, creating this in unsafe code
        /// (including FFI) with an invalid value will invoke undefined behaviour.
        /// You may want to use the newtype enum style instead.
        pub fn rustified_non_exhaustive_enum<T: AsRef<str>>(
            mut self,
            arg: T,
        ) -> Builder {
            self.options.rustified_non_exhaustive_enums.insert(arg);
            self
        }
    }

    fn_with_regex_arg! {
        /// Mark the given enum (or set of enums, if using a pattern) as a set of
        /// constants that are not to be put into a module.
        pub fn constified_enum<T: AsRef<str>>(mut self, arg: T) -> Builder {
            self.options.constified_enums.insert(arg);
            self
        }
    }

    fn_with_regex_arg! {
        /// Mark the given enum (or set of enums, if using a pattern) as a set of
        /// constants that should be put into a module.
        ///
        /// This makes bindgen generate modules containing constants instead of
        /// just constants. Regular expressions are supported.
        pub fn constified_enum_module<T: AsRef<str>>(mut self, arg: T) -> Builder {
            self.options.constified_enum_modules.insert(arg);
            self
        }
    }

    /// Set the default type for macro constants
    pub fn default_macro_constant_type(
        mut self,
        arg: codegen::MacroTypeVariation,
    ) -> Builder {
        self.options.default_macro_constant_type = arg;
        self
    }

    /// Set the default style of code to generate for typedefs
    pub fn default_alias_style(
        mut self,
        arg: codegen::AliasVariation,
    ) -> Builder {
        self.options.default_alias_style = arg;
        self
    }

    fn_with_regex_arg! {
        /// Mark the given typedef alias (or set of aliases, if using a pattern) to
        /// use regular Rust type aliasing.
        ///
        /// This is the default behavior and should be used if `default_alias_style`
        /// was set to NewType or NewTypeDeref and you want to override it for a
        /// set of typedefs.
        pub fn type_alias<T: AsRef<str>>(mut self, arg: T) -> Builder {
            self.options.type_alias.insert(arg);
            self
        }
    }

    fn_with_regex_arg! {
        /// Mark the given typedef alias (or set of aliases, if using a pattern) to
        /// be generated as a new type by having the aliased type be wrapped in a
        /// #[repr(transparent)] struct.
        ///
        /// Used to enforce stricter type checking.
        pub fn new_type_alias<T: AsRef<str>>(mut self, arg: T) -> Builder {
            self.options.new_type_alias.insert(arg);
            self
        }
    }

    fn_with_regex_arg! {
        /// Mark the given typedef alias (or set of aliases, if using a pattern) to
        /// be generated as a new type by having the aliased type be wrapped in a
        /// #[repr(transparent)] struct and also have an automatically generated
        /// impl's of `Deref` and `DerefMut` to their aliased type.
        pub fn new_type_alias_deref<T: AsRef<str>>(mut self, arg: T) -> Builder {
            self.options.new_type_alias_deref.insert(arg);
            self
        }
    }

    /// Set the default style of code to generate for unions with a non-Copy member.
    pub fn default_non_copy_union_style(
        mut self,
        arg: codegen::NonCopyUnionStyle,
    ) -> Self {
        self.options.default_non_copy_union_style = arg;
        self
    }

    fn_with_regex_arg! {
        /// Mark the given union (or set of union, if using a pattern) to use
        /// a bindgen-generated wrapper for its members if at least one is non-Copy.
        pub fn bindgen_wrapper_union<T: AsRef<str>>(mut self, arg: T) -> Self {
            self.options.bindgen_wrapper_union.insert(arg);
            self
        }
    }

    fn_with_regex_arg! {
        /// Mark the given union (or set of union, if using a pattern) to use
        /// [`::core::mem::ManuallyDrop`] for its members if at least one is non-Copy.
        ///
        /// Note: `ManuallyDrop` was stabilized in Rust 1.20.0, do not use it if your
        /// MSRV is lower.
        pub fn manually_drop_union<T: AsRef<str>>(mut self, arg: T) -> Self {
            self.options.manually_drop_union.insert(arg);
            self
        }
    }

    fn_with_regex_arg! {
        /// Add a string to prepend to the generated bindings. The string is passed
        /// through without any modification.
        pub fn raw_line<T: Into<String>>(mut self, arg: T) -> Self {
            self.options.raw_lines.push(arg.into());
            self
        }
    }

    /// Add a given line to the beginning of module `mod`.
    pub fn module_raw_line<T, U>(mut self, mod_: T, line: U) -> Self
    where
        T: Into<String>,
        U: Into<String>,
    {
        self.options
            .module_lines
            .entry(mod_.into())
            .or_insert_with(Vec::new)
            .push(line.into());
        self
    }

    /// Add a given set of lines to the beginning of module `mod`.
    pub fn module_raw_lines<T, I>(mut self, mod_: T, lines: I) -> Self
    where
        T: Into<String>,
        I: IntoIterator,
        I::Item: Into<String>,
    {
        self.options
            .module_lines
            .entry(mod_.into())
            .or_insert_with(Vec::new)
            .extend(lines.into_iter().map(Into::into));
        self
    }

    /// Add an argument to be passed straight through to clang.
    pub fn clang_arg<T: Into<String>>(mut self, arg: T) -> Builder {
        self.options.clang_args.push(arg.into());
        self
    }

    /// Add arguments to be passed straight through to clang.
    pub fn clang_args<I>(mut self, iter: I) -> Builder
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        for arg in iter {
            self = self.clang_arg(arg.as_ref())
        }
        self
    }

    /// Emit bindings for builtin definitions (for example `__builtin_va_list`)
    /// in the generated Rust.
    pub fn emit_builtins(mut self) -> Builder {
        self.options.builtins = true;
        self
    }

    /// Avoid converting floats to `f32`/`f64` by default.
    pub fn no_convert_floats(mut self) -> Self {
        self.options.convert_floats = false;
        self
    }

    /// Set whether layout tests should be generated.
    pub fn layout_tests(mut self, doit: bool) -> Self {
        self.options.layout_tests = doit;
        self
    }

    /// Set whether `Debug` should be implemented, if it can not be derived automatically.
    pub fn impl_debug(mut self, doit: bool) -> Self {
        self.options.impl_debug = doit;
        self
    }

    /// Set whether `PartialEq` should be implemented, if it can not be derived automatically.
    pub fn impl_partialeq(mut self, doit: bool) -> Self {
        self.options.impl_partialeq = doit;
        self
    }

    /// Set whether `Copy` should be derived by default.
    pub fn derive_copy(mut self, doit: bool) -> Self {
        self.options.derive_copy = doit;
        self
    }

    /// Set whether `Debug` should be derived by default.
    pub fn derive_debug(mut self, doit: bool) -> Self {
        self.options.derive_debug = doit;
        self
    }

    /// Set whether `Default` should be derived by default.
    pub fn derive_default(mut self, doit: bool) -> Self {
        self.options.derive_default = doit;
        self
    }

    /// Set whether `Hash` should be derived by default.
    pub fn derive_hash(mut self, doit: bool) -> Self {
        self.options.derive_hash = doit;
        self
    }

    /// Set whether `PartialOrd` should be derived by default.
    /// If we don't compute partialord, we also cannot compute
    /// ord. Set the derive_ord to `false` when doit is `false`.
    pub fn derive_partialord(mut self, doit: bool) -> Self {
        self.options.derive_partialord = doit;
        if !doit {
            self.options.derive_ord = false;
        }
        self
    }

    /// Set whether `Ord` should be derived by default.
    /// We can't compute `Ord` without computing `PartialOrd`,
    /// so we set the same option to derive_partialord.
    pub fn derive_ord(mut self, doit: bool) -> Self {
        self.options.derive_ord = doit;
        self.options.derive_partialord = doit;
        self
    }

    /// Set whether `PartialEq` should be derived by default.
    ///
    /// If we don't derive `PartialEq`, we also cannot derive `Eq`, so deriving
    /// `Eq` is also disabled when `doit` is `false`.
    pub fn derive_partialeq(mut self, doit: bool) -> Self {
        self.options.derive_partialeq = doit;
        if !doit {
            self.options.derive_eq = false;
        }
        self
    }

    /// Set whether `Eq` should be derived by default.
    ///
    /// We can't derive `Eq` without also deriving `PartialEq`, so we also
    /// enable deriving `PartialEq` when `doit` is `true`.
    pub fn derive_eq(mut self, doit: bool) -> Self {
        self.options.derive_eq = doit;
        if doit {
            self.options.derive_partialeq = doit;
        }
        self
    }

    /// Set whether or not to time bindgen phases, and print information to
    /// stderr.
    pub fn time_phases(mut self, doit: bool) -> Self {
        self.options.time_phases = doit;
        self
    }

    /// Emit Clang AST.
    pub fn emit_clang_ast(mut self) -> Builder {
        self.options.emit_ast = true;
        self
    }

    /// Emit IR.
    pub fn emit_ir(mut self) -> Builder {
        self.options.emit_ir = true;
        self
    }

    /// Enable C++ namespaces.
    pub fn enable_cxx_namespaces(mut self) -> Builder {
        self.options.enable_cxx_namespaces = true;
        self
    }

    /// Enable detecting must_use attributes on C functions.
    ///
    /// This is quite slow in some cases (see #1465), so it's disabled by
    /// default.
    ///
    /// Note that for this to do something meaningful for now at least, the rust
    /// target version has to have support for `#[must_use]`.
    pub fn enable_function_attribute_detection(mut self) -> Self {
        self.options.enable_function_attribute_detection = true;
        self
    }

    /// Disable name auto-namespacing.
    ///
    /// By default, bindgen mangles names like `foo::bar::Baz` to look like
    /// `foo_bar_Baz` instead of just `Baz`.
    ///
    /// This method disables that behavior.
    ///
    /// Note that this intentionally does not change the names used for
    /// allowlisting and blocklisting, which should still be mangled with the
    /// namespaces.
    ///
    /// Note, also, that this option may cause bindgen to generate duplicate
    /// names.
    pub fn disable_name_namespacing(mut self) -> Builder {
        self.options.disable_name_namespacing = true;
        self
    }

    /// Disable nested struct naming.
    ///
    /// The following structs have different names for C and C++. In case of C
    /// they are visible as `foo` and `bar`. In case of C++ they are visible as
    /// `foo` and `foo::bar`.
    ///
    /// ```c
    /// struct foo {
    ///     struct bar {
    ///     } b;
    /// };
    /// ```
    ///
    /// Bindgen wants to avoid duplicate names by default so it follows C++ naming
    /// and it generates `foo`/`foo_bar` instead of just `foo`/`bar`.
    ///
    /// This method disables this behavior and it is indented to be used only
    /// for headers that were written for C.
    pub fn disable_nested_struct_naming(mut self) -> Builder {
        self.options.disable_nested_struct_naming = true;
        self
    }

    /// Treat inline namespaces conservatively.
    ///
    /// This is tricky, because in C++ is technically legal to override an item
    /// defined in an inline namespace:
    ///
    /// ```cpp
    /// inline namespace foo {
    ///     using Bar = int;
    /// }
    /// using Bar = long;
    /// ```
    ///
    /// Even though referencing `Bar` is a compiler error.
    ///
    /// We want to support this (arguably esoteric) use case, but we don't want
    /// to make the rest of bindgen users pay an usability penalty for that.
    ///
    /// To support this, we need to keep all the inline namespaces around, but
    /// then bindgen usage is a bit more difficult, because you cannot
    /// reference, e.g., `std::string` (you'd need to use the proper inline
    /// namespace).
    ///
    /// We could complicate a lot of the logic to detect name collisions, and if
    /// not detected generate a `pub use inline_ns::*` or something like that.
    ///
    /// That's probably something we can do if we see this option is needed in a
    /// lot of cases, to improve it's usability, but my guess is that this is
    /// not going to be too useful.
    pub fn conservative_inline_namespaces(mut self) -> Builder {
        self.options.conservative_inline_namespaces = true;
        self
    }

    /// Whether inline functions should be generated or not.
    ///
    /// Note that they will usually not work. However you can use
    /// `-fkeep-inline-functions` or `-fno-inline-functions` if you are
    /// responsible of compiling the library to make them callable.
    pub fn generate_inline_functions(mut self, doit: bool) -> Self {
        self.options.generate_inline_functions = doit;
        self
    }

    /// Ignore functions.
    pub fn ignore_functions(mut self) -> Builder {
        self.options.codegen_config.remove(CodegenConfig::FUNCTIONS);
        self
    }

    /// Ignore methods.
    pub fn ignore_methods(mut self) -> Builder {
        self.options.codegen_config.remove(CodegenConfig::METHODS);
        self
    }

    /// Use core instead of libstd in the generated bindings.
    pub fn use_core(mut self) -> Builder {
        self.options.use_core = true;
        self
    }

    /// Use the given prefix for the raw types instead of `::std::os::raw`.
    pub fn ctypes_prefix<T: Into<String>>(mut self, prefix: T) -> Builder {
        self.options.ctypes_prefix = Some(prefix.into());
        self
    }

    /// Use the given prefix for the anon fields.
    pub fn anon_fields_prefix<T: Into<String>>(mut self, prefix: T) -> Builder {
        self.options.anon_fields_prefix = prefix.into();
        self
    }

    /// Allows configuring types in different situations, see the
    /// [`ParseCallbacks`](./callbacks/trait.ParseCallbacks.html) documentation.
    pub fn parse_callbacks(
        mut self,
        cb: Box<dyn callbacks::ParseCallbacks>,
    ) -> Self {
        self.options.parse_callbacks.push(Rc::from(cb));
        self
    }

    /// Choose what to generate using a
    /// [`CodegenConfig`](./struct.CodegenConfig.html).
    pub fn with_codegen_config(mut self, config: CodegenConfig) -> Self {
        self.options.codegen_config = config;
        self
    }

    /// Whether to detect include paths using clang_sys.
    pub fn detect_include_paths(mut self, doit: bool) -> Self {
        self.options.detect_include_paths = doit;
        self
    }

    /// Whether to try to fit macro constants to types smaller than u32/i32
    pub fn fit_macro_constants(mut self, doit: bool) -> Self {
        self.options.fit_macro_constants = doit;
        self
    }

    /// Prepend the enum name to constant or newtype variants.
    pub fn prepend_enum_name(mut self, doit: bool) -> Self {
        self.options.prepend_enum_name = doit;
        self
    }

    /// Set whether `size_t` should be translated to `usize` automatically.
    pub fn size_t_is_usize(mut self, is: bool) -> Self {
        self.options.size_t_is_usize = is;
        self
    }

    /// Set whether rustfmt should format the generated bindings.
    pub fn rustfmt_bindings(mut self, doit: bool) -> Self {
        self.options.rustfmt_bindings = doit;
        self
    }

    /// Set whether we should record matched items in our regex sets.
    pub fn record_matches(mut self, doit: bool) -> Self {
        self.options.record_matches = doit;
        self
    }

    /// Set the absolute path to the rustfmt configuration file, if None, the standard rustfmt
    /// options are used.
    pub fn rustfmt_configuration_file(mut self, path: Option<PathBuf>) -> Self {
        self = self.rustfmt_bindings(true);
        self.options.rustfmt_configuration_file = path;
        self
    }

    /// Sets an explicit path to rustfmt, to be used when rustfmt is enabled.
    pub fn with_rustfmt<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.options.rustfmt_path = Some(path.into());
        self
    }

    /// If true, always emit explicit padding fields.
    ///
    /// If a struct needs to be serialized in its native format (padding bytes
    /// and all), for example writing it to a file or sending it on the network,
    /// then this should be enabled, as anything reading the padding bytes of
    /// a struct may lead to Undefined Behavior.
    pub fn explicit_padding(mut self, doit: bool) -> Self {
        self.options.force_explicit_padding = doit;
        self
    }

    /// If true, enables experimental support to generate vtable functions.
    ///
    /// Should mostly work, though some edge cases are likely to be broken.
    pub fn vtable_generation(mut self, doit: bool) -> Self {
        self.options.vtable_generation = doit;
        self
    }

    /// If true, enables the sorting of the output in a predefined manner.
    ///
    /// TODO: Perhaps move the sorting order out into a config
    pub fn sort_semantically(mut self, doit: bool) -> Self {
        self.options.sort_semantically = doit;
        self
    }

    /// If true, merges extern blocks.
    pub fn merge_extern_blocks(mut self, doit: bool) -> Self {
        self.options.merge_extern_blocks = doit;
        self
    }

    /// Generate the Rust bindings using the options built up thus far.
    pub fn generate(mut self) -> Result<Bindings, BindgenError> {
        // Add any extra arguments from the environment to the clang command line.
        self.options.clang_args.extend(get_extra_clang_args());

        // Transform input headers to arguments on the clang command line.
        self.options.clang_args.extend(
            self.options.input_headers
                [..self.options.input_headers.len().saturating_sub(1)]
                .iter()
                .flat_map(|header| ["-include".into(), header.to_string()]),
        );

        let input_unsaved_files =
            std::mem::take(&mut self.options.input_header_contents)
                .into_iter()
                .map(|(name, contents)| clang::UnsavedFile::new(name, contents))
                .collect::<Vec<_>>();

        Bindings::generate(self.options, input_unsaved_files)
    }

    /// Preprocess and dump the input header files to disk.
    ///
    /// This is useful when debugging bindgen, using C-Reduce, or when filing
    /// issues. The resulting file will be named something like `__bindgen.i` or
    /// `__bindgen.ii`
    pub fn dump_preprocessed_input(&self) -> io::Result<()> {
        let clang =
            clang_sys::support::Clang::find(None, &[]).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Other,
                    "Cannot find clang executable",
                )
            })?;

        // The contents of a wrapper file that includes all the input header
        // files.
        let mut wrapper_contents = String::new();

        // Whether we are working with C or C++ inputs.
        let mut is_cpp = args_are_cpp(&self.options.clang_args);

        // For each input header, add `#include "$header"`.
        for header in &self.options.input_headers {
            is_cpp |= file_is_cpp(header);

            wrapper_contents.push_str("#include \"");
            wrapper_contents.push_str(header);
            wrapper_contents.push_str("\"\n");
        }

        // For each input header content, add a prefix line of `#line 0 "$name"`
        // followed by the contents.
        for (name, contents) in &self.options.input_header_contents {
            is_cpp |= file_is_cpp(name);

            wrapper_contents.push_str("#line 0 \"");
            wrapper_contents.push_str(name);
            wrapper_contents.push_str("\"\n");
            wrapper_contents.push_str(contents);
        }

        let wrapper_path = PathBuf::from(if is_cpp {
            "__bindgen.cpp"
        } else {
            "__bindgen.c"
        });

        {
            let mut wrapper_file = File::create(&wrapper_path)?;
            wrapper_file.write_all(wrapper_contents.as_bytes())?;
        }

        let mut cmd = Command::new(clang.path);
        cmd.arg("-save-temps")
            .arg("-E")
            .arg("-C")
            .arg("-c")
            .arg(&wrapper_path)
            .stdout(Stdio::piped());

        for a in &self.options.clang_args {
            cmd.arg(a);
        }

        for a in get_extra_clang_args() {
            cmd.arg(a);
        }

        let mut child = cmd.spawn()?;

        let mut preprocessed = child.stdout.take().unwrap();
        let mut file = File::create(if is_cpp {
            "__bindgen.ii"
        } else {
            "__bindgen.i"
        })?;
        io::copy(&mut preprocessed, &mut file)?;

        if child.wait()?.success() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "clang exited with non-zero status",
            ))
        }
    }

    fn_with_regex_arg! {
        /// Don't derive `PartialEq` for a given type. Regular
        /// expressions are supported.
        pub fn no_partialeq<T: Into<String>>(mut self, arg: T) -> Builder {
            self.options.no_partialeq_types.insert(arg.into());
            self
        }
    }

    fn_with_regex_arg! {
        /// Don't derive `Copy` for a given type. Regular
        /// expressions are supported.
        pub fn no_copy<T: Into<String>>(mut self, arg: T) -> Self {
            self.options.no_copy_types.insert(arg.into());
            self
        }
    }

    fn_with_regex_arg! {
        /// Don't derive `Debug` for a given type. Regular
        /// expressions are supported.
        pub fn no_debug<T: Into<String>>(mut self, arg: T) -> Self {
            self.options.no_debug_types.insert(arg.into());
            self
        }
    }

    fn_with_regex_arg! {
        /// Don't derive/impl `Default` for a given type. Regular
        /// expressions are supported.
        pub fn no_default<T: Into<String>>(mut self, arg: T) -> Self {
            self.options.no_default_types.insert(arg.into());
            self
        }
    }

    fn_with_regex_arg! {
        /// Don't derive `Hash` for a given type. Regular
        /// expressions are supported.
        pub fn no_hash<T: Into<String>>(mut self, arg: T) -> Builder {
            self.options.no_hash_types.insert(arg.into());
            self
        }
    }

    fn_with_regex_arg! {
        /// Add `#[must_use]` for the given type. Regular
        /// expressions are supported.
        pub fn must_use_type<T: Into<String>>(mut self, arg: T) -> Builder {
            self.options.must_use_types.insert(arg.into());
            self
        }
    }

    /// Set whether `arr[size]` should be treated as `*mut T` or `*mut [T; size]` (same for mut)
    pub fn array_pointers_in_arguments(mut self, doit: bool) -> Self {
        self.options.array_pointers_in_arguments = doit;
        self
    }

    /// Set the wasm import module name
    pub fn wasm_import_module_name<T: Into<String>>(
        mut self,
        import_name: T,
    ) -> Self {
        self.options.wasm_import_module_name = Some(import_name.into());
        self
    }

    /// Specify the dynamic library name if we are generating bindings for a shared library.
    pub fn dynamic_library_name<T: Into<String>>(
        mut self,
        dynamic_library_name: T,
    ) -> Self {
        self.options.dynamic_library_name = Some(dynamic_library_name.into());
        self
    }

    /// Require successful linkage for all routines in a shared library.
    /// This allows us to optimize function calls by being able to safely assume function pointers
    /// are valid.
    pub fn dynamic_link_require_all(mut self, req: bool) -> Self {
        self.options.dynamic_link_require_all = req;
        self
    }

    /// Generate bindings as `pub` only if the bound item is publically accessible by C++.
    pub fn respect_cxx_access_specs(mut self, doit: bool) -> Self {
        self.options.respect_cxx_access_specs = doit;
        self
    }

    /// Always translate enum integer types to native Rust integer types.
    ///
    /// This will result in enums having types such as `u32` and `i16` instead
    /// of `c_uint` and `c_short`. Types for Rustified enums are always
    /// translated.
    pub fn translate_enum_integer_types(mut self, doit: bool) -> Self {
        self.options.translate_enum_integer_types = doit;
        self
    }

    /// Generate types with C style naming.
    ///
    /// This will add prefixes to the generated type names. For example instead of a struct `A` we
    /// will generate struct `struct_A`. Currently applies to structs, unions, and enums.
    pub fn c_naming(mut self, doit: bool) -> Self {
        self.options.c_naming = doit;
        self
    }

    /// Override the ABI of a given function. Regular expressions are supported.
    pub fn override_abi<T: Into<String>>(mut self, abi: Abi, arg: T) -> Self {
        self.options
            .abi_overrides
            .entry(abi)
            .or_default()
            .insert(arg.into());
        self
    }

    /// If true, wraps unsafe operations in unsafe blocks.
    pub fn wrap_unsafe_ops(mut self, doit: bool) -> Self {
        self.options.wrap_unsafe_ops = doit;
        self
    }

    #[cfg(feature = "experimental")]
    /// Whether to generate extern wrappers for `static` and `static inline` functions. Defaults to
    /// false.
    pub fn wrap_static_fns(mut self, doit: bool) -> Self {
        self.options.wrap_static_fns = doit;
        self
    }

    #[cfg(feature = "experimental")]
    /// Set the path for the source code file that would be created if any wrapper functions must
    /// be generated due to the presence of static functions.
    ///
    /// Bindgen will automatically add the right extension to the header and source code files.
    pub fn wrap_static_fns_path<T: AsRef<Path>>(mut self, path: T) -> Self {
        self.options.wrap_static_fns_path = Some(path.as_ref().to_owned());
        self
    }

    #[cfg(feature = "experimental")]
    /// Set the suffix added to the extern wrapper functions generated for `static` and `static
    /// inline` functions.
    pub fn wrap_static_fns_suffix<T: AsRef<str>>(mut self, suffix: T) -> Self {
        self.options.wrap_static_fns_suffix = Some(suffix.as_ref().to_owned());
        self
    }
}

/// Configuration options for generated bindings.
#[derive(Clone, Debug)]
struct BindgenOptions {
    /// The set of types that have been blocklisted and should not appear
    /// anywhere in the generated code.
    blocklisted_types: RegexSet,

    /// The set of functions that have been blocklisted and should not appear
    /// in the generated code.
    blocklisted_functions: RegexSet,

    /// The set of items, regardless of item-type, that have been
    /// blocklisted and should not appear in the generated code.
    blocklisted_items: RegexSet,

    /// The set of files whose contents should be blocklisted and should not
    /// appear in the generated code.
    blocklisted_files: RegexSet,

    /// The set of types that should be treated as opaque structures in the
    /// generated code.
    opaque_types: RegexSet,

    /// The explicit rustfmt path.
    rustfmt_path: Option<PathBuf>,

    /// The path to which we should write a Makefile-syntax depfile (if any).
    depfile: Option<deps::DepfileSpec>,

    /// The set of types that we should have bindings for in the generated
    /// code.
    ///
    /// This includes all types transitively reachable from any type in this
    /// set. One might think of allowlisted types/vars/functions as GC roots,
    /// and the generated Rust code as including everything that gets marked.
    allowlisted_types: RegexSet,

    /// Allowlisted functions. See docs for `allowlisted_types` for more.
    allowlisted_functions: RegexSet,

    /// Allowlisted variables. See docs for `allowlisted_types` for more.
    allowlisted_vars: RegexSet,

    /// The set of files whose contents should be allowlisted.
    allowlisted_files: RegexSet,

    /// The default style of code to generate for enums
    default_enum_style: codegen::EnumVariation,

    /// The enum patterns to mark an enum as a bitfield
    /// (newtype with bitwise operations).
    bitfield_enums: RegexSet,

    /// The enum patterns to mark an enum as a newtype.
    newtype_enums: RegexSet,

    /// The enum patterns to mark an enum as a global newtype.
    newtype_global_enums: RegexSet,

    /// The enum patterns to mark an enum as a Rust enum.
    rustified_enums: RegexSet,

    /// The enum patterns to mark an enum as a non-exhaustive Rust enum.
    rustified_non_exhaustive_enums: RegexSet,

    /// The enum patterns to mark an enum as a module of constants.
    constified_enum_modules: RegexSet,

    /// The enum patterns to mark an enum as a set of constants.
    constified_enums: RegexSet,

    /// The default type for C macro constants.
    default_macro_constant_type: codegen::MacroTypeVariation,

    /// The default style of code to generate for typedefs.
    default_alias_style: codegen::AliasVariation,

    /// Typedef patterns that will use regular type aliasing.
    type_alias: RegexSet,

    /// Typedef patterns that will be aliased by creating a new struct.
    new_type_alias: RegexSet,

    /// Typedef patterns that will be wrapped in a new struct and have
    /// Deref and Deref to their aliased type.
    new_type_alias_deref: RegexSet,

    /// The default style of code to generate for union containing non-Copy
    /// members.
    default_non_copy_union_style: codegen::NonCopyUnionStyle,

    /// The union patterns to mark an non-Copy union as using the bindgen
    /// generated wrapper.
    bindgen_wrapper_union: RegexSet,

    /// The union patterns to mark an non-Copy union as using the
    /// `::core::mem::ManuallyDrop` wrapper.
    manually_drop_union: RegexSet,

    /// Whether we should generate builtins or not.
    builtins: bool,

    /// True if we should dump the Clang AST for debugging purposes.
    emit_ast: bool,

    /// True if we should dump our internal IR for debugging purposes.
    emit_ir: bool,

    /// Output graphviz dot file.
    emit_ir_graphviz: Option<String>,

    /// True if we should emulate C++ namespaces with Rust modules in the
    /// generated bindings.
    enable_cxx_namespaces: bool,

    /// True if we should try to find unexposed attributes in functions, in
    /// order to be able to generate #[must_use] attributes in Rust.
    enable_function_attribute_detection: bool,

    /// True if we should avoid mangling names with namespaces.
    disable_name_namespacing: bool,

    /// True if we should avoid generating nested struct names.
    disable_nested_struct_naming: bool,

    /// True if we should avoid embedding version identifiers into source code.
    disable_header_comment: bool,

    /// True if we should generate layout tests for generated structures.
    layout_tests: bool,

    /// True if we should implement the Debug trait for C/C++ structures and types
    /// that do not support automatically deriving Debug.
    impl_debug: bool,

    /// True if we should implement the PartialEq trait for C/C++ structures and types
    /// that do not support automatically deriving PartialEq.
    impl_partialeq: bool,

    /// True if we should derive Copy trait implementations for C/C++ structures
    /// and types.
    derive_copy: bool,

    /// True if we should derive Debug trait implementations for C/C++ structures
    /// and types.
    derive_debug: bool,

    /// True if we should derive Default trait implementations for C/C++ structures
    /// and types.
    derive_default: bool,

    /// True if we should derive Hash trait implementations for C/C++ structures
    /// and types.
    derive_hash: bool,

    /// True if we should derive PartialOrd trait implementations for C/C++ structures
    /// and types.
    derive_partialord: bool,

    /// True if we should derive Ord trait implementations for C/C++ structures
    /// and types.
    derive_ord: bool,

    /// True if we should derive PartialEq trait implementations for C/C++ structures
    /// and types.
    derive_partialeq: bool,

    /// True if we should derive Eq trait implementations for C/C++ structures
    /// and types.
    derive_eq: bool,

    /// True if we should avoid using libstd to use libcore instead.
    use_core: bool,

    /// An optional prefix for the "raw" types, like `c_int`, `c_void`...
    ctypes_prefix: Option<String>,

    /// The prefix for the anon fields.
    anon_fields_prefix: String,

    /// Whether to time the bindgen phases.
    time_phases: bool,

    /// Whether we should convert float types to f32/f64 types.
    convert_floats: bool,

    /// The set of raw lines to prepend to the top-level module of generated
    /// Rust code.
    raw_lines: Vec<String>,

    /// The set of raw lines to prepend to each of the modules.
    ///
    /// This only makes sense if the `enable_cxx_namespaces` option is set.
    module_lines: HashMap<String, Vec<String>>,

    /// The set of arguments to pass straight through to Clang.
    clang_args: Vec<String>,

    /// The input header files.
    input_headers: Vec<String>,

    /// Tuples of unsaved file contents of the form (name, contents).
    input_header_contents: Vec<(String, String)>,

    /// A user-provided visitor to allow customizing different kinds of
    /// situations.
    parse_callbacks: Vec<Rc<dyn callbacks::ParseCallbacks>>,

    /// Which kind of items should we generate? By default, we'll generate all
    /// of them.
    codegen_config: CodegenConfig,

    /// Whether to treat inline namespaces conservatively.
    ///
    /// See the builder method description for more details.
    conservative_inline_namespaces: bool,

    /// Whether to keep documentation comments in the generated output. See the
    /// documentation for more details. Defaults to true.
    generate_comments: bool,

    /// Whether to generate inline functions. Defaults to false.
    generate_inline_functions: bool,

    /// Whether to allowlist types recursively. Defaults to true.
    allowlist_recursively: bool,

    /// Instead of emitting 'use objc;' to files generated from objective c files,
    /// generate '#[macro_use] extern crate objc;'
    objc_extern_crate: bool,

    /// Instead of emitting 'use block;' to files generated from objective c files,
    /// generate '#[macro_use] extern crate block;'
    generate_block: bool,

    /// Instead of emitting 'use block;' to files generated from objective c files,
    /// generate '#[macro_use] extern crate block;'
    block_extern_crate: bool,

    /// Whether to use the clang-provided name mangling. This is true and
    /// probably needed for C++ features.
    ///
    /// However, some old libclang versions seem to return incorrect results in
    /// some cases for non-mangled functions, see [1], so we allow disabling it.
    ///
    /// [1]: https://github.com/rust-lang/rust-bindgen/issues/528
    enable_mangling: bool,

    /// Whether to detect include paths using clang_sys.
    detect_include_paths: bool,

    /// Whether to try to fit macro constants into types smaller than u32/i32
    fit_macro_constants: bool,

    /// Whether to prepend the enum name to constant or newtype variants.
    prepend_enum_name: bool,

    /// Version of the Rust compiler to target
    rust_target: RustTarget,

    /// Features to enable, derived from `rust_target`
    rust_features: RustFeatures,

    /// Whether we should record which items in the regex sets ever matched.
    ///
    /// This may be a bit slower, but will enable reporting of unused allowlist
    /// items via the `error!` log.
    record_matches: bool,

    /// Whether `size_t` should be translated to `usize` automatically.
    size_t_is_usize: bool,

    /// Whether rustfmt should format the generated bindings.
    rustfmt_bindings: bool,

    /// The absolute path to the rustfmt configuration file, if None, the standard rustfmt
    /// options are used.
    rustfmt_configuration_file: Option<PathBuf>,

    /// The set of types that we should not derive `PartialEq` for.
    no_partialeq_types: RegexSet,

    /// The set of types that we should not derive `Copy` for.
    no_copy_types: RegexSet,

    /// The set of types that we should not derive `Debug` for.
    no_debug_types: RegexSet,

    /// The set of types that we should not derive/impl `Default` for.
    no_default_types: RegexSet,

    /// The set of types that we should not derive `Hash` for.
    no_hash_types: RegexSet,

    /// The set of types that we should be annotated with `#[must_use]`.
    must_use_types: RegexSet,

    /// Decide if C arrays should be regular pointers in rust or array pointers
    array_pointers_in_arguments: bool,

    /// Wasm import module name.
    wasm_import_module_name: Option<String>,

    /// The name of the dynamic library (if we are generating bindings for a shared library). If
    /// this is None, no dynamic bindings are created.
    dynamic_library_name: Option<String>,

    /// Require successful linkage for all routines in a shared library.
    /// This allows us to optimize function calls by being able to safely assume function pointers
    /// are valid. No effect if `dynamic_library_name` is None.
    dynamic_link_require_all: bool,

    /// Only make generated bindings `pub` if the items would be publically accessible
    /// by C++.
    respect_cxx_access_specs: bool,

    /// Always translate enum integer types to native Rust integer types.
    translate_enum_integer_types: bool,

    /// Generate types with C style naming.
    c_naming: bool,

    /// Always output explicit padding fields
    force_explicit_padding: bool,

    /// Emit vtable functions.
    vtable_generation: bool,

    /// Sort the code generation.
    sort_semantically: bool,

    /// Deduplicate `extern` blocks.
    merge_extern_blocks: bool,

    abi_overrides: HashMap<Abi, RegexSet>,

    /// Whether to wrap unsafe operations in unsafe blocks or not.
    wrap_unsafe_ops: bool,

    wrap_static_fns: bool,

    wrap_static_fns_suffix: Option<String>,

    wrap_static_fns_path: Option<PathBuf>,
}

impl BindgenOptions {
    fn build(&mut self) {
        let regex_sets = [
            &mut self.allowlisted_vars,
            &mut self.allowlisted_types,
            &mut self.allowlisted_functions,
            &mut self.allowlisted_files,
            &mut self.blocklisted_types,
            &mut self.blocklisted_functions,
            &mut self.blocklisted_items,
            &mut self.blocklisted_files,
            &mut self.opaque_types,
            &mut self.bitfield_enums,
            &mut self.constified_enums,
            &mut self.constified_enum_modules,
            &mut self.newtype_enums,
            &mut self.newtype_global_enums,
            &mut self.rustified_enums,
            &mut self.rustified_non_exhaustive_enums,
            &mut self.type_alias,
            &mut self.new_type_alias,
            &mut self.new_type_alias_deref,
            &mut self.bindgen_wrapper_union,
            &mut self.manually_drop_union,
            &mut self.no_partialeq_types,
            &mut self.no_copy_types,
            &mut self.no_debug_types,
            &mut self.no_default_types,
            &mut self.no_hash_types,
            &mut self.must_use_types,
        ];
        let record_matches = self.record_matches;
        for regex_set in self.abi_overrides.values_mut().chain(regex_sets) {
            regex_set.build(record_matches);
        }
    }

    /// Update rust target version
    pub fn set_rust_target(&mut self, rust_target: RustTarget) {
        self.rust_target = rust_target;

        // Keep rust_features synced with rust_target
        self.rust_features = rust_target.into();
    }

    /// Get features supported by target Rust version
    pub fn rust_features(&self) -> RustFeatures {
        self.rust_features
    }

    fn last_callback<T>(
        &self,
        f: impl Fn(&dyn callbacks::ParseCallbacks) -> Option<T>,
    ) -> Option<T> {
        self.parse_callbacks
            .iter()
            .filter_map(|cb| f(cb.as_ref()))
            .last()
    }

    fn all_callbacks<T>(
        &self,
        f: impl Fn(&dyn callbacks::ParseCallbacks) -> Vec<T>,
    ) -> Vec<T> {
        self.parse_callbacks
            .iter()
            .flat_map(|cb| f(cb.as_ref()))
            .collect()
    }

    fn process_comment(&self, comment: &str) -> String {
        let comment = comment::preprocess(comment);
        self.parse_callbacks
            .last()
            .and_then(|cb| cb.process_comment(&comment))
            .unwrap_or(comment)
    }
}

impl Default for BindgenOptions {
    fn default() -> BindgenOptions {
        macro_rules! options {
            ($($field:ident $(: $value:expr)?,)* --default-fields-- $($default_field:ident,)*) => {
                BindgenOptions {
                    $($field $(: $value)*,)*
                    $($default_field: Default::default(),)*
                }
            };
        }

        let rust_target = RustTarget::default();

        options! {
            rust_target,
            rust_features: rust_target.into(),
            layout_tests: true,
            derive_copy: true,
            derive_debug: true,
            anon_fields_prefix: DEFAULT_ANON_FIELDS_PREFIX.into(),
            convert_floats: true,
            codegen_config: CodegenConfig::all(),
            generate_comments: true,
            allowlist_recursively: true,
            enable_mangling: true,
            detect_include_paths: true,
            prepend_enum_name: true,
            record_matches: true,
            rustfmt_bindings: true,
            size_t_is_usize: true,

            --default-fields--
            blocklisted_types,
            blocklisted_functions,
            blocklisted_items,
            blocklisted_files,
            opaque_types,
            rustfmt_path,
            depfile,
            allowlisted_types,
            allowlisted_functions,
            allowlisted_vars,
            allowlisted_files,
            default_enum_style,
            bitfield_enums,
            newtype_enums,
            newtype_global_enums,
            rustified_enums,
            rustified_non_exhaustive_enums,
            constified_enums,
            constified_enum_modules,
            default_macro_constant_type,
            default_alias_style,
            type_alias,
            new_type_alias,
            new_type_alias_deref,
            default_non_copy_union_style,
            bindgen_wrapper_union,
            manually_drop_union,
            builtins,
            emit_ast,
            emit_ir,
            emit_ir_graphviz,
            impl_debug,
            impl_partialeq,
            derive_default,
            derive_hash,
            derive_partialord,
            derive_ord,
            derive_partialeq,
            derive_eq,
            enable_cxx_namespaces,
            enable_function_attribute_detection,
            disable_name_namespacing,
            disable_nested_struct_naming,
            disable_header_comment,
            use_core,
            ctypes_prefix,
            raw_lines,
            module_lines,
            clang_args,
            input_headers,
            input_header_contents,
            parse_callbacks,
            conservative_inline_namespaces,
            generate_inline_functions,
            generate_block,
            objc_extern_crate,
            block_extern_crate,
            fit_macro_constants,
            time_phases,
            rustfmt_configuration_file,
            no_partialeq_types,
            no_copy_types,
            no_debug_types,
            no_default_types,
            no_hash_types,
            must_use_types,
            array_pointers_in_arguments,
            wasm_import_module_name,
            dynamic_library_name,
            dynamic_link_require_all,
            respect_cxx_access_specs,
            translate_enum_integer_types,
            c_naming,
            force_explicit_padding,
            vtable_generation,
            sort_semantically,
            merge_extern_blocks,
            abi_overrides,
            wrap_unsafe_ops,
            wrap_static_fns,
            wrap_static_fns_suffix,
            wrap_static_fns_path,
        }
    }
}

#[cfg(feature = "runtime")]
fn ensure_libclang_is_loaded() {
    if clang_sys::is_loaded() {
        return;
    }

    // XXX (issue #350): Ensure that our dynamically loaded `libclang`
    // doesn't get dropped prematurely, nor is loaded multiple times
    // across different threads.

    lazy_static! {
        static ref LIBCLANG: std::sync::Arc<clang_sys::SharedLibrary> = {
            clang_sys::load().expect("Unable to find libclang");
            clang_sys::get_library().expect(
                "We just loaded libclang and it had better still be \
                 here!",
            )
        };
    }

    clang_sys::set_library(Some(LIBCLANG.clone()));
}

#[cfg(not(feature = "runtime"))]
fn ensure_libclang_is_loaded() {}

/// Error type for rust-bindgen.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BindgenError {
    /// The header was a folder.
    FolderAsHeader(PathBuf),
    /// Permissions to read the header is insufficient.
    InsufficientPermissions(PathBuf),
    /// The header does not exist.
    NotExist(PathBuf),
    /// Clang diagnosed an error.
    ClangDiagnostic(String),
    /// Code generation reported an error.
    Codegen(CodegenError),
}

impl std::fmt::Display for BindgenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindgenError::FolderAsHeader(h) => {
                write!(f, "'{}' is a folder", h.display())
            }
            BindgenError::InsufficientPermissions(h) => {
                write!(f, "insufficient permissions to read '{}'", h.display())
            }
            BindgenError::NotExist(h) => {
                write!(f, "header '{}' does not exist.", h.display())
            }
            BindgenError::ClangDiagnostic(message) => {
                write!(f, "clang diagnosed error: {}", message)
            }
            BindgenError::Codegen(err) => {
                write!(f, "codegen error: {}", err)
            }
        }
    }
}

impl std::error::Error for BindgenError {}

/// Generated Rust bindings.
#[derive(Debug)]
pub struct Bindings {
    options: BindgenOptions,
    warnings: Vec<String>,
    module: proc_macro2::TokenStream,
}

pub(crate) const HOST_TARGET: &str =
    include_str!(concat!(env!("OUT_DIR"), "/host-target.txt"));

// Some architecture triplets are different between rust and libclang, see #1211
// and duplicates.
fn rust_to_clang_target(rust_target: &str) -> String {
    if rust_target.starts_with("aarch64-apple-") {
        let mut clang_target = "arm64-apple-".to_owned();
        clang_target
            .push_str(rust_target.strip_prefix("aarch64-apple-").unwrap());
        return clang_target;
    } else if rust_target.starts_with("riscv64gc-") {
        let mut clang_target = "riscv64-".to_owned();
        clang_target.push_str(rust_target.strip_prefix("riscv64gc-").unwrap());
        return clang_target;
    } else if rust_target.ends_with("-espidf") {
        let mut clang_target =
            rust_target.strip_suffix("-espidf").unwrap().to_owned();
        clang_target.push_str("-elf");
        if clang_target.starts_with("riscv32imc-") {
            clang_target = "riscv32-".to_owned() +
                clang_target.strip_prefix("riscv32imc-").unwrap();
        }
        return clang_target;
    }
    rust_target.to_owned()
}

/// Returns the effective target, and whether it was explicitly specified on the
/// clang flags.
fn find_effective_target(clang_args: &[String]) -> (String, bool) {
    let mut args = clang_args.iter();
    while let Some(opt) = args.next() {
        if opt.starts_with("--target=") {
            let mut split = opt.split('=');
            split.next();
            return (split.next().unwrap().to_owned(), true);
        }

        if opt == "-target" {
            if let Some(target) = args.next() {
                return (target.clone(), true);
            }
        }
    }

    // If we're running from a build script, try to find the cargo target.
    if let Ok(t) = env::var("TARGET") {
        return (rust_to_clang_target(&t), false);
    }

    (rust_to_clang_target(HOST_TARGET), false)
}

impl Bindings {
    /// Generate bindings for the given options.
    pub(crate) fn generate(
        mut options: BindgenOptions,
        input_unsaved_files: Vec<clang::UnsavedFile>,
    ) -> Result<Bindings, BindgenError> {
        ensure_libclang_is_loaded();

        #[cfg(feature = "runtime")]
        debug!(
            "Generating bindings, libclang at {}",
            clang_sys::get_library().unwrap().path().display()
        );
        #[cfg(not(feature = "runtime"))]
        debug!("Generating bindings, libclang linked");

        options.build();

        let (effective_target, explicit_target) =
            find_effective_target(&options.clang_args);

        let is_host_build =
            rust_to_clang_target(HOST_TARGET) == effective_target;

        // NOTE: The is_host_build check wouldn't be sound normally in some
        // cases if we were to call a binary (if you have a 32-bit clang and are
        // building on a 64-bit system for example).  But since we rely on
        // opening libclang.so, it has to be the same architecture and thus the
        // check is fine.
        if !explicit_target && !is_host_build {
            options
                .clang_args
                .insert(0, format!("--target={}", effective_target));
        };

        fn detect_include_paths(options: &mut BindgenOptions) {
            if !options.detect_include_paths {
                return;
            }

            // Filter out include paths and similar stuff, so we don't incorrectly
            // promote them to `-isystem`.
            let clang_args_for_clang_sys = {
                let mut last_was_include_prefix = false;
                options
                    .clang_args
                    .iter()
                    .filter(|arg| {
                        if last_was_include_prefix {
                            last_was_include_prefix = false;
                            return false;
                        }

                        let arg = &**arg;

                        // https://clang.llvm.org/docs/ClangCommandLineReference.html
                        // -isystem and -isystem-after are harmless.
                        if arg == "-I" || arg == "--include-directory" {
                            last_was_include_prefix = true;
                            return false;
                        }

                        if arg.starts_with("-I") ||
                            arg.starts_with("--include-directory=")
                        {
                            return false;
                        }

                        true
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            };

            debug!(
                "Trying to find clang with flags: {:?}",
                clang_args_for_clang_sys
            );

            let clang = match clang_sys::support::Clang::find(
                None,
                &clang_args_for_clang_sys,
            ) {
                None => return,
                Some(clang) => clang,
            };

            debug!("Found clang: {:?}", clang);

            // Whether we are working with C or C++ inputs.
            let is_cpp = args_are_cpp(&options.clang_args) ||
                options.input_headers.iter().any(|h| file_is_cpp(h));

            let search_paths = if is_cpp {
                clang.cpp_search_paths
            } else {
                clang.c_search_paths
            };

            if let Some(search_paths) = search_paths {
                for path in search_paths.into_iter() {
                    if let Ok(path) = path.into_os_string().into_string() {
                        options.clang_args.push("-isystem".to_owned());
                        options.clang_args.push(path);
                    }
                }
            }
        }

        detect_include_paths(&mut options);

        #[cfg(unix)]
        fn can_read(perms: &std::fs::Permissions) -> bool {
            use std::os::unix::fs::PermissionsExt;
            perms.mode() & 0o444 > 0
        }

        #[cfg(not(unix))]
        fn can_read(_: &std::fs::Permissions) -> bool {
            true
        }

        if let Some(h) = options.input_headers.last() {
            let path = Path::new(h);
            if let Ok(md) = std::fs::metadata(path) {
                if md.is_dir() {
                    return Err(BindgenError::FolderAsHeader(path.into()));
                }
                if !can_read(&md.permissions()) {
                    return Err(BindgenError::InsufficientPermissions(
                        path.into(),
                    ));
                }
                let h = h.clone();
                options.clang_args.push(h);
            } else {
                return Err(BindgenError::NotExist(path.into()));
            }
        }

        for (idx, f) in input_unsaved_files.iter().enumerate() {
            if idx != 0 || !options.input_headers.is_empty() {
                options.clang_args.push("-include".to_owned());
            }
            options.clang_args.push(f.name.to_str().unwrap().to_owned())
        }

        debug!("Fixed-up options: {:?}", options);

        let time_phases = options.time_phases;
        let mut context = BindgenContext::new(options, &input_unsaved_files);

        if is_host_build {
            debug_assert_eq!(
                context.target_pointer_size(),
                std::mem::size_of::<*mut ()>(),
                "{:?} {:?}",
                effective_target,
                HOST_TARGET
            );
        }

        {
            let _t = time::Timer::new("parse").with_output(time_phases);
            parse(&mut context)?;
        }

        let (module, options, warnings) =
            codegen::codegen(context).map_err(BindgenError::Codegen)?;

        Ok(Bindings {
            options,
            warnings,
            module,
        })
    }

    /// Write these bindings as source text to a file.
    pub fn write_to_file<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        let file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(path.as_ref())?;
        self.write(Box::new(file))?;
        Ok(())
    }

    /// Write these bindings as source text to the given `Write`able.
    pub fn write<'a>(&self, mut writer: Box<dyn Write + 'a>) -> io::Result<()> {
        if !self.options.disable_header_comment {
            let version = option_env!("CARGO_PKG_VERSION");
            let header = format!(
                "/* automatically generated by rust-bindgen {} */\n\n",
                version.unwrap_or("(unknown version)")
            );
            writer.write_all(header.as_bytes())?;
        }

        for line in self.options.raw_lines.iter() {
            writer.write_all(line.as_bytes())?;
            writer.write_all("\n".as_bytes())?;
        }

        if !self.options.raw_lines.is_empty() {
            writer.write_all("\n".as_bytes())?;
        }

        let bindings = self.module.to_string();

        match self.rustfmt_generated_string(&bindings) {
            Ok(rustfmt_bindings) => {
                writer.write_all(rustfmt_bindings.as_bytes())?;
            }
            Err(err) => {
                eprintln!(
                    "Failed to run rustfmt: {} (non-fatal, continuing)",
                    err
                );
                writer.write_all(bindings.as_bytes())?;
            }
        }
        Ok(())
    }

    /// Gets the rustfmt path to rustfmt the generated bindings.
    fn rustfmt_path(&self) -> io::Result<Cow<PathBuf>> {
        debug_assert!(self.options.rustfmt_bindings);
        if let Some(ref p) = self.options.rustfmt_path {
            return Ok(Cow::Borrowed(p));
        }
        if let Ok(rustfmt) = env::var("RUSTFMT") {
            return Ok(Cow::Owned(rustfmt.into()));
        }
        #[cfg(feature = "which-rustfmt")]
        match which::which("rustfmt") {
            Ok(p) => Ok(Cow::Owned(p)),
            Err(e) => {
                Err(io::Error::new(io::ErrorKind::Other, format!("{}", e)))
            }
        }
        #[cfg(not(feature = "which-rustfmt"))]
        // No rustfmt binary was specified, so assume that the binary is called
        // "rustfmt" and that it is in the user's PATH.
        Ok(Cow::Owned("rustfmt".into()))
    }

    /// Checks if rustfmt_bindings is set and runs rustfmt on the string
    fn rustfmt_generated_string<'a>(
        &self,
        source: &'a str,
    ) -> io::Result<Cow<'a, str>> {
        let _t = time::Timer::new("rustfmt_generated_string")
            .with_output(self.options.time_phases);

        if !self.options.rustfmt_bindings {
            return Ok(Cow::Borrowed(source));
        }

        let rustfmt = self.rustfmt_path()?;
        let mut cmd = Command::new(&*rustfmt);

        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());

        if let Some(path) = self
            .options
            .rustfmt_configuration_file
            .as_ref()
            .and_then(|f| f.to_str())
        {
            cmd.args(["--config-path", path]);
        }

        let mut child = cmd.spawn()?;
        let mut child_stdin = child.stdin.take().unwrap();
        let mut child_stdout = child.stdout.take().unwrap();

        let source = source.to_owned();

        // Write to stdin in a new thread, so that we can read from stdout on this
        // thread. This keeps the child from blocking on writing to its stdout which
        // might block us from writing to its stdin.
        let stdin_handle = ::std::thread::spawn(move || {
            let _ = child_stdin.write_all(source.as_bytes());
            source
        });

        let mut output = vec![];
        io::copy(&mut child_stdout, &mut output)?;

        let status = child.wait()?;
        let source = stdin_handle.join().expect(
            "The thread writing to rustfmt's stdin doesn't do \
             anything that could panic",
        );

        match String::from_utf8(output) {
            Ok(bindings) => match status.code() {
                Some(0) => Ok(Cow::Owned(bindings)),
                Some(2) => Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Rustfmt parsing errors.".to_string(),
                )),
                Some(3) => {
                    warn!("Rustfmt could not format some lines.");
                    Ok(Cow::Owned(bindings))
                }
                _ => Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Internal rustfmt error".to_string(),
                )),
            },
            _ => Ok(Cow::Owned(source)),
        }
    }

    /// Emit all the warning messages raised while generating the bindings in a build script.
    ///
    /// If you are using `bindgen` outside of a build script you should use [`Bindings::warnings`]
    /// and handle the messages accordingly instead.
    #[inline]
    pub fn emit_warnings(&self) {
        for message in &self.warnings {
            println!("cargo:warning={}", message);
        }
    }

    /// Return all the warning messages raised while generating the bindings.
    #[inline]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

impl std::fmt::Display for Bindings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut bytes = vec![];
        self.write(Box::new(&mut bytes) as Box<dyn Write>)
            .expect("writing to a vec cannot fail");
        f.write_str(
            std::str::from_utf8(&bytes)
                .expect("we should only write bindings that are valid utf-8"),
        )
    }
}

/// Determines whether the given cursor is in any of the files matched by the
/// options.
fn filter_builtins(ctx: &BindgenContext, cursor: &clang::Cursor) -> bool {
    ctx.options().builtins || !cursor.is_builtin()
}

/// Parse one `Item` from the Clang cursor.
fn parse_one(
    ctx: &mut BindgenContext,
    cursor: clang::Cursor,
    parent: Option<ItemId>,
) -> clang_sys::CXChildVisitResult {
    if !filter_builtins(ctx, &cursor) {
        return CXChildVisit_Continue;
    }

    use clang_sys::CXChildVisit_Continue;
    match Item::parse(cursor, parent, ctx) {
        Ok(..) => {}
        Err(ParseError::Continue) => {}
        Err(ParseError::Recurse) => {
            cursor.visit(|child| parse_one(ctx, child, parent));
        }
    }
    CXChildVisit_Continue
}

/// Parse the Clang AST into our `Item` internal representation.
fn parse(context: &mut BindgenContext) -> Result<(), BindgenError> {
    use clang_sys::*;

    let mut error = None;
    for d in context.translation_unit().diags().iter() {
        let msg = d.format();
        let is_err = d.severity() >= CXDiagnostic_Error;
        if is_err {
            let error = error.get_or_insert_with(String::new);
            error.push_str(&msg);
            error.push('\n');
        } else {
            eprintln!("clang diag: {}", msg);
        }
    }

    if let Some(message) = error {
        return Err(BindgenError::ClangDiagnostic(message));
    }

    let cursor = context.translation_unit().cursor();

    if context.options().emit_ast {
        fn dump_if_not_builtin(cur: &clang::Cursor) -> CXChildVisitResult {
            if !cur.is_builtin() {
                clang::ast_dump(cur, 0)
            } else {
                CXChildVisit_Continue
            }
        }
        cursor.visit(|cur| dump_if_not_builtin(&cur));
    }

    let root = context.root_module();
    context.with_module(root, |context| {
        cursor.visit(|cursor| parse_one(context, cursor, None))
    });

    assert!(
        context.current_module() == context.root_module(),
        "How did this happen?"
    );
    Ok(())
}

/// Extracted Clang version data
#[derive(Debug)]
pub struct ClangVersion {
    /// Major and minor semver, if parsing was successful
    pub parsed: Option<(u32, u32)>,
    /// full version string
    pub full: String,
}

/// Get the major and the minor semver numbers of Clang's version
pub fn clang_version() -> ClangVersion {
    ensure_libclang_is_loaded();

    //Debian clang version 11.0.1-2
    let raw_v: String = clang::extract_clang_version();
    let split_v: Option<Vec<&str>> = raw_v
        .split_whitespace()
        .find(|t| t.chars().next().map_or(false, |v| v.is_ascii_digit()))
        .map(|v| v.split('.').collect());
    if let Some(v) = split_v {
        if v.len() >= 2 {
            let maybe_major = v[0].parse::<u32>();
            let maybe_minor = v[1].parse::<u32>();
            if let (Ok(major), Ok(minor)) = (maybe_major, maybe_minor) {
                return ClangVersion {
                    parsed: Some((major, minor)),
                    full: raw_v.clone(),
                };
            }
        }
    };
    ClangVersion {
        parsed: None,
        full: raw_v.clone(),
    }
}

/// Looks for the env var `var_${TARGET}`, and falls back to just `var` when it is not found.
fn get_target_dependent_env_var(var: &str) -> Option<String> {
    if let Ok(target) = env::var("TARGET") {
        if let Ok(v) = env::var(format!("{}_{}", var, target)) {
            return Some(v);
        }
        if let Ok(v) = env::var(format!("{}_{}", var, target.replace('-', "_")))
        {
            return Some(v);
        }
    }
    env::var(var).ok()
}

/// A ParseCallbacks implementation that will act on file includes by echoing a rerun-if-changed
/// line
///
/// When running inside a `build.rs` script, this can be used to make cargo invalidate the
/// generated bindings whenever any of the files included from the header change:
/// ```
/// use bindgen::builder;
/// let bindings = builder()
///     .header("path/to/input/header")
///     .parse_callbacks(Box::new(bindgen::CargoCallbacks))
///     .generate();
/// ```
#[derive(Debug)]
pub struct CargoCallbacks;

impl callbacks::ParseCallbacks for CargoCallbacks {
    fn include_file(&self, filename: &str) {
        println!("cargo:rerun-if-changed={}", filename);
    }
}

/// Test command_line_flag function.
#[test]
fn commandline_flag_unit_test_function() {
    //Test 1
    let bindings = crate::builder();
    let command_line_flags = bindings.command_line_flags();

    let test_cases = vec![
        "--rust-target",
        "--no-derive-default",
        "--generate",
        "functions,types,vars,methods,constructors,destructors",
    ]
    .iter()
    .map(|&x| x.into())
    .collect::<Vec<String>>();

    assert!(test_cases.iter().all(|x| command_line_flags.contains(x)));

    //Test 2
    let bindings = crate::builder()
        .header("input_header")
        .allowlist_type("Distinct_Type")
        .allowlist_function("safe_function");

    let command_line_flags = bindings.command_line_flags();
    let test_cases = vec![
        "--rust-target",
        "input_header",
        "--no-derive-default",
        "--generate",
        "functions,types,vars,methods,constructors,destructors",
        "--allowlist-type",
        "Distinct_Type",
        "--allowlist-function",
        "safe_function",
    ]
    .iter()
    .map(|&x| x.into())
    .collect::<Vec<String>>();
    println!("{:?}", command_line_flags);

    assert!(test_cases.iter().all(|x| command_line_flags.contains(x)));
}

#[test]
fn test_rust_to_clang_target() {
    assert_eq!(rust_to_clang_target("aarch64-apple-ios"), "arm64-apple-ios");
}

#[test]
fn test_rust_to_clang_target_riscv() {
    assert_eq!(
        rust_to_clang_target("riscv64gc-unknown-linux-gnu"),
        "riscv64-unknown-linux-gnu"
    )
}

#[test]
fn test_rust_to_clang_target_espidf() {
    assert_eq!(
        rust_to_clang_target("riscv32imc-esp-espidf"),
        "riscv32-esp-elf"
    );
    assert_eq!(
        rust_to_clang_target("xtensa-esp32-espidf"),
        "xtensa-esp32-elf"
    );
}
