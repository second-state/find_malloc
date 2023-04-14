use anyhow::Context;
use find_malloc::export_malloc;
use std::fs::File;
use std::io::{BufReader, Seek};
use structopt::StructOpt;
use wasmbin::Module;

#[derive(StructOpt)]
struct DumpOpts {
    filename: String,
    output_filename: String,
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let opts = DumpOpts::from_args();
    let f = File::open(&opts.filename)?;
    let mut f = BufReader::new(f);
    let m = Module::decode_from(&mut f).with_context(|| {
        format!(
            "Parsing error at offset 0x{:08X}",
            f.stream_position().unwrap()
        )
    })?;

    let m = export_malloc(m).unwrap();
    m.encode_into(File::create(opts.output_filename)?)?;
    Ok(())
}
