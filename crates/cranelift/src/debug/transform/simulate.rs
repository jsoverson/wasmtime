use super::expression::{CompiledExpression, FunctionFrameInfo};
use super::utils::{add_internal_types, append_vmctx_info, get_function_frame_info};
use super::AddressTransform;
use crate::debug::ModuleMemoryOffset;
use crate::CompiledFunctionsMetadata;
use anyhow::{Context, Error};
use cranelift_codegen::isa::TargetIsa;
use cranelift_wasm::get_vmctx_value_label;
use gimli::write;
use gimli::{self, LineEncoding};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use wasmtime_environ::{
    DebugInfoData, DefinedFuncIndex, EntityRef, FuncIndex, FunctionMetadata, WasmFileInfo, WasmType,
};

const PRODUCER_NAME: &str = "wasmtime";

macro_rules! assert_dwarf_str {
    ($s:expr) => {{
        let s = $s;
        if cfg!(debug_assertions) {
            // Perform check the same way as gimli does it.
            let bytes: Vec<u8> = s.clone().into();
            debug_assert!(!bytes.contains(&0), "DWARF string shall not have NULL byte");
        }
        s
    }};
}

fn generate_line_info(
    addr_tr: &AddressTransform,
    translated: &HashSet<DefinedFuncIndex>,
    out_encoding: gimli::Encoding,
    w: &WasmFileInfo,
    comp_dir_id: write::StringId,
    name_id: write::StringId,
    name: &str,
) -> Result<write::LineProgram, Error> {
    let out_comp_dir = write::LineString::StringRef(comp_dir_id);
    let out_comp_name = write::LineString::StringRef(name_id);

    let line_encoding = LineEncoding::default();

    let mut out_program = write::LineProgram::new(
        out_encoding,
        line_encoding,
        out_comp_dir,
        out_comp_name,
        None,
    );

    let file_index = out_program.add_file(
        write::LineString::String(name.as_bytes().to_vec()),
        out_program.default_directory(),
        None,
    );

    for (i, map) in addr_tr.map() {
        let symbol = i.index();
        if translated.contains(&i) {
            continue;
        }

        let base_addr = map.offset;
        out_program.begin_sequence(Some(write::Address::Symbol { symbol, addend: 0 }));
        for addr_map in map.addresses.iter() {
            let address_offset = (addr_map.generated - base_addr) as u64;
            out_program.row().address_offset = address_offset;
            out_program.row().op_index = 0;
            out_program.row().file = file_index;
            let wasm_offset = w.code_section_offset + addr_map.wasm;
            out_program.row().line = wasm_offset;
            out_program.row().column = 0;
            out_program.row().discriminator = 1;
            out_program.row().is_statement = true;
            out_program.row().basic_block = false;
            out_program.row().prologue_end = false;
            out_program.row().epilogue_begin = false;
            out_program.row().isa = 0;
            out_program.generate_row();
        }
        let end_addr = (map.offset + map.len - 1) as u64;
        out_program.end_sequence(end_addr);
    }

    Ok(out_program)
}

fn check_invalid_chars_in_name(s: &str) -> Option<&str> {
    if s.contains('\x00') {
        None
    } else {
        Some(s)
    }
}

fn autogenerate_dwarf_wasm_path(di: &DebugInfoData) -> PathBuf {
    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
    let module_name = di
        .name_section
        .module_name
        .and_then(check_invalid_chars_in_name)
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("<gen-{}>", NEXT_ID.fetch_add(1, SeqCst)));
    let path = format!("/<wasm-module>/{}.wasm", module_name);
    PathBuf::from(path)
}

struct WasmTypesDieRefs {
    vmctx: write::UnitEntryId,
    i32: write::UnitEntryId,
    i64: write::UnitEntryId,
    f32: write::UnitEntryId,
    f64: write::UnitEntryId,
}

