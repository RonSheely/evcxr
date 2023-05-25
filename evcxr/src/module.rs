// Copyright 2020 The Evcxr Authors.
//
// Licensed under the Apache License, Version 2.0 <LICENSE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE
// or https://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use crate::code_block::CodeBlock;
use crate::errors::bail;
use crate::errors::CompilationError;
use crate::errors::Error;
use crate::eval_context::Config;
use crate::eval_context::ContextState;
use once_cell::sync::Lazy;
use regex::Regex;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

fn shared_object_name_from_crate_name(crate_name: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("lib{crate_name}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{crate_name}.dll")
    } else {
        format!("lib{crate_name}.so")
    }
}

fn create_dir(dir: &Path) -> Result<(), Error> {
    if let Err(err) = fs::create_dir_all(dir) {
        bail!("Error creating directory '{:?}': {}", dir, err);
    }
    Ok(())
}

fn write_file(dir: &Path, basename: &str, contents: &str) -> Result<(), Error> {
    create_dir(dir)?;
    let filename = dir.join(basename);
    // If the file contents is already correct, then skip writing it again. This
    // is mostly to avoid rewriting Cargo.toml which should change relatively
    // little.
    if fs::read_to_string(&filename)
        .map(|c| c == contents)
        .unwrap_or(false)
    {
        return Ok(());
    }
    if let Err(err) = fs::write(&filename, contents) {
        bail!("Error writing '{:?}': {}", filename, err);
    }
    Ok(())
}

/// On Mac, if we copy the dylib, we get intermittent failures where we end up
/// with the previous version of the file when we shouldn't. On windows, if
/// rename the file, we get errors subsequently when something (perhaps the
/// Windows linker) tries to delete the file that it expects to still be there.
/// On Linux either renaming or copying works, but renaming should be more
/// efficient, so we do that.
#[cfg(windows)]
fn rename_or_copy_so_file(src: &Path, dest: &Path) -> Result<(), Error> {
    // Copy file by reading and writing instead of using std::fs::copy. The src
    // is a hard-linked file and we want to make extra sure that we end up with
    // a completely independent copy.
    fn alt_copy(src: &Path, dest: &Path) -> Result<(), std::io::Error> {
        use std::fs::File;
        std::io::copy(&mut File::open(src)?, &mut File::create(dest)?)?;
        Ok(())
    }
    if let Err(err) = alt_copy(src, dest) {
        bail!("Error copying '{:?}' to '{:?}': {}", src, dest, err);
    }
    Ok(())
}

#[cfg(not(windows))]
fn rename_or_copy_so_file(src: &Path, dest: &Path) -> Result<(), Error> {
    if let Err(err) = fs::rename(src, dest) {
        bail!("Error renaming '{:?}' to '{:?}': {}", src, dest, err);
    }
    Ok(())
}

pub(crate) struct Module {
    build_num: i32,
}

const CRATE_NAME: &str = "ctx";

impl Module {
    pub(crate) fn new() -> Result<Module, Error> {
        let module = Module { build_num: 0 };
        Ok(module)
    }

    pub(crate) fn so_path(&self, config: &Config) -> PathBuf {
        config
            .deps_dir()
            .join(shared_object_name_from_crate_name(CRATE_NAME))
    }

    // Writes Cargo.toml. Should be called before compile.
    pub(crate) fn write_cargo_toml(&self, state: &ContextState) -> Result<(), Error> {
        write_file(
            state.config.crate_dir(),
            "Cargo.toml",
            &self.get_cargo_toml_contents(state),
        )
    }

    // Writes .cargo/config.toml. Should be called before compile.
    pub(crate) fn write_config_toml(&self, state: &ContextState) -> Result<(), Error> {
        let dot_config_dir = state.config.crate_dir().join(".cargo");
        fs::create_dir_all(dot_config_dir.as_path())?;
        write_file(
            dot_config_dir.as_path(),
            "config.toml",
            &self.get_config_toml_contents(state),
        )
    }

