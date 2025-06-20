//! Module that handles parsing of Mach-O files from ScanContext bytes
//! The implementation provides utility functions to determine
//! if a given binary data corresponds to a Mach-O file, and further
//! breaks down the data into relevant Mach-O structures and populates
//! both protobuf structure fields and constants. This together with
//! also exported functions can be later used in YARA rules.

use std::cell::RefCell;

use crate::modules::prelude::*;
use crate::modules::protos::macho::*;
use bstr::BString;
use itertools::Itertools;
use md5::{Digest, Md5};

mod parser;
#[cfg(test)]
mod tests;

thread_local!(
    static DYLIB_MD5_CACHE: RefCell<Option<String>> =
        const { RefCell::new(None) };
    static ENTITLEMENT_MD5_CACHE: RefCell<Option<String>> =
        const { RefCell::new(None) };
    static EXPORT_MD5_CACHE: RefCell<Option<String>> =
        const { RefCell::new(None) };
    static IMPORT_MD5_CACHE: RefCell<Option<String>> =
        const { RefCell::new(None) };
    static SYM_MD5_CACHE: RefCell<Option<String>> =
        const { RefCell::new(None) };
);

/// Get the index of a Mach-O file within a fat binary based on CPU type.
///
/// This function iterates through the architecture types contained in a
/// Mach-O fat binary and returns the index of the file that matches the
/// specified CPU type.
///
/// # Arguments
///
/// * `ctx`: A mutable reference to the scanning context.
/// * `type_arg`: The CPU type to search for within the fat binary.
///
/// # Returns
///
/// An `Option<i64>` containing the index of the matching Mach-O file, or
/// `None` if no match is found.
#[module_export(name = "file_index_for_arch")]
fn file_index_type(ctx: &mut ScanContext, type_arg: i64) -> Option<i64> {
    let macho = ctx.module_output::<Macho>()?;

    // Ensure nfat_arch is present
    let nfat = macho.nfat_arch?;

    // Iterate over fat_arch up to nfat entries
    for i in 0..nfat as usize {
        if let Some(arch) = macho.fat_arch.get(i) {
            if let Some(cputype) = arch.cputype {
                if cputype as i64 == type_arg {
                    return Some(i as i64);
                }
            }
        }
    }

    None
}

/// Get the index of a Mach-O file within a fat binary based on both
/// CPU type and subtype.
///
/// This function extends `file_index_type` by also considering the CPU subtype
/// during the search, allowing for more precise matching.
///
/// # Arguments
///
/// * `ctx`: A mutable reference to the scanning context.
/// * `type_arg`: The CPU type to search for.
/// * `subtype_arg`: The CPU subtype to search for.
///
/// # Returns
///
/// An `Option<i64>` containing the index of the matching Mach-O file, or
/// `None` if no match is found.
#[module_export(name = "file_index_for_arch")]
fn file_index_subtype(
    ctx: &mut ScanContext,
    type_arg: i64,
    subtype_arg: i64,
) -> Option<i64> {
    let macho = ctx.module_output::<Macho>()?;

    // Ensure nfat_arch is present
    let nfat = macho.nfat_arch?;

    // Iterate over fat_arch up to nfat entries
    for i in 0..nfat as usize {
        if let Some(arch) = macho.fat_arch.get(i) {
            if let (Some(cputype), Some(cpusubtype)) =
                (arch.cputype, arch.cpusubtype)
            {
                if cputype as i64 == type_arg
                    && cpusubtype as i64 == subtype_arg
                {
                    return Some(i as i64);
                }
            }
        }
    }

    None
}

/// Get the real entry point offset for a specific CPU type within a fat
/// Mach-O binary.
///
/// It navigates through the architectures in the binary, finds the one that
/// matches the specified CPU type, and returns its entry point offset.
///
/// # Arguments
///
/// * `ctx`: A mutable reference to the scanning context.
/// * `type_arg`: The CPU type of the desired architecture.
///
/// # Returns
///
/// An `Option<i64>` containing the offset of the entry point for the specified
/// architecture, or `None` if not found.
#[module_export(name = "entry_point_for_arch")]
fn ep_for_arch_type(ctx: &mut ScanContext, type_arg: i64) -> Option<i64> {
    let macho = ctx.module_output::<Macho>()?;

    // Ensure nfat_arch is present
    let nfat = macho.nfat_arch?;

    // Iterate over fat_arch up to nfat entries
    for i in 0..nfat as usize {
        if let Some(arch) = macho.fat_arch.get(i) {
            if let Some(cputype) = arch.cputype {
                if cputype as i64 == type_arg {
                    let file_offset = arch.offset?;
                    let entry_point = macho.file.get(i)?.entry_point?;
                    return file_offset
                        .checked_add(entry_point)
                        .map(|sum| sum as i64);
                }
            }
        }
    }

    None
}

