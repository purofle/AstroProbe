//! PLT/GOT hook of libmain.so's own `dlopen` import.
//!
//! libmain's `NativeLoader.load()` dlopens libunity.so and then libil2cpp.so.
//! Redirecting libmain's GOT slot for `dlopen` gives us the libil2cpp handle
//! the moment it is loaded — long before libunity calls `il2cpp_init` — with
//! no polling and without changing who loads what, or in which order.
//!
//! Runs from `.init_array`: libmain.so is our DT_NEEDED parent and bionic
//! relocates the whole load group before running any constructor, so its GOT
//! is already bound (BIND_NOW) and RELRO-protected by the time we get here.
use crate::is_lib;
use std::{
    ffi::{c_char, c_int, c_void, CStr},
    panic::{catch_unwind, AssertUnwindSafe},
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

const DT_NULL: i64 = 0;
const DT_PLTRELSZ: i64 = 2;
const DT_STRTAB: i64 = 5;
const DT_SYMTAB: i64 = 6;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_STRSZ: i64 = 10;
const DT_JMPREL: i64 = 23;
const R_AARCH64_GLOB_DAT: u32 = 1025;
const R_AARCH64_JUMP_SLOT: u32 = 1026;

#[repr(C)]
struct Elf64Dyn {
    d_tag: i64,
    d_val: u64,
}
#[repr(C)]
struct Elf64Rela {
    r_offset: u64,
    r_info: u64,
    r_addend: i64,
}
#[repr(C)]
struct Elf64Sym {
    st_name: u32,
    st_info: u8,
    st_other: u8,
    st_shndx: u16,
    st_value: u64,
    st_size: u64,
}

type DlopenFn = unsafe extern "C" fn(*const c_char, c_int) -> *mut c_void;
static ORIG_DLOPEN: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

unsafe extern "C" fn dlopen_detour(filename: *const c_char, flags: c_int) -> *mut c_void {
    // bionic picks the linker namespace from the caller address; we live in
    // the same (classloader) namespace as libmain, so the lookup is unchanged
    let orig: DlopenFn = std::mem::transmute(ORIG_DLOPEN.load(Ordering::Acquire));
    let handle = orig(filename, flags);
    if !handle.is_null() && !filename.is_null() {
        let name = CStr::from_ptr(filename);
        let _ = catch_unwind(AssertUnwindSafe(|| crate::il2cpp::on_dlopen(name, handle)));
    }
    handle
}

struct Module {
    bias: usize,
    phdr: *const libc::Elf64_Phdr,
    phnum: usize,
}

unsafe extern "C" fn find_libmain(info: *mut libc::dl_phdr_info, _size: usize, data: *mut c_void) -> c_int {
    let info = &*info;
    if info.dlpi_name.is_null() || !is_lib(CStr::from_ptr(info.dlpi_name).to_bytes(), b"libmain.so") {
        return 0;
    }
    *(data as *mut Option<Module>) = Some(Module {
        bias: info.dlpi_addr as usize,
        phdr: info.dlpi_phdr,
        phnum: info.dlpi_phnum as usize,
    });
    1
}

/// Address of the GOT slot through which `m` calls `sym`, and whether that slot
/// sits inside PT_GNU_RELRO (read-only after relocation).
unsafe fn find_slot(m: &Module, sym: &[u8]) -> Result<(*mut *mut c_void, bool), String> {
    let phdrs = std::slice::from_raw_parts(m.phdr, m.phnum);
    let dynamic = phdrs.iter().find(|p| p.p_type == libc::PT_DYNAMIC).ok_or("no PT_DYNAMIC")?;

    // bionic does not rewrite d_ptr in place: every address below is a vaddr
    let (mut jmprel, mut pltrelsz, mut rela, mut relasz) = (0, 0, 0, 0);
    let (mut symtab, mut strtab, mut strsz) = (0, 0, 0);
    let mut d = (m.bias + dynamic.p_vaddr as usize) as *const Elf64Dyn;
    while (*d).d_tag != DT_NULL {
        let v = (*d).d_val as usize;
        match (*d).d_tag {
            DT_JMPREL => jmprel = v,
            DT_PLTRELSZ => pltrelsz = v,
            DT_RELA => rela = v,
            DT_RELASZ => relasz = v,
            DT_SYMTAB => symtab = v,
            DT_STRTAB => strtab = v,
            DT_STRSZ => strsz = v,
            _ => {}
        }
        d = d.add(1);
    }
    if symtab == 0 || strtab == 0 {
        return Err("no DT_SYMTAB/DT_STRTAB".into());
    }

    let symtab = (m.bias + symtab) as *const Elf64Sym;
    let strtab = (m.bias + strtab) as *const u8;
    let mut found = None;
    for (table, size) in [(jmprel, pltrelsz), (rela, relasz)] {
        if table == 0 {
            continue;
        }
        let relocs = std::slice::from_raw_parts(
            (m.bias + table) as *const Elf64Rela,
            size / std::mem::size_of::<Elf64Rela>(),
        );
        for r in relocs {
            let kind = r.r_info as u32;
            if kind != R_AARCH64_JUMP_SLOT && kind != R_AARCH64_GLOB_DAT {
                continue;
            }
            let name_off = (*symtab.add((r.r_info >> 32) as usize)).st_name as usize;
            if name_off >= strsz {
                continue;
            }
            if CStr::from_ptr(strtab.add(name_off) as *const c_char).to_bytes() == sym {
                found = Some(r.r_offset as usize);
                break;
            }
        }
        if found.is_some() {
            break;
        }
    }
    let off = found.ok_or_else(|| format!("no JUMP_SLOT/GLOB_DAT for {}", String::from_utf8_lossy(sym)))?;
    let relro = phdrs.iter().any(|p| {
        p.p_type == libc::PT_GNU_RELRO
            && (p.p_vaddr as usize..p.p_vaddr as usize + p.p_memsz as usize).contains(&off)
    });
    Ok(((m.bias + off) as *mut *mut c_void, relro))
}

/// Point libmain.so's `dlopen` GOT slot at `dlopen_detour`.
pub fn hook_libmain_dlopen() -> Result<(), String> {
    unsafe {
        let mut m: Option<Module> = None;
        libc::dl_iterate_phdr(Some(find_libmain), &mut m as *mut _ as *mut c_void);
        let m = m.ok_or("libmain.so not found in dl_iterate_phdr")?;
        let (slot, relro) = find_slot(&m, b"dlopen")?;

        let page = libc::sysconf(libc::_SC_PAGESIZE) as usize;
        let start = (slot as usize & !(page - 1)) as *mut c_void;
        if libc::mprotect(start, page, libc::PROT_READ | libc::PROT_WRITE) != 0 {
            return Err(format!("mprotect RW {start:p}: {}", std::io::Error::last_os_error()));
        }
        let cur = ptr::read_volatile(slot);
        ORIG_DLOPEN.store(cur, Ordering::Release);
        ptr::write_volatile(slot, dlopen_detour as *mut c_void);
        let prot = if relro { libc::PROT_READ } else { libc::PROT_READ | libc::PROT_WRITE };
        if libc::mprotect(start, page, prot) != 0 {
            log::warn!("mprotect restore {start:p}: {}", std::io::Error::last_os_error());
        }
        log::info!("libmain.so dlopen GOT slot {slot:p} (bias {:#x}, relro {relro}): {cur:p} -> detour", m.bias);
        Ok(())
    }
}