    pub(crate) fn check(
        &mut self,
        code_block: &CodeBlock,
        config: &Config,
    ) -> Result<Vec<CompilationError>, Error> {
        self.write_code(code_block, config)?;
        let output = config
            .cargo_command("check")
            .arg("--message-format=json")
            .output();

        let cargo_output = match output {
            Ok(out) => out,
            Err(err) => bail!("Error running 'cargo check': {}", err),
        };
        let (errors, _non_json_error) = errors_from_cargo_output(&cargo_output, code_block);
        Ok(errors)
    }

    pub(crate) fn compile(
        &mut self,
        code_block: &CodeBlock,
        config: &Config,
    ) -> Result<SoFile, Error> {
        let mut command = config.cargo_command("rustc");
        if config.time_passes && config.toolchain != "nightly" {
            bail!("time_passes option requires nightly compiler");
        }

        command
            .arg("--target")
            .arg(&config.target)
            .arg("--message-format=json")
            .arg("--")
            .arg("-C")
            .arg("prefer-dynamic")
            .env("CARGO_TARGET_DIR", "target")
            .env("RUSTC", &config.rustc_path);
        if config.linker == "lld" {
            command
                .arg("-C")
                .arg(format!("link-arg=-fuse-ld={}", config.linker));
        }
        if let Some(sccache) = &config.sccache {
            command.env("RUSTC_WRAPPER", sccache);
        }
        if config.time_passes {
            command.arg("-Ztime-passes");
        }
        self.write_code(code_block, config)?;
        let cargo_output = run_cargo(command, code_block)?;
        if config.time_passes {
            let output = String::from_utf8_lossy(&cargo_output.stderr);
            eprintln!("{output}");
        }
        self.build_num += 1;
        let copied_so_file = config
            .deps_dir()
            .join(shared_object_name_from_crate_name(&format!(
                "code_{}",
                self.build_num
            )));
        // Every time we compile, the output file is the same. We need to
        // renamed it so that we have a unique filename, otherwise we wouldn't
        // be able to load the result of the next compilation. Also, on Windows,
        // a loaded dll gets locked, so we couldn't even compile a second time
        // if we didn't load a different file.
        rename_or_copy_so_file(&self.so_path(config), &copied_so_file)?;
        Ok(SoFile {
            path: copied_so_file,
        })
    }