/// Get the real entry point offset for a specific CPU type and subtype
/// within a fat Mach-O binary.
///
/// Similar to `ep_for_arch_type`, but adds consideration for the CPU subtype
/// to allow for more precise location of the entry point.
///
/// # Arguments
///
/// * `ctx`: A mutable reference to the scanning context.
/// * `type_arg`: The CPU type of the desired architecture.
/// * `subtype_arg`: The CPU subtype of the desired architecture.
///
/// # Returns
///
/// An `Option<i64>` containing the offset of the entry point for the specified
/// architecture and subtype, or `None` if not found.
#[module_export(name = "entry_point_for_arch")]
fn ep_for_arch_subtype(
    ctx: &mut ScanContext,
    type_arg: i64,
    subtype_arg: i64,
) -> Option<i64> {
    let macho = ctx.module_output::<Macho>()?;

    // Ensure nfat_arch is present
    let nfat = macho.nfat_arch?;

    // Iterate over fat_arch up to nfat entries
    for i in 0..nfat as usize {
        if let Some(arch) = macho.fat_arch.get(i) {
            if let (Some(cputype), Some(cpusubtype)) =
                (arch.cputype, arch.cpusubtype)
            {
                if cputype as i64 == type_arg
                    && cpusubtype as i64 == subtype_arg
                {
                    let file_offset = arch.offset?;
                    let entry_point = macho.file.get(i)?.entry_point?;
                    return file_offset
                        .checked_add(entry_point)
                        .map(|sum| sum as i64);
                }
            }
        }
    }

    None
}

/// Returns true if the Mach-O parsed entitlements contain `entitlement`
///
/// `entitlement` is case-insensitive.
#[module_export]
fn has_entitlement(
    ctx: &ScanContext,
    entitlement: RuntimeString,
) -> Option<bool> {
    let macho = ctx.module_output::<Macho>()?;
    let expected = entitlement.as_bstr(ctx);

    for entitlement in macho.entitlements.iter() {
        if expected.eq_ignore_ascii_case(entitlement.as_bytes()) {
            return Some(true);
        }
    }

    for file in macho.file.iter() {
        for entitlement in file.entitlements.iter() {
            if expected.eq_ignore_ascii_case(entitlement.as_bytes()) {
                return Some(true);
            }
        }
    }

    Some(false)
}

/// Returns true if the Mach-O parsed dylibs contain `dylib_name`
///
/// `dylib_name` is case-insensitive.
#[module_export]
fn has_dylib(ctx: &ScanContext, dylib_name: RuntimeString) -> Option<bool> {
    let macho = ctx.module_output::<Macho>()?;
    let expected_name = dylib_name.as_bstr(ctx);

    for dylib in macho.dylibs.iter() {
        if dylib.name.as_ref().is_some_and(|name| {
            expected_name.eq_ignore_ascii_case(name.as_bytes())
        }) {
            return Some(true);
        }
    }

    for file in macho.file.iter() {
        for dylib in file.dylibs.iter() {
            if dylib.name.as_ref().is_some_and(|name| {
                expected_name.eq_ignore_ascii_case(name.as_bytes())
            }) {
                return Some(true);
            }
        }
    }

    Some(false)
}

/// Returns true if the Mach-O parsed rpaths contain `rpath`
///
/// `rpath` is case-insensitive.
#[module_export]
fn has_rpath(ctx: &ScanContext, rpath: RuntimeString) -> Option<bool> {
    let macho = ctx.module_output::<Macho>()?;
    let expected_rpath = rpath.as_bstr(ctx);

    for rp in macho.rpaths.iter() {
        if expected_rpath.eq_ignore_ascii_case(rp.as_bytes()) {
            return Some(true);
        }
    }

    for file in macho.file.iter() {
        for rp in file.rpaths.iter() {
            if expected_rpath.eq_ignore_ascii_case(rp.as_bytes()) {
                return Some(true);
            }
        }
    }

    Some(false)
}

