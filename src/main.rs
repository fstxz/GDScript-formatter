use std::{
    env, fs,
    io::{self, IsTerminal, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
};

use clap::{CommandFactory, Parser};
use rayon::prelude::*;

use gdscript_formatter::{
    FormatterConfig,
    formatter::{Formatter, format_gdscript_with_config},
};

const DAEMON_ADDR: &str = "localhost:27542";

/// This struct is used to hold all the information about the result when
/// formatting a single file. Now that we use parallel processing, we need to
/// keep track of the original index to order the files in the output when
/// printing results.
#[derive(Debug, Clone)]
struct FormatterOutput {
    index: usize,
    file_path: PathBuf,
    formatted_content: String,
    is_formatted: bool,
}

#[derive(Parser)]
#[clap(
    about = "A GDScript code formatter using Topiary and Tree-sitter",
    // Use the version number directly from Cargo.toml at compile time
    version = env!("CARGO_PKG_VERSION"),
    long_about = "Format GDScript files with consistent style and indentation. \
    By default, the formatter overwrites input files with the formatted code. \
    Use --stdout to output to standard output instead."
)]
struct Args {
    #[arg(
        help = "Input GDScript file(s) to format. If no file path is provided, the program reads from standard input and outputs to standard output.",
        value_name = "FILES"
    )]
    input: Vec<PathBuf>,
    #[arg(
        long,
        help = "Output formatted code to stdout instead of overwriting the input file. \
        If multiple input files are provided, each file's content is preceded by a comment indicating the file name, with the form \
        #--file:<file_path> \
        This flag is ignored when reading from stdin (stdout is always used)."
    )]
    stdout: bool,
    #[arg(
        short,
        long,
        help = "Check if the file is already formatted without making changes. \
        Exits with code 0 if the file is already formatted and 1 if it's not formatted"
    )]
    check: bool,
    #[arg(
        long,
        help = "Use spaces for indentation instead of tabs. \
        The number of spaces is controlled by --indent-size"
    )]
    use_spaces: bool,
    #[arg(
        long,
        help = "Number of spaces to use for each indentation level when --use-spaces is enabled. \
        Has no effect without the --use-spaces flag.",
        default_value = "4",
        value_name = "NUM"
    )]
    indent_size: usize,
    #[arg(
        long,
        help = "Reorder source-level declarations (signals, properties, methods, etc.) according to the official GDScript style guide. \
        This is optional and applies after the main formatting pass."
    )]
    reorder_code: bool,
    #[arg(
        short,
        long,
        help = "Enable safe mode. This mode ensures that after formatting, the code still has the same syntax and structure \
        as before. If not, formatting is canceled. This offers some protection against the formatter failing on new syntax \
        at the cost of a small little extra running time. Currently incompatible with --reorder-code.",
        conflicts_with = "reorder_code"
    )]
    safe: bool,
    #[arg(
        long,
        help = "Run the formatter in daemon mode. To connect to this daemeon and format, use --client."
    )]
    daemon: bool,
    #[arg(
        long,
        help = "Run the formatter in client mode. Requires a running daemon."
    )]
    client: bool,
    #[arg(
        long,
        help = "This will automatically start the daemon if it's not already running. Subsequent invocations of the formatter \
        with this flag will use the daemon to format the code. The daemon will exit automatically after some time of inactivity. \
        Don't use this flag together with --daemon and --client."
    )]
    auto_daemon: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // If there are no arguments and nothing piped from stdin, print the help message
    if env::args().len() == 1 && io::stdin().is_terminal() {
        Args::command().print_help()?;
        println!();
        return Ok(());
    }

    let args = Args::parse();

    let config = FormatterConfig {
        indent_size: args.indent_size,
        use_spaces: args.use_spaces,
        reorder_code: args.reorder_code,
        safe: args.safe,
    };

    if args.daemon {
        if args.client {
            return Err("Can't be a daemon and client at the same time.".into());
        }
        return daemon_main(&config);
    }

    if args.input.is_empty() {
        let mut input_content = String::new();
        io::stdin()
            .read_to_string(&mut input_content)
            .map_err(|error| format!("Failed to read from stdin: {}", error))?;

        let formatted_content = if args.client {
            client_main(&input_content)
        } else {
            format_gdscript_with_config(&input_content, &config)
        }?;

        if args.check {
            if input_content != formatted_content {
                eprintln!("The input passed via stdin is not formatted");
                std::process::exit(1);
            } else {
                eprintln!("The input passed via stdin is already formatted");
            }
        } else {
            print!("{}", formatted_content);
        }

        return Ok(());
    }

    let input_gdscript_files: Vec<&PathBuf> = args
        .input
        .iter()
        .filter(|path| path.extension().map_or(false, |ext| ext == "gd"))
        .collect();

    if input_gdscript_files.is_empty() {
        eprintln!(
            "Error: No GDScript files found in the arguments provided. Please provide at least one .gd file."
        );
        std::process::exit(1);
    }

    let total_files = input_gdscript_files.len();

    eprint!(
        "Formatting {} file{}...",
        total_files,
        if total_files == 1 { "" } else { "s" }
    );
    io::stdout().flush().unwrap();

    // We use the rayon library to automatically process files in parallel for
    // us. The formatter runs largely single threaded so this speeds things up a
    // lot on multi-core CPUs
    let outputs: Vec<Result<FormatterOutput, String>> = input_gdscript_files
        .par_iter()
        .enumerate()
        .map(|(index, file_path)| {
            let input_content = fs::read_to_string(file_path).map_err(|error| {
                format!("Failed to read file {}: {}", file_path.display(), error)
            })?;

            let formatted_content =
                format_gdscript_with_config(&input_content, &config).map_err(|error| {
                    format!("Failed to format file {}: {}", file_path.display(), error)
                })?;

            let is_formatted = input_content == formatted_content;

            Ok(FormatterOutput {
                index,
                file_path: (*file_path).clone(),
                formatted_content,
                is_formatted,
            })
        })
        .collect();

    // Restore the original order of the input files based on their initial index
    let mut sorted_outputs: Vec<_> = outputs.into_iter().collect();
    sorted_outputs.sort_by_key(|output| {
        match output {
            Ok(output) => output.index,
            // Sort errors at the end in no particular order
            Err(_) => usize::MAX,
        }
    });

    // If true, all input files were already formatted (used for check mode)
    let mut all_formatted = true;
    for output in sorted_outputs {
        match output {
            Ok(output) => {
                if args.check {
                    if !output.is_formatted {
                        all_formatted = false;
                    }
                } else if args.stdout {
                    // Clear the progress message before printing formatted files to stdout
                    terminal_clear_line();
                    // A little bit hacky, but because terminals by default output both stdout and stderr
                    // we need to return carriage to the start to print formatted output from the start of the line
                    eprint!("\r");
                    // If there are multiple input files we still allow stdout but we print a separator
                    if total_files > 1 {
                        println!("#--file:{}", output.file_path.display());
                    }
                    print!("{}", output.formatted_content);
                } else {
                    fs::write(&output.file_path, output.formatted_content).map_err(|e| {
                        format!(
                            "Failed to write to file {}: {}",
                            output.file_path.display(),
                            e
                        )
                    })?;
                }
            }
            Err(error_msg) => {
                return Err(error_msg.into());
            }
        }
    }

    if args.check {
        if all_formatted {
            terminal_clear_line();
            eprintln!("\rAll {} file(s) are formatted", total_files);
        } else {
            terminal_clear_line();
            eprintln!("\rSome files are not formatted");
            std::process::exit(1);
        }
    } else if !args.stdout {
        terminal_clear_line();
        eprintln!(
            "\rFormatted {} file{}",
            total_files,
            if total_files == 1 { "" } else { "s" }
        );
    }

    Ok(())
}

