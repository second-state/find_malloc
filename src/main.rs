use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufReader, Seek};
use structopt::StructOpt;
use wasmbin::indices::FuncId;
use wasmbin::instructions::Instruction;
use wasmbin::io::DecodeError;
use wasmbin::sections::{self, ExportDesc, Section};
use wasmbin::types::ValueType;
use wasmbin::Module;

#[derive(StructOpt)]
struct DumpOpts {
    filename: String,
    output_filename: String,
}

fn find_from_exports(m: &Module) -> Result<Option<(wasmbin::indices::FuncId, bool)>, DecodeError> {
    log::info!("find malloc from exports");
    if let Some(blob_exports) = m.find_std_section::<sections::payload::Export>() {
        let exports = blob_exports.try_contents()?;
        for export in exports {
            match (export.name.as_str(), export.desc.clone()) {
                ("malloc", ExportDesc::Func(func_id)) => return Ok(Some((func_id, true))),
                ("dlmalloc", ExportDesc::Func(func_id)) => return Ok(Some((func_id, false))),
                _ => {}
            }
        }
    }
    Ok(None)
}

fn find_from_name_section(m: &Module) -> Result<Option<wasmbin::indices::FuncId>, DecodeError> {
    log::info!("find malloc from name sections");
    let mut malloc_func_index: Option<wasmbin::indices::FuncId> = None;

    'find: {
        // find from name section
        for s in m.sections.iter() {
            if let Section::Custom(custom_section) = s {
                let custom_section = custom_section.try_contents()?;
                match custom_section {
                    sections::CustomSection::Name(names) => {
                        let names = names.try_contents()?;
                        for s in names {
                            if let sections::NameSubSection::Func(f) = s {
                                let f = f.try_contents()?;
                                for f in &f.items {
                                    match f.value.as_str() {
                                        "malloc" | "dlmalloc" => {
                                            let _ = malloc_func_index.insert(f.index.clone());
                                            break 'find;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    };
    Ok(malloc_func_index)
}

fn find_by_wasi(
    m: &Module,
    module_name: &str,
    func_name: &str,
) -> Result<Option<FuncId>, DecodeError> {
    log::info!("find malloc by {}:{}", module_name, func_name);
    let mut malloc_fn_index: Option<FuncId> = None;

    let mut target_wasi_fn_index: Option<FuncId> = None;

    let import_func_size = if let Some(s) = m.find_std_section::<sections::payload::Import>() {
        s.try_contents()?.iter().fold(0, |t, i| {
            if i.path.module.as_str() == module_name && i.path.name.as_str() == func_name {
                target_wasi_fn_index = Some(FuncId { index: t })
            }
            if let sections::ImportDesc::Func(..) = i.desc {
                t + 1
            } else {
                t
            }
        })
    } else {
        0
    };

    if target_wasi_fn_index.is_none() {
        return Ok(None);
    }

    let target_wasi_fn_index = target_wasi_fn_index.unwrap();

    let empty_types = vec![];
    let types = if let Some(types) = m.find_std_section::<sections::payload::Type>() {
        types.try_contents()?
    } else {
        &empty_types
    };

    let empty_funcs = vec![];
    let funcs = if let Some(funcs) = m.find_std_section::<sections::payload::Function>() {
        funcs.try_contents()?
    } else {
        &empty_funcs
    };

    let empty_func_body = vec![];
    let func_body = if let Some(code) = m.find_std_section::<sections::payload::Code>() {
        code.try_contents()?
    } else {
        &empty_func_body
    };

    for (_caller_index, body) in func_body.into_iter().enumerate() {
        let body = body.try_contents()?;

        let mut find = false;

        for expr in &body.expr {
            match expr {
                Instruction::Call(func_id) => {
                    if find {
                        let func_index = func_id.index;
                        if func_index < import_func_size {
                            log::warn!("found a host function after {}:{}", module_name, func_name);
                            return Ok(None);
                        }
                        let wasm_func_index = (func_index - import_func_size) as usize;
                        let caller_type = funcs
                            .get(wasm_func_index)
                            .and_then(|type_index| types.get(type_index.index as usize));
                        if let Some(func_type) = caller_type {
                            match (func_type.params.as_slice(), func_type.results.as_slice()) {
                                (&[ValueType::I32], &[ValueType::I32]) => {
                                    malloc_fn_index = Some(func_id.clone());
                                    return Ok(malloc_fn_index);
                                }
                                _ => {
                                    log::warn!("expect a fn(i32)->i32,but got a {:?}", func_type);
                                    return Ok(None);
                                }
                            }
                        } else {
                            log::warn!("can't find FuncType of {:?}", func_id);
                            return Ok(None);
                        }
                    } else {
                        if func_id.index == target_wasi_fn_index.index {
                            find = true;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(malloc_fn_index)
}

fn export_malloc(mut m: Module) -> Result<Module, DecodeError> {
    let mut malloc_func_index: Option<wasmbin::indices::FuncId>;

    'find: {
        if let Some((index, is_malloc)) = find_from_exports(&m)? {
            if is_malloc {
                return Ok(m);
            } else {
                malloc_func_index = Some(index);
                break 'find;
            }
        }
        malloc_func_index = find_from_name_section(&m)?;
        if malloc_func_index.is_some() {
            break 'find;
        }

        malloc_func_index = find_by_wasi(&m, "wasi_snapshot_preview1", "environ_sizes_get")?;
        if malloc_func_index.is_some() {
            break 'find;
        }

        malloc_func_index = find_by_wasi(&m, "wasi_snapshot_preview1", "args_sizes_get")?;
        if malloc_func_index.is_some() {
            break 'find;
        }
    }

    if let Some(malloc_func_index) = malloc_func_index {
        log::info!("export {:?} as malloc", malloc_func_index);
        m.find_or_insert_std_section(|| sections::payload::Export::default())
            .try_contents_mut()?
            .push(sections::Export {
                name: "malloc".to_string(),
                desc: sections::ExportDesc::Func(malloc_func_index),
            });
    } else {
        log::error!("malloc is not found");
    }

    Ok(m)
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