/// Returns true if the Mach-O parsed imports contain `import`
///
/// `import` is case-insensitive
#[module_export]
fn has_import(ctx: &ScanContext, import: RuntimeString) -> Option<bool> {
    let macho = ctx.module_output::<Macho>()?;
    let expected_import = import.as_bstr(ctx);

    for im in macho.imports.iter() {
        if expected_import.eq_ignore_ascii_case(im.as_bytes()) {
            return Some(true);
        }
    }

    for file in macho.file.iter() {
        for im in file.imports.iter() {
            if expected_import.eq_ignore_ascii_case(im.as_bytes()) {
                return Some(true);
            }
        }
    }

    Some(false)
}

/// Returns true if the Mach-O parsed exports contain `export`
///
/// `export` is case-insensitive
#[module_export]
fn has_export(ctx: &ScanContext, export: RuntimeString) -> Option<bool> {
    let macho = ctx.module_output::<Macho>()?;
    let expected_export = export.as_bstr(ctx);

    for ex in macho.exports.iter() {
        if expected_export.eq_ignore_ascii_case(ex.as_bytes()) {
            return Some(true);
        }
    }

    for file in macho.file.iter() {
        for ex in file.exports.iter() {
            if expected_export.eq_ignore_ascii_case(ex.as_bytes()) {
                return Some(true);
            }
        }
    }

    Some(false)
}

/// Returns a md5 hash of the dylibs designated in the mach-o binary
#[module_export]
fn dylib_hash(ctx: &mut ScanContext) -> Option<RuntimeString> {
    let cached = DYLIB_MD5_CACHE.with(|cache| -> Option<RuntimeString> {
        cache
            .borrow()
            .as_deref()
            .map(|s| RuntimeString::from_slice(ctx, s.as_bytes()))
    });

    if cached.is_some() {
        return cached;
    }

    let macho = ctx.module_output::<Macho>()?;
    let mut dylibs_to_hash = &macho.dylibs;

    // if there are not any dylibs in the main Macho, the dylibs of the nested
    // file should be hashed
    if dylibs_to_hash.is_empty() && !macho.file.is_empty() {
        dylibs_to_hash = &macho.file[0].dylibs;
    }

    // we need to check again as the nested file dylibs could be empty too
    if dylibs_to_hash.is_empty() {
        return None;
    }

    let mut md5_hash = Md5::new();

    let dylibs_to_hash = dylibs_to_hash
        .iter()
        .filter_map(|dylib| {
            dylib
                .name
                .as_ref()
                .map(|name| BString::new(name.trim().to_lowercase()))
        })
        .unique()
        .sorted()
        .join(",");

    md5_hash.update(dylibs_to_hash.as_bytes());

    let digest = format!("{:x}", md5_hash.finalize());

    DYLIB_MD5_CACHE.with(|cache| {
        *cache.borrow_mut() = Some(digest.clone());
    });

    Some(RuntimeString::new(digest))
}

/// Returns a md5 hash of the entitlements designated in the mach-o binary
#[module_export]
fn entitlement_hash(ctx: &mut ScanContext) -> Option<RuntimeString> {
    let cached =
        ENTITLEMENT_MD5_CACHE.with(|cache| -> Option<RuntimeString> {
            cache
                .borrow()
                .as_deref()
                .map(|s| RuntimeString::from_slice(ctx, s.as_bytes()))
        });

    if cached.is_some() {
        return cached;
    }

    let macho = ctx.module_output::<Macho>()?;
    let mut entitlements_to_hash = &macho.entitlements;

    // if there are not any entitlements in the main Macho, the entitlements of the
    // nested file should be hashed
    if entitlements_to_hash.is_empty() && !macho.file.is_empty() {
        entitlements_to_hash = &macho.file[0].entitlements;
    }

    // we need to check again as the nested file entitlements could be empty too
    if entitlements_to_hash.is_empty() {
        return None;
    }

    let mut md5_hash = Md5::new();

    let entitlements_str: String = entitlements_to_hash
        .iter()
        .map(|e| e.trim().to_lowercase())
        .unique()
        .sorted()
        .join(",");

    md5_hash.update(entitlements_str.as_bytes());

    let digest = format!("{:x}", md5_hash.finalize());

    ENTITLEMENT_MD5_CACHE.with(|cache| {
        *cache.borrow_mut() = Some(digest.clone());
    });

    Some(RuntimeString::new(digest))
}

