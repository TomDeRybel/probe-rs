mod common;
mod debugger;
mod info;
mod run;

use debugger::CliState;

use probe_rs::{
    debug::DebugInfo,
    flashing::{erase_all, BinOptions, FileDownloadError, Format},
    MemoryInterface, Probe,
};

use probe_rs_cli_util::{
    common_options::{CargoOptions, FlashOptions, ProbeOptions},
    flash::run_flash_download,
};

use capstone::{arch::arm::ArchMode, prelude::*, Capstone, Endian};
use clap::arg_enum;
use rustyline::Editor;
use structopt::StructOpt;

use anyhow::{anyhow, Context, Result};

use std::time::Instant;
use std::{fs::File, path::PathBuf};
use std::{num::ParseIntError, path::Path};

#[derive(StructOpt)]
#[structopt(
    name = "Probe-rs CLI",
    about = "A CLI for on top of the debug probe capabilities provided by probe-rs",
    author = "Noah Hüsser <yatekii@yatekii.ch> / Dominik Böhi <dominik.boehi@gmail.ch>"
)]
enum Cli {
    /// List all connected debug probes
    #[structopt(name = "list")]
    List {},
    /// Gets infos about the selected debug probe and connected target
    #[structopt(name = "info")]
    Info {
        #[structopt(flatten)]
        common: ProbeOptions,
    },
    /// Resets the target attached to the selected debug probe
    #[structopt(name = "reset")]
    Reset {
        #[structopt(flatten)]
        shared: CoreOptions,

        #[structopt(flatten)]
        common: ProbeOptions,

        /// Whether the reset pin should be asserted or deasserted. If left open, just pulse it
        assert: Option<bool>,
    },
    #[structopt(name = "debug")]
    Debug {
        #[structopt(flatten)]
        shared: CoreOptions,

        #[structopt(flatten)]
        common: ProbeOptions,

        #[structopt(long, parse(from_os_str))]
        /// Binary to debug
        exe: Option<PathBuf>,
    },
    /// Dump memory from attached target
    #[structopt(name = "dump")]
    Dump {
        #[structopt(flatten)]
        shared: CoreOptions,

        #[structopt(flatten)]
        common: ProbeOptions,

        /// The address of the memory to dump from the target.
        #[structopt(parse(try_from_str = parse_u32))]
        loc: u32,
        /// The amount of memory (in words) to dump.
        #[structopt(parse(try_from_str = parse_u32))]
        words: u32,
    },
    /// Download memory to attached target
    #[structopt(name = "download")]
    Download {
        #[structopt(flatten)]
        common: ProbeOptions,

        /// Format of the file to be downloaded to the flash. Possible values are case-insensitive.
        #[structopt(
            possible_values = &DownloadFileType::variants(),
            case_insensitive = true,
            default_value = "elf",
            long
        )]
        format: DownloadFileType,

        /// The address in memory where the binary will be put at. This is only considered when `bin` is selected as the format.
        #[structopt(long, parse(try_from_str = parse_u32))]
        base_address: Option<u32>,
        /// The number of bytes to skip at the start of the binary file. This is only considered when `bin` is selected as the format.
        #[structopt(long, parse(try_from_str = parse_u32))]
        skip_bytes: Option<u32>,

        /// The path to the file to be downloaded to the flash
        path: String,
    },
    /// Erase all nonvolatile memory of attached target
    #[structopt(name = "erase")]
    Erase {
        #[structopt(flatten)]
        common: ProbeOptions,
    },
    /// Flash and run an ELF program
    #[structopt(name = "run")]
    Run {
        #[structopt(flatten)]
        common: ProbeOptions,

        /// The path to the ELF file to flash and run
        path: String,
    },
    #[structopt(name = "trace")]
    Trace {
        #[structopt(flatten)]
        shared: CoreOptions,

        #[structopt(flatten)]
        common: ProbeOptions,

        /// The address of the memory to dump from the target.
        #[structopt(parse(try_from_str = parse_u32))]
        loc: u32,
    },
}

/// Shared options for core selection, shared between commands
#[derive(StructOpt)]
struct CoreOptions {
    #[structopt(long, default_value = "0")]
    core: usize,
}

fn main() -> Result<()> {
    // Initialize the logging backend.
    pretty_env_logger::init();

    let matches = Cli::from_args();

    match matches {
        Cli::List {} => list_connected_devices(),
        Cli::Info { common } => crate::info::show_info_of_device(&common),
        Cli::Reset {
            shared,
            common,
            assert,
        } => reset_target_of_device(&shared, &common, assert),
        Cli::Debug {
            shared,
            common,
            exe,
        } => debug(&shared, &common, exe),
        Cli::Dump {
            shared,
            common,
            loc,
            words,
        } => dump_memory(&shared, &common, loc, words),
        Cli::Download {
            common,
            format,
            base_address,
            skip_bytes,
            path,
        } => download_program_fast(common, format.into(base_address, skip_bytes), &path),
        Cli::Run { common, path } => run::run(common, &path),
        Cli::Erase { common } => erase(&common),
        Cli::Trace {
            shared,
            common,
            loc,
        } => trace_u32_on_target(&shared, &common, loc),
    }
}

fn list_connected_devices() -> Result<()> {
    let links = Probe::list_all();

    if !links.is_empty() {
        println!("The following devices were found:");
        links
            .iter()
            .enumerate()
            .for_each(|(num, link)| println!("[{}]: {:?}", num, link));
    } else {
        println!("No devices were found.");
    }

    Ok(())
}