    fn write_code(&self, code_block: &CodeBlock, config: &Config) -> Result<(), Error> {
        write_file(&config.src_dir(), "lib.rs", &code_block.code_string())?;
        self.maybe_bump_lib_mtime();
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    fn maybe_bump_lib_mtime(&self) {}

    #[cfg(target_os = "macos")]
    fn maybe_bump_lib_mtime(&self) {
        // Some Macs use a filesystem that only has 1 second precision on file modification
        // timestamps. Cargo uses these timestamps to see if it needs to recompile things, otherwise
        // it just reuses the previous output. We set the modification timestamp on our source file
        // to be 10 seconds in the future. That way it's guaranteed to be newer than any outputs
        // produced by previous runs. In the event that setting the mtime fails, we just ignore it,
        // as this mostly affects tests and we don't want inability to set mtime to break things for
        // users.
        let _ = filetime::set_file_mtime(
            self.src_dir().join("lib.rs"),
            filetime::FileTime::from_unix_time(filetime::FileTime::now().unix_seconds() + 10, 0),
        );
    }

    fn get_cargo_toml_contents(&self, state: &ContextState) -> String {
        let crate_imports = state.format_cargo_deps();
        format!(
            r#"
[package]
name = "{}"
version = "1.0.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]
path = "src/lib.rs"

[profile.dev]
opt-level = {}
debug = false
strip = "debuginfo"
rpath = true
lto = false
debug-assertions = true
codegen-units = 16
panic = 'unwind'
incremental = true
overflow-checks = true

[dependencies]
{}
"#,
            CRATE_NAME,
            state.opt_level(),
            crate_imports
        )
    }

    // Pass offline mode to cargo through .cargo/config.toml
    fn get_config_toml_contents(&self, state: &ContextState) -> String {
        format!(
            r#"
[net]
offline = {}
"#,
            state.offline_mode()
        )
    }
}

/// Run a cargo command prepared for the provided `code_block`, processing the
/// command's output.
fn run_cargo(
    mut command: std::process::Command,
    code_block: &CodeBlock,
) -> Result<std::process::Output, Error> {
    use std::io::{BufRead, Read};

    let mb_child = command
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn();
    let mut child = match mb_child {
        Ok(out) => out,
        Err(err) => bail!("Error running 'cargo rustc': {}", err),
    };

    // Collect stdout in a parallel thread
    let mut stdout = child.stdout.take().unwrap();
    let output_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf)?;
        Ok::<_, Error>(buf)
    });

    // Collect stderr synchronously
    let stderr = std::io::BufReader::new(child.stderr.take().unwrap());
    let mut all_errors = Vec::new();
    for mb_line in stderr.split(10) {
        let mut line = mb_line?;
        tee_error_line(&line);
        all_errors.append(&mut line);
        all_errors.push(10);
    }

    let status = child.wait()?;
    let all_output = output_thread.join().expect("Panic in child thread")?;

    let cargo_output = std::process::Output {
        status,
        stdout: all_output,
        stderr: all_errors,
    };
    if cargo_output.status.success() {
        Ok(cargo_output)
    } else {
        let (errors, non_json_error) = errors_from_cargo_output(&cargo_output, code_block);
        if errors.is_empty() {
            if let Some(error) = non_json_error {
                bail!(Error::Message(error));
            } else {
                bail!(Error::Message(format!(
                    "Compilation failed, but no parsable errors were found. STDERR:\n\
                     {}\nSTDOUT:{}\n",
                    String::from_utf8_lossy(&cargo_output.stderr),
                    String::from_utf8_lossy(&cargo_output.stdout)
                )));
            }
        } else {
            bail!(Error::CompilationErrors(errors));
        }
    }
}

/// Process one line from cargo, either copying it to stderr or ignoring.
///
/// At this point it looks for messages about compiling dependency crates.
fn tee_error_line(line: &[u8]) {
    use std::io::Write;
    static CRATE_COMPILING: Lazy<regex::bytes::Regex> =
        Lazy::new(|| regex::bytes::Regex::new("^\\s*Compiling (\\w+)(?:\\s+.*)?$").unwrap());
    if let Some(captures) = CRATE_COMPILING.captures(line) {
        let crate_name = captures.get(1).unwrap().as_bytes();
        if crate_name != CRATE_NAME.as_bytes() {
            // write line and the following nl symbol as it was stripped before
            std::io::stderr()
                .write_all(line)
                .expect("Writing to stderr should not fail");
            eprintln!();
        }
    }
}

fn errors_from_cargo_output(
    cargo_output: &std::process::Output,
    code_block: &CodeBlock,
) -> (Vec<CompilationError>, Option<String>) {
    // Our compiler errors should all be in JSON format, but for errors from
    // Cargo errors, we need to add explicit matching for those errors that we
    // expect we might see.
    static KNOWN_NON_JSON_ERRORS: Lazy<Regex> =
        Lazy::new(|| Regex::new("(error: no matching package named)").unwrap());

    let stderr = String::from_utf8_lossy(&cargo_output.stderr);
    let stdout = String::from_utf8_lossy(&cargo_output.stdout);
    let mut non_json_error = None;
    let errors = stderr
        .lines()
        .chain(stdout.lines())
        .filter_map(|line| {
            json::parse(line)
                .ok()
                .and_then(|json| CompilationError::opt_new(json, code_block))
                .or_else(|| {
                    if KNOWN_NON_JSON_ERRORS.is_match(line) {
                        non_json_error = Some(line.to_owned());
                    }
                    None
                })
        })
        .collect();
    (errors, non_json_error)
}

pub(crate) struct SoFile {
    pub(crate) path: PathBuf,
}