/// Returns a md5 hash of the export symbols in the mach-o binary
#[module_export]
fn export_hash(ctx: &mut ScanContext) -> Option<RuntimeString> {
    let cached = EXPORT_MD5_CACHE.with(|cache| -> Option<RuntimeString> {
        cache
            .borrow()
            .as_deref()
            .map(|s| RuntimeString::from_slice(ctx, s.as_bytes()))
    });

    if cached.is_some() {
        return cached;
    }

    let macho = ctx.module_output::<Macho>()?;
    let mut exports_to_hash = &macho.exports;

    // if there are not any exports in the main Macho, the exports of the
    // nested file should be hashed
    if exports_to_hash.is_empty() && !macho.file.is_empty() {
        exports_to_hash = &macho.file[0].exports;
    }

    // we need to check again as the nested file exports could be empty too
    if exports_to_hash.is_empty() {
        return None;
    }

    let mut md5_hash = Md5::new();

    let exports_str: String = exports_to_hash
        .iter()
        .map(|e| e.trim().to_lowercase())
        .unique()
        .sorted()
        .join(",");

    md5_hash.update(exports_str.as_bytes());

    let digest = format!("{:x}", md5_hash.finalize());

    EXPORT_MD5_CACHE.with(|cache| {
        *cache.borrow_mut() = Some(digest.clone());
    });

    Some(RuntimeString::new(digest))
}

/// Returns a md5 hash of the imported symbols in the mach-o binary
#[module_export]
fn import_hash(ctx: &mut ScanContext) -> Option<RuntimeString> {
    let cached = IMPORT_MD5_CACHE.with(|cache| -> Option<RuntimeString> {
        cache
            .borrow()
            .as_deref()
            .map(|s| RuntimeString::from_slice(ctx, s.as_bytes()))
    });

    if cached.is_some() {
        return cached;
    }

    let macho = ctx.module_output::<Macho>()?;
    let mut imports_to_hash = &macho.imports;

    // if there are not any imports in the main Macho, the imports of the
    // nested file should be hashed
    if imports_to_hash.is_empty() && !macho.file.is_empty() {
        imports_to_hash = &macho.file[0].imports;
    }

    // we need to check again as the nested file imports could be empty too
    if imports_to_hash.is_empty() {
        return None;
    }

    let mut md5_hash = Md5::new();

    let imports_str: String = imports_to_hash
        .iter()
        .map(|e| e.trim().to_lowercase())
        .unique()
        .sorted()
        .join(",");

    md5_hash.update(imports_str.as_bytes());

    let digest = format!("{:x}", md5_hash.finalize());

    IMPORT_MD5_CACHE.with(|cache| {
        *cache.borrow_mut() = Some(digest.clone());
    });

    Some(RuntimeString::new(digest))
}

/// Returns a md5 hash of the symbol table in the mach-o binary
#[module_export]
fn sym_hash(ctx: &mut ScanContext) -> Option<RuntimeString> {
    let cached = SYM_MD5_CACHE.with(|cache| -> Option<RuntimeString> {
        cache
            .borrow()
            .as_deref()
            .map(|s| RuntimeString::from_slice(ctx, s.as_bytes()))
    });

    if cached.is_some() {
        return cached;
    }

    let macho = ctx.module_output::<Macho>()?;
    let mut symtab_to_hash = &macho.symtab.entries;

    // if there is not a symbtol table in the main Macho, the symbol table of the
    // nested file should be hashed
    if symtab_to_hash.is_empty() && !macho.file.is_empty() {
        symtab_to_hash = &macho.file[0].symtab.entries;
    }

    // we need to check again as the nested symbol table could be empty too
    if symtab_to_hash.is_empty() {
        return None;
    }

    let mut md5_hash: digest::core_api::CoreWrapper<md5::Md5Core> = Md5::new();

    let symtab_hash_entries = symtab_to_hash
        .iter()
        .map(|e| BString::new(e.trim().to_lowercase()))
        .unique()
        .sorted()
        .join(",");

    md5_hash.update(symtab_hash_entries);

    let digest = format!("{:x}", md5_hash.finalize());

    SYM_MD5_CACHE.with(|cache| {
        *cache.borrow_mut() = Some(digest.clone());
    });

    Some(RuntimeString::new(digest))
}

#[module_main]
fn main(data: &[u8], _meta: Option<&[u8]>) -> Result<Macho, ModuleError> {
    DYLIB_MD5_CACHE.with(|cache| *cache.borrow_mut() = None);
    ENTITLEMENT_MD5_CACHE.with(|cache| *cache.borrow_mut() = None);
    EXPORT_MD5_CACHE.with(|cache| *cache.borrow_mut() = None);
    IMPORT_MD5_CACHE.with(|cache| *cache.borrow_mut() = None);
    SYM_MD5_CACHE.with(|cache| *cache.borrow_mut() = None);

    match parser::MachO::parse(data) {
        Ok(macho) => Ok(macho.into()),
        Err(_) => Ok(Macho::new()),
    }
}