fn dump_memory(
    shared_options: &CoreOptions,
    common: &ProbeOptions,
    loc: u32,
    words: u32,
) -> Result<()> {
    let mut session = common.simple_attach()?;

    let mut data = vec![0_u32; words as usize];

    // Start timer.
    let instant = Instant::now();

    // let loc = 220 * 1024;

    let mut core = session.core(shared_options.core)?;

    core.read_32(loc, &mut data.as_mut_slice())?;
    // Stop timer.
    let elapsed = instant.elapsed();

    // Print read values.
    for word in 0..words {
        println!(
            "Addr 0x{:08x?}: 0x{:08x}",
            loc + 4 * word,
            data[word as usize]
        );
    }
    // Print stats.
    println!("Read {:?} words in {:?}", words, elapsed);

    Ok(())
}

fn download_program_fast(common: ProbeOptions, format: Format, path: &str) -> Result<()> {
    let mut session = common.simple_attach()?;

    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) => return Err(FileDownloadError::IO(e)).context("Failed to open binary file."),
    };

    let mut loader = session.target().flash_loader();

    match format {
        Format::Bin(options) => loader.load_bin_data(&mut file, options),
        Format::Elf => loader.load_elf_data(&mut file),
        Format::Hex => loader.load_hex_data(&mut file),
    }?;

    run_flash_download(
        &mut session,
        Path::new(path),
        &FlashOptions {
            version: false,
            list_chips: false,
            list_probes: false,
            disable_progressbars: false,
            reset_halt: false,
            log: None,
            restore_unwritten: false,
            flash_layout_output_path: None,
            elf: None,
            work_dir: None,
            cargo_options: CargoOptions::default(),
            probe_options: common,
        },
        loader,
    )?;

    Ok(())
}

fn erase(common: &ProbeOptions) -> Result<()> {
    let mut session = common.simple_attach()?;

    erase_all(&mut session)?;

    Ok(())
}

fn reset_target_of_device(
    shared_options: &CoreOptions,
    common: &ProbeOptions,
    _assert: Option<bool>,
) -> Result<()> {
    let mut session = common.simple_attach()?;

    session.core(shared_options.core)?.reset()?;

    Ok(())
}

fn trace_u32_on_target(
    shared_options: &CoreOptions,
    common: &ProbeOptions,
    loc: u32,
) -> Result<()> {
    use scroll::{Pwrite, LE};
    use std::io::prelude::*;
    use std::thread::sleep;
    use std::time::Duration;

    let mut xs = vec![];
    let mut ys = vec![];

    let start = Instant::now();

    let mut session = common.simple_attach()?;

    let mut core = session.core(shared_options.core)?;

    loop {
        // Prepare read.
        let elapsed = start.elapsed();
        let instant = elapsed.as_secs() * 1000 + u64::from(elapsed.subsec_millis());

        // Read data.
        let value: u32 = core.read_word_32(loc)?;

        xs.push(instant);
        ys.push(value);

        // Send value to plot.py.
        let mut buf = [0_u8; 8];
        // Unwrap is safe!
        buf.pwrite_with(instant, 0, LE).unwrap();
        buf.pwrite_with(value, 4, LE).unwrap();
        std::io::stdout().write_all(&buf)?;

        std::io::stdout().flush()?;

        // Schedule next read.
        let elapsed = start.elapsed();
        let instant = elapsed.as_secs() * 1000 + u64::from(elapsed.subsec_millis());
        let poll_every_ms = 50;
        let time_to_wait = poll_every_ms - instant % poll_every_ms;
        sleep(Duration::from_millis(time_to_wait));
    }
}

fn debug(shared_options: &CoreOptions, common: &ProbeOptions, exe: Option<PathBuf>) -> Result<()> {
    let mut session = common.simple_attach()?;

    let cs = Capstone::new()
        .arm()
        .mode(ArchMode::Thumb)
        .endian(Endian::Little)
        .build()
        .map_err(|err| anyhow!("Error creating capstone: {:?}", err))?;

    let di = exe
        .as_ref()
        .and_then(|path| DebugInfo::from_file(path).ok());

    let cli = debugger::DebugCli::new();

    let core = session.core(shared_options.core)?;

    let mut cli_data = debugger::CliData {
        core,
        debug_info: di,
        capstone: cs,
    };

    let mut rl = Editor::<()>::new();

    loop {
        let readline = rl.readline(">> ");
        match readline {
            Ok(line) => {
                let history_entry: &str = line.as_ref();
                rl.add_history_entry(history_entry);
                let cli_state = cli.handle_line(&line, &mut cli_data)?;

                match cli_state {
                    CliState::Continue => (),
                    CliState::Stop => break,
                }
            }
            Err(e) => {
                use rustyline::error::ReadlineError;

                match e {
                    // For end of file and ctrl-c, we just quit
                    ReadlineError::Eof | ReadlineError::Interrupted => return Ok(()),
                    actual_error => {
                        // Show error message and quit
                        println!("Error handling input: {:?}", actual_error);
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

arg_enum! {
    #[derive(Debug, Clone, Copy)]
    enum DownloadFileType {
        Elf,
        Hex,
        Bin,
    }
}

impl DownloadFileType {
    fn into(self, base_address: Option<u32>, skip: Option<u32>) -> Format {
        match self {
            DownloadFileType::Elf => Format::Elf,
            DownloadFileType::Hex => Format::Hex,
            DownloadFileType::Bin => Format::Bin(BinOptions {
                base_address,
                skip: skip.unwrap_or(0),
            }),
        }
    }
}

fn parse_u32(input: &str) -> Result<u32, ParseIntError> {
    parse_int::parse(input)
}