fn terminal_clear_line() {
    eprint!("\r{}", " ".repeat(80));
}

// Packet format:
//   uint32, little-endian | file content size
//   byte array            | file content
fn daemon_main(config: &FormatterConfig) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(DAEMON_ADDR)?;

    let mut formatter = Formatter::new(config);

    println!("Daemon started, listening for incoming connections...");
    loop {
        let (mut stream, _) = listener.accept()?;

        let mut file_length_bytes = [0u8; 4];
        stream.read_exact(&mut file_length_bytes)?;

        let file_length = u32::from_le_bytes(file_length_bytes);

        let mut file_buffer = vec![0; file_length as usize];
        stream.read_exact(&mut file_buffer)?;

        let content = String::from_utf8(file_buffer)?;

        formatter.format(content)?;
        let result = formatter.finish()?;

        let content_length = result.len() as u32;
        let mut buffer = Vec::with_capacity((content_length + 4) as usize);
        buffer.extend_from_slice(&content_length.to_le_bytes());
        buffer.extend_from_slice(result.as_bytes());
        stream.write_all(&buffer)?;
    }
}

fn client_main(content: &str) -> Result<String, Box<dyn std::error::Error>> {
    let content = content.to_owned();
    let mut stream = TcpStream::connect(DAEMON_ADDR)?;

    let content_length = content.len() as u32;
    let mut buffer = Vec::with_capacity((content_length + 4) as usize);
    buffer.extend_from_slice(&content_length.to_le_bytes());
    buffer.extend_from_slice(content.as_bytes());
    stream.write_all(&buffer)?;

    let mut formatted_content_length_bytes = [0u8; 4];
    stream.read_exact(&mut formatted_content_length_bytes)?;

    let formatted_content_length = u32::from_le_bytes(formatted_content_length_bytes);
    let mut formatted_content_buffer = vec![0; formatted_content_length as usize];
    stream.read_exact(&mut formatted_content_buffer)?;
    let formatted_content = String::from_utf8(formatted_content_buffer)?;
    Ok(formatted_content)
}