fn add_wasm_types(
    unit: &mut write::Unit,
    root_id: write::UnitEntryId,
    out_strings: &mut write::StringTable,
    memory_offset: &ModuleMemoryOffset,
) -> WasmTypesDieRefs {
    let (_wp_die_id, vmctx_die_id) = add_internal_types(unit, root_id, out_strings, memory_offset);

    macro_rules! def_type {
        ($id:literal, $size:literal, $enc:path) => {{
            let die_id = unit.add(root_id, gimli::DW_TAG_base_type);
            let die = unit.get_mut(die_id);
            die.set(
                gimli::DW_AT_name,
                write::AttributeValue::StringRef(out_strings.add($id)),
            );
            die.set(gimli::DW_AT_byte_size, write::AttributeValue::Data1($size));
            die.set(gimli::DW_AT_encoding, write::AttributeValue::Encoding($enc));
            die_id
        }};
    }

    let i32_die_id = def_type!("i32", 4, gimli::DW_ATE_signed);
    let i64_die_id = def_type!("i64", 8, gimli::DW_ATE_signed);
    let f32_die_id = def_type!("f32", 4, gimli::DW_ATE_float);
    let f64_die_id = def_type!("f64", 8, gimli::DW_ATE_float);

    WasmTypesDieRefs {
        vmctx: vmctx_die_id,
        i32: i32_die_id,
        i64: i64_die_id,
        f32: f32_die_id,
        f64: f64_die_id,
    }
}

fn resolve_var_type(
    index: usize,
    wasm_types: &WasmTypesDieRefs,
    func_meta: &FunctionMetadata,
) -> Option<(write::UnitEntryId, bool)> {
    let (ty, is_param) = if index < func_meta.params.len() {
        (func_meta.params[index], true)
    } else {
        let mut i = (index - func_meta.params.len()) as u32;
        let mut j = 0;
        while j < func_meta.locals.len() && i >= func_meta.locals[j].0 {
            i -= func_meta.locals[j].0;
            j += 1;
        }
        if j >= func_meta.locals.len() {
            // Ignore the var index out of bound.
            return None;
        }
        (func_meta.locals[j].1, false)
    };
    let type_die_id = match ty {
        WasmType::I32 => wasm_types.i32,
        WasmType::I64 => wasm_types.i64,
        WasmType::F32 => wasm_types.f32,
        WasmType::F64 => wasm_types.f64,
        _ => {
            // Ignore unsupported types.
            return None;
        }
    };
    Some((type_die_id, is_param))
}

fn generate_vars(
    unit: &mut write::Unit,
    die_id: write::UnitEntryId,
    addr_tr: &AddressTransform,
    frame_info: &FunctionFrameInfo,
    scope_ranges: &[(u64, u64)],
    wasm_types: &WasmTypesDieRefs,
    func_meta: &FunctionMetadata,
    locals_names: Option<&HashMap<u32, &str>>,
    out_strings: &mut write::StringTable,
    isa: &dyn TargetIsa,
) -> Result<(), Error> {
    let vmctx_label = get_vmctx_value_label();

    // Normalize order of ValueLabelsRanges keys to have reproducable results.
    let mut vars = frame_info.value_ranges.keys().collect::<Vec<_>>();
    vars.sort_by(|a, b| a.index().cmp(&b.index()));

    for label in vars {
        if label.index() == vmctx_label.index() {
            append_vmctx_info(
                unit,
                die_id,
                wasm_types.vmctx,
                addr_tr,
                Some(frame_info),
                scope_ranges,
                out_strings,
                isa,
            )?;
        } else {
            let var_index = label.index();
            let (type_die_id, is_param) =
                if let Some(result) = resolve_var_type(var_index, wasm_types, func_meta) {
                    result
                } else {
                    // Skipping if type of local cannot be detected.
                    continue;
                };

            let loc_list_id = {
                let locs = CompiledExpression::from_label(*label)
                    .build_with_locals(scope_ranges, addr_tr, Some(frame_info), isa)
                    .map(|i| {
                        i.map(|(begin, length, data)| write::Location::StartLength {
                            begin,
                            length,
                            data,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                unit.locations.add(write::LocationList(locs))
            };

            let var_id = unit.add(
                die_id,
                if is_param {
                    gimli::DW_TAG_formal_parameter
                } else {
                    gimli::DW_TAG_variable
                },
            );
            let var = unit.get_mut(var_id);

            let name_id = match locals_names
                .and_then(|m| m.get(&(var_index as u32)))
                .and_then(|s| check_invalid_chars_in_name(s))
            {
                Some(n) => out_strings.add(assert_dwarf_str!(n)),
                None => out_strings.add(format!("var{}", var_index)),
            };

            var.set(gimli::DW_AT_name, write::AttributeValue::StringRef(name_id));
            var.set(
                gimli::DW_AT_type,
                write::AttributeValue::UnitRef(type_die_id),
            );
            var.set(
                gimli::DW_AT_location,
                write::AttributeValue::LocationListRef(loc_list_id),
            );
        }
    }
    Ok(())
}

fn check_invalid_chars_in_path(path: PathBuf) -> Option<PathBuf> {
    path.clone()
        .to_str()
        .and_then(move |s| if s.contains('\x00') { None } else { Some(path) })
}

pub fn generate_simulated_dwarf(
    addr_tr: &AddressTransform,
    di: &DebugInfoData,
    memory_offset: &ModuleMemoryOffset,
    funcs: &CompiledFunctionsMetadata,
    translated: &HashSet<DefinedFuncIndex>,
    out_encoding: gimli::Encoding,
    out_units: &mut write::UnitTable,
    out_strings: &mut write::StringTable,
    isa: &dyn TargetIsa,
) -> Result<(), Error> {
    let path = di
        .wasm_file
        .path
        .to_owned()
        .and_then(check_invalid_chars_in_path)
        .unwrap_or_else(|| autogenerate_dwarf_wasm_path(di));

    let func_names = &di.name_section.func_names;
    let locals_names = &di.name_section.locals_names;
    let imported_func_count = di.wasm_file.imported_func_count;

    let (unit, root_id, name_id) = {
        let comp_dir_id = out_strings.add(assert_dwarf_str!(path
            .parent()
            .context("path dir")?
            .to_str()
            .context("path dir encoding")?));
        let name = path
            .file_name()
            .context("path name")?
            .to_str()
            .context("path name encoding")?;
        let name_id = out_strings.add(assert_dwarf_str!(name));

        let out_program = generate_line_info(
            addr_tr,
            translated,
            out_encoding,
            &di.wasm_file,
            comp_dir_id,
            name_id,
            name,
        )?;

        let unit_id = out_units.add(write::Unit::new(out_encoding, out_program));
        let unit = out_units.get_mut(unit_id);

        let root_id = unit.root();
        let root = unit.get_mut(root_id);

        let id = out_strings.add(PRODUCER_NAME);
        root.set(gimli::DW_AT_producer, write::AttributeValue::StringRef(id));
        root.set(gimli::DW_AT_name, write::AttributeValue::StringRef(name_id));
        root.set(
            gimli::DW_AT_stmt_list,
            write::AttributeValue::LineProgramRef,
        );
        root.set(
            gimli::DW_AT_comp_dir,
            write::AttributeValue::StringRef(comp_dir_id),
        );
        (unit, root_id, name_id)
    };

    let wasm_types = add_wasm_types(unit, root_id, out_strings, memory_offset);

    for (i, map) in addr_tr.map().iter() {
        let index = i.index();
        if translated.contains(&i) {
            continue;
        }

        let start = map.offset as u64;
        let end = start + map.len as u64;
        let die_id = unit.add(root_id, gimli::DW_TAG_subprogram);
        let die = unit.get_mut(die_id);
        die.set(
            gimli::DW_AT_low_pc,
            write::AttributeValue::Address(write::Address::Symbol {
                symbol: index,
                addend: start as i64,
            }),
        );
        die.set(
            gimli::DW_AT_high_pc,
            write::AttributeValue::Udata(end - start),
        );

        let func_index = imported_func_count + (index as u32);
        let id = match func_names
            .get(&FuncIndex::from_u32(func_index))
            .and_then(|s| check_invalid_chars_in_name(s))
        {
            Some(n) => out_strings.add(assert_dwarf_str!(n)),
            None => out_strings.add(format!("wasm-function[{}]", func_index)),
        };

        die.set(gimli::DW_AT_name, write::AttributeValue::StringRef(id));

        die.set(
            gimli::DW_AT_decl_file,
            write::AttributeValue::StringRef(name_id),
        );

        let f_start = map.addresses[0].wasm;
        let wasm_offset = di.wasm_file.code_section_offset + f_start;
        die.set(
            gimli::DW_AT_decl_file,
            write::AttributeValue::Udata(wasm_offset),
        );

        if let Some(frame_info) = get_function_frame_info(memory_offset, funcs, i) {
            let source_range = addr_tr.func_source_range(i);
            generate_vars(
                unit,
                die_id,
                addr_tr,
                &frame_info,
                &[(source_range.0, source_range.1)],
                &wasm_types,
                &di.wasm_file.funcs[index],
                locals_names.get(&FuncIndex::from_u32(index as u32)),
                out_strings,
                isa,
            )?;
        }
    }

    Ok(())
}
