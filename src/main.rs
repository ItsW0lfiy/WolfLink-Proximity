use std::collections::BTreeMap;
use std::env;
use std::ffi::c_void;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::mem::{size_of, zeroed};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

type Handle = *mut c_void;

const PROCESS_VM_READ: u32 = 0x0010;
const PROCESS_QUERY_INFORMATION: u32 = 0x0400;
const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
const INVALID_HANDLE_VALUE: Handle = usize::MAX as Handle;

const MEM_COMMIT: u32 = 0x1000;
const MEM_PRIVATE: u32 = 0x20000;
const PAGE_NOACCESS: u32 = 0x01;
const PAGE_READONLY: u32 = 0x02;
const PAGE_READWRITE: u32 = 0x04;
const PAGE_WRITECOPY: u32 = 0x08;
const PAGE_EXECUTE_READ: u32 = 0x20;
const PAGE_EXECUTE_READWRITE: u32 = 0x40;
const PAGE_EXECUTE_WRITECOPY: u32 = 0x80;
const PAGE_GUARD: u32 = 0x100;

const CHUNK_SIZE: usize = 4 * 1024 * 1024;
const CFRAME_BYTES: usize = 12 * 4;

// Current-client layout. Every RVA is validated against MSVC RTTI before use,
// and every position is required to match the Primitive's duplicate copy.
const HUMANOID_VTABLE_RVA: usize = 0x06cb_c560;
const PRIMITIVE_VTABLE_RVA: usize = 0x06ca_6f68;
const PART_VTABLE_RVA: usize = 0x06c2_98b0;
const MODEL_VTABLE_RVA: usize = 0x06d4_bf60;
const PLAYER_VTABLE_RVA: usize = 0x06d8_d170;
const HUMANOID_ROOT_PRIMITIVE_OFFSET: usize = 0x88;
const PRIMITIVE_POSITION_OFFSET: usize = 0xd4;
const PRIMITIVE_POSITION_COPY_OFFSET: usize = 0x11c;
const PART_SUBOBJECT_OFFSET: usize = 0x148;
const PART_PRIMITIVE_FIELD_OFFSET: usize = 0x178;
const PART_PARENT_MODEL_OFFSET: usize = 0x68;
const INSTANCE_NAME_OFFSET: usize = 0x70;

#[repr(C)]
#[derive(Clone, Copy)]
struct MemoryBasicInformation {
    base_address: *mut c_void,
    allocation_base: *mut c_void,
    allocation_protect: u32,
    partition_id: u16,
    _padding: u16,
    region_size: usize,
    state: u32,
    protect: u32,
    kind: u32,
    _padding2: u32,
}

#[repr(C)]
struct ProcessEntry32W {
    size: u32,
    usage: u32,
    process_id: u32,
    default_heap_id: usize,
    module_id: u32,
    threads: u32,
    parent_process_id: u32,
    priority_class_base: i32,
    flags: u32,
    exe_file: [u16; 260],
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> Handle;
    fn CloseHandle(handle: Handle) -> i32;
    fn ReadProcessMemory(
        process: Handle,
        base_address: *const c_void,
        buffer: *mut c_void,
        size: usize,
        bytes_read: *mut usize,
    ) -> i32;
    fn VirtualQueryEx(
        process: Handle,
        address: *const c_void,
        information: *mut MemoryBasicInformation,
        length: usize,
    ) -> usize;
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
    fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
    fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
}

struct ProcessHandle(Handle);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

#[derive(Clone)]
struct Region {
    base: usize,
    allocation_base: usize,
    size: usize,
    state: u32,
    protect: u32,
    kind: u32,
}

#[derive(Clone)]
struct Candidate {
    matrix_address: usize,
    position_address: usize,
    layout: &'static str,
    matrix: [f32; 9],
    position: [f32; 3],
}

fn usage() -> ! {
    eprintln!("Usage:");
    eprintln!("  roblox-memory-scanner survey [--pid PID]");
    eprintln!("  roblox-memory-scanner capture-cframes --out FILE [--pid PID] [--all-readable]");
    eprintln!("  roblox-memory-scanner compare-cframes --input FILE --out FILE [--pid PID]");
    eprintln!(
        "  roblox-memory-scanner find-vector --x X --y Y --z Z --out FILE [--pid PID] [--all-readable]"
    );
    eprintln!("  roblox-memory-scanner track-vectors --input FILE --out FILE [--pid PID]");
    eprintln!(
        "  roblox-memory-scanner classify-vector --input TRACKING_CSV --x X --y Y --z Z --out FILE [--pid PID]"
    );
    eprintln!(
        "  roblox-memory-scanner find-pointer-owners --target ADDRESS --out FILE [--pid PID] [--all-readable]"
    );
    eprintln!(
        "  roblox-memory-scanner inspect-object-strings --address ADDRESS [--bytes N] [--pid PID]"
    );
    eprintln!(
        "  roblox-memory-scanner inspect-object-pointers --address ADDRESS [--bytes N] [--pid PID]"
    );
    eprintln!("  roblox-memory-scanner locate-humanoid-roots [--pid PID]");
    std::process::exit(2);
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}

fn requested_pid(args: &[String]) -> Result<u32, String> {
    if let Some(raw) = arg_value(args, "--pid") {
        let pid = raw
            .parse::<u32>()
            .map_err(|_| format!("Invalid PID: {raw}"))?;
        let matches = roblox_processes()?;
        if !matches.iter().any(|(found, _)| *found == pid) {
            return Err(format!(
                "PID {pid} is not a live RobloxPlayerBeta.exe process"
            ));
        }
        return Ok(pid);
    }

    let matches = roblox_processes()?;
    match matches.as_slice() {
        [(pid, _)] => Ok(*pid),
        [] => Err("RobloxPlayerBeta.exe is not running".to_string()),
        _ => Err(format!(
            "Multiple RobloxPlayerBeta.exe processes found: {}. Pass --pid explicitly.",
            matches
                .iter()
                .map(|(pid, _)| pid.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn roblox_processes() -> Result<Vec<(u32, String)>, String> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err("CreateToolhelp32Snapshot failed".to_string());
    }
    let snapshot = ProcessHandle(snapshot);
    let mut entry: ProcessEntry32W = unsafe { zeroed() };
    entry.size = size_of::<ProcessEntry32W>() as u32;
    let mut found = Vec::new();
    let mut ok = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    while ok != 0 {
        let end = entry
            .exe_file
            .iter()
            .position(|ch| *ch == 0)
            .unwrap_or(entry.exe_file.len());
        let name = String::from_utf16_lossy(&entry.exe_file[..end]);
        if name.eq_ignore_ascii_case("RobloxPlayerBeta.exe") {
            found.push((entry.process_id, name));
        }
        ok = unsafe { Process32NextW(snapshot.0, &mut entry) };
    }
    Ok(found)
}

fn open_read_only(pid: u32) -> Result<ProcessHandle, String> {
    let access = PROCESS_QUERY_INFORMATION | PROCESS_VM_READ;
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        Err(format!(
            "OpenProcess({pid}) failed for read/query access (0x{access:04x})"
        ))
    } else {
        Ok(ProcessHandle(handle))
    }
}

fn regions(process: &ProcessHandle) -> Vec<Region> {
    let mut result = Vec::new();
    let mut address = 0usize;
    loop {
        let mut info: MemoryBasicInformation = unsafe { zeroed() };
        let queried = unsafe {
            VirtualQueryEx(
                process.0,
                address as *const c_void,
                &mut info,
                size_of::<MemoryBasicInformation>(),
            )
        };
        if queried == 0 {
            break;
        }
        let base = info.base_address as usize;
        result.push(Region {
            base,
            allocation_base: info.allocation_base as usize,
            size: info.region_size,
            state: info.state,
            protect: info.protect,
            kind: info.kind,
        });
        let next = base.saturating_add(info.region_size);
        if next <= address {
            break;
        }
        address = next;
    }
    result
}

fn is_readable(protect: u32) -> bool {
    if protect & (PAGE_GUARD | PAGE_NOACCESS) != 0 {
        return false;
    }
    matches!(
        protect & 0xff,
        PAGE_READONLY
            | PAGE_READWRITE
            | PAGE_WRITECOPY
            | PAGE_EXECUTE_READ
            | PAGE_EXECUTE_READWRITE
            | PAGE_EXECUTE_WRITECOPY
    )
}

fn is_writable(protect: u32) -> bool {
    matches!(
        protect & 0xff,
        PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
    )
}

fn read_exact(process: &ProcessHandle, address: usize, buffer: &mut [u8]) -> bool {
    let mut bytes_read = 0usize;
    let ok = unsafe {
        ReadProcessMemory(
            process.0,
            address as *const c_void,
            buffer.as_mut_ptr() as *mut c_void,
            buffer.len(),
            &mut bytes_read,
        )
    };
    ok != 0 && bytes_read == buffer.len()
}

fn read_f32(bytes: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn image_modules(process: &ProcessHandle) -> Vec<(usize, usize)> {
    let mut allocation_bases = BTreeMap::new();
    for region in regions(process) {
        if region.state == MEM_COMMIT && region.allocation_base != 0 {
            allocation_bases.insert(region.allocation_base, ());
        }
    }

    let mut images = Vec::new();
    let mut header = vec![0u8; 4096];
    for (base, _) in allocation_bases {
        if !read_exact(process, base, &mut header) || &header[..2] != b"MZ" {
            continue;
        }
        let pe_offset = read_u32(&header, 0x3c) as usize;
        if pe_offset
            .checked_add(84)
            .is_none_or(|end| end > header.len())
            || &header[pe_offset..pe_offset + 4] != b"PE\0\0"
        {
            continue;
        }
        let image_size = read_u32(&header, pe_offset + 80) as usize;
        if image_size >= 0x1000 {
            images.push((base, image_size));
        }
    }
    images
}

fn matrix_is_rotation(matrix: &[f32; 9]) -> bool {
    if matrix.iter().any(|v| !v.is_finite() || v.abs() > 1.05) {
        return false;
    }
    let norm = |i: usize| {
        matrix[i] * matrix[i] + matrix[i + 1] * matrix[i + 1] + matrix[i + 2] * matrix[i + 2]
    };
    let dot = |a: usize, b: usize| {
        matrix[a] * matrix[b] + matrix[a + 1] * matrix[b + 1] + matrix[a + 2] * matrix[b + 2]
    };
    if (norm(0) - 1.0).abs() > 0.035
        || (norm(3) - 1.0).abs() > 0.035
        || (norm(6) - 1.0).abs() > 0.035
        || dot(0, 3).abs() > 0.035
        || dot(0, 6).abs() > 0.035
        || dot(3, 6).abs() > 0.035
    {
        return false;
    }
    let determinant = matrix[0] * (matrix[4] * matrix[8] - matrix[5] * matrix[7])
        - matrix[1] * (matrix[3] * matrix[8] - matrix[5] * matrix[6])
        + matrix[2] * (matrix[3] * matrix[7] - matrix[4] * matrix[6]);
    (determinant - 1.0).abs() <= 0.08
}

fn position_is_plausible(position: &[f32; 3]) -> bool {
    position
        .iter()
        .all(|v| v.is_finite() && v.abs() <= 1_000_000.0)
        && position.iter().any(|v| v.abs() >= 1.0)
}

fn candidate_at(
    bytes: &[u8],
    offset: usize,
    absolute: usize,
    matrix_first: bool,
) -> Option<Candidate> {
    let mut values = [0.0f32; 12];
    for (index, value) in values.iter_mut().enumerate() {
        *value = read_f32(bytes, offset + index * 4);
    }
    let (matrix, position, matrix_address, position_address, layout) = if matrix_first {
        let matrix: [f32; 9] = values[..9].try_into().unwrap();
        let position: [f32; 3] = values[9..].try_into().unwrap();
        (matrix, position, absolute, absolute + 36, "matrix-position")
    } else {
        let position: [f32; 3] = values[..3].try_into().unwrap();
        let matrix: [f32; 9] = values[3..].try_into().unwrap();
        (matrix, position, absolute + 12, absolute, "position-matrix")
    };
    if matrix_is_rotation(&matrix) && position_is_plausible(&position) {
        Some(Candidate {
            matrix_address,
            position_address,
            layout,
            matrix,
            position,
        })
    } else {
        None
    }
}

fn survey(pid: u32) -> Result<(), String> {
    let process = open_read_only(pid)?;
    let all = regions(&process);
    let mut groups: BTreeMap<(u32, u32, u32), (usize, u128)> = BTreeMap::new();
    for region in &all {
        let entry = groups
            .entry((region.state, region.kind, region.protect))
            .or_insert((0, 0));
        entry.0 += 1;
        entry.1 += region.size as u128;
    }
    println!("Roblox PID: {pid}");
    println!("OpenProcess access: PROCESS_QUERY_INFORMATION | PROCESS_VM_READ (0x0410)");
    println!("regions,state,type,protect,MiB,readable,writable");
    for ((state, kind, protect), (count, bytes)) in groups {
        println!(
            "{count},0x{state:08x},0x{kind:08x},0x{protect:08x},{:.2},{},{}",
            bytes as f64 / 1_048_576.0,
            is_readable(protect),
            is_writable(protect)
        );
    }
    Ok(())
}

fn capture_cframes(pid: u32, output: PathBuf, all_readable: bool) -> Result<(), String> {
    let process = open_read_only(pid)?;
    let all = regions(&process);
    let file = File::create(&output).map_err(|e| format!("Create {}: {e}", output.display()))?;
    let mut writer = BufWriter::new(file);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    writeln!(writer, "# roblox-memory-scanner cframe snapshot v1").map_err(|e| e.to_string())?;
    writeln!(writer, "# pid={pid}").map_err(|e| e.to_string())?;
    writeln!(writer, "# unix_time={timestamp}").map_err(|e| e.to_string())?;
    writeln!(
        writer,
        "# scope={}",
        if all_readable {
            "all-readable"
        } else {
            "private-writable"
        }
    )
    .map_err(|e| e.to_string())?;
    writeln!(
        writer,
        "matrix_address,position_address,layout,m00,m01,m02,m10,m11,m12,m20,m21,m22,x,y,z"
    )
    .map_err(|e| e.to_string())?;

    let mut selected_bytes = 0u128;
    let mut read_bytes = 0u128;
    let mut read_failures = 0usize;
    let mut candidates = 0usize;
    let mut buffer = vec![0u8; CHUNK_SIZE + CFRAME_BYTES];

    for region in all {
        let selected = region.state == MEM_COMMIT
            && is_readable(region.protect)
            && (all_readable || (region.kind == MEM_PRIVATE && is_writable(region.protect)));
        if !selected {
            continue;
        }
        selected_bytes += region.size as u128;
        let mut offset = 0usize;
        while offset < region.size {
            let wanted = (region.size - offset).min(buffer.len());
            if wanted < CFRAME_BYTES {
                break;
            }
            let chunk = &mut buffer[..wanted];
            if !read_exact(&process, region.base + offset, chunk) {
                read_failures += 1;
                offset = offset.saturating_add(CHUNK_SIZE);
                continue;
            }
            read_bytes += wanted as u128;
            let scan_len = (region.size - offset).min(CHUNK_SIZE);
            let last_start = scan_len.min(wanted - CFRAME_BYTES);
            let mut local = 0usize;
            while local <= last_start {
                let absolute = region.base + offset + local;
                for matrix_first in [true, false] {
                    if let Some(candidate) = candidate_at(chunk, local, absolute, matrix_first) {
                        writeln!(
                            writer,
                            "0x{:016x},0x{:016x},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                            candidate.matrix_address,
                            candidate.position_address,
                            candidate.layout,
                            candidate.matrix[0],
                            candidate.matrix[1],
                            candidate.matrix[2],
                            candidate.matrix[3],
                            candidate.matrix[4],
                            candidate.matrix[5],
                            candidate.matrix[6],
                            candidate.matrix[7],
                            candidate.matrix[8],
                            candidate.position[0],
                            candidate.position[1],
                            candidate.position[2]
                        )
                        .map_err(|e| e.to_string())?;
                        candidates += 1;
                    }
                }
                local += 4;
            }
            offset = offset.saturating_add(CHUNK_SIZE);
        }
    }
    writer.flush().map_err(|e| e.to_string())?;
    eprintln!("Roblox PID: {pid}");
    eprintln!("Selected: {:.2} MiB", selected_bytes as f64 / 1_048_576.0);
    eprintln!("Read: {:.2} MiB", read_bytes as f64 / 1_048_576.0);
    eprintln!("Read failures: {read_failures}");
    eprintln!("CFrame candidates: {candidates}");
    eprintln!("Snapshot: {}", output.display());
    Ok(())
}

fn parse_address(raw: &str) -> Result<usize, String> {
    usize::from_str_radix(raw.trim_start_matches("0x"), 16)
        .map_err(|e| format!("Invalid address {raw}: {e}"))
}

fn required_f32(args: &[String], name: &str) -> Result<f32, String> {
    let raw = arg_value(args, name).unwrap_or_else(|| usage());
    raw.parse::<f32>()
        .map_err(|e| format!("Invalid {name} value {raw}: {e}"))
}

fn find_vector(
    pid: u32,
    output: PathBuf,
    wanted: [f32; 3],
    all_readable: bool,
) -> Result<(), String> {
    if !position_is_plausible(&wanted) {
        return Err("Requested vector is not a plausible finite world position".to_string());
    }
    let process = open_read_only(pid)?;
    let all = regions(&process);
    let file = File::create(&output).map_err(|e| format!("Create {}: {e}", output.display()))?;
    let mut writer = BufWriter::new(file);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    writeln!(writer, "# roblox-memory-scanner exact vector hits v1").map_err(|e| e.to_string())?;
    writeln!(writer, "# pid={pid}").map_err(|e| e.to_string())?;
    writeln!(writer, "# unix_time={timestamp}").map_err(|e| e.to_string())?;
    writeln!(
        writer,
        "# scope={}",
        if all_readable {
            "all-readable"
        } else {
            "private-writable"
        }
    )
    .map_err(|e| e.to_string())?;
    writeln!(writer, "address,x,y,z,region_base,region_size,protect,type")
        .map_err(|e| e.to_string())?;

    let mut pattern = [0u8; 12];
    for (index, value) in wanted.iter().enumerate() {
        pattern[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }

    let mut selected_bytes = 0u128;
    let mut read_bytes = 0u128;
    let mut read_failures = 0usize;
    let mut hits = 0usize;
    let mut buffer = vec![0u8; CHUNK_SIZE + 8];
    for region in all {
        let selected = region.state == MEM_COMMIT
            && is_readable(region.protect)
            && (all_readable || (region.kind == MEM_PRIVATE && is_writable(region.protect)));
        if !selected {
            continue;
        }
        selected_bytes += region.size as u128;
        let mut offset = 0usize;
        while offset < region.size {
            let wanted_bytes = (region.size - offset).min(buffer.len());
            if wanted_bytes < pattern.len() {
                break;
            }
            let chunk = &mut buffer[..wanted_bytes];
            if !read_exact(&process, region.base + offset, chunk) {
                read_failures += 1;
                offset = offset.saturating_add(CHUNK_SIZE);
                continue;
            }
            read_bytes += wanted_bytes as u128;
            let scan_len = (region.size - offset).min(CHUNK_SIZE);
            let last_start = scan_len.min(wanted_bytes - pattern.len());
            let mut local = 0usize;
            while local <= last_start {
                if chunk[local..local + pattern.len()] == pattern {
                    writeln!(
                        writer,
                        "0x{:016x},{},{},{},0x{:016x},{},0x{:08x},0x{:08x}",
                        region.base + offset + local,
                        wanted[0],
                        wanted[1],
                        wanted[2],
                        region.base,
                        region.size,
                        region.protect,
                        region.kind
                    )
                    .map_err(|e| e.to_string())?;
                    hits += 1;
                }
                local += 4;
            }
            offset = offset.saturating_add(CHUNK_SIZE);
        }
    }
    writer.flush().map_err(|e| e.to_string())?;
    eprintln!("Roblox PID: {pid}");
    eprintln!("Vector: {}, {}, {}", wanted[0], wanted[1], wanted[2]);
    eprintln!("Selected: {:.2} MiB", selected_bytes as f64 / 1_048_576.0);
    eprintln!("Read: {:.2} MiB", read_bytes as f64 / 1_048_576.0);
    eprintln!("Read failures: {read_failures}");
    eprintln!("Exact vector hits: {hits}");
    eprintln!("Hits: {}", output.display());
    Ok(())
}

fn track_vectors(pid: u32, input: PathBuf, output: PathBuf) -> Result<(), String> {
    let process = open_read_only(pid)?;
    let reader =
        BufReader::new(File::open(&input).map_err(|e| format!("Open {}: {e}", input.display()))?);
    let file = File::create(&output).map_err(|e| format!("Create {}: {e}", output.display()))?;
    let mut writer = BufWriter::new(file);
    writeln!(
        writer,
        "address,readable,plausible,changed,distance,old_x,old_y,old_z,new_x,new_y,new_z,dx,dy,dz"
    )
    .map_err(|e| e.to_string())?;

    let mut baseline_pid = None;
    let mut considered = 0usize;
    let mut readable = 0usize;
    let mut plausible = 0usize;
    let mut changed = 0usize;
    for line in reader.lines() {
        let line = line.map_err(|e| e.to_string())?;
        if let Some(raw) = line.strip_prefix("# pid=") {
            baseline_pid = raw.parse::<u32>().ok();
            continue;
        }
        if line.starts_with('#') || line.starts_with("address,") || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() != 8 {
            return Err(format!("Malformed vector-hit line: {line}"));
        }
        let address = parse_address(fields[0])?;
        let old = [
            fields[1].parse::<f32>().map_err(|e| e.to_string())?,
            fields[2].parse::<f32>().map_err(|e| e.to_string())?,
            fields[3].parse::<f32>().map_err(|e| e.to_string())?,
        ];
        considered += 1;
        let mut bytes = [0u8; 12];
        let was_readable = read_exact(&process, address, &mut bytes);
        if !was_readable {
            writeln!(writer, "0x{address:016x},false,false,false,,,,,,,,,,")
                .map_err(|e| e.to_string())?;
            continue;
        }
        readable += 1;
        let current = [
            read_f32(&bytes, 0),
            read_f32(&bytes, 4),
            read_f32(&bytes, 8),
        ];
        let is_plausible = position_is_plausible(&current);
        if is_plausible {
            plausible += 1;
        }
        let delta = [
            current[0] - old[0],
            current[1] - old[1],
            current[2] - old[2],
        ];
        let distance = (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt();
        let did_change = distance >= 0.001;
        if did_change {
            changed += 1;
        }
        writeln!(
            writer,
            "0x{address:016x},true,{is_plausible},{did_change},{distance},{},{},{},{},{},{},{},{},{}",
            old[0], old[1], old[2], current[0], current[1], current[2], delta[0], delta[1], delta[2]
        )
        .map_err(|e| e.to_string())?;
    }
    writer.flush().map_err(|e| e.to_string())?;
    println!("Roblox PID: {pid}");
    if let Some(snapshot_pid) = baseline_pid {
        println!("Snapshot PID: {snapshot_pid}");
        if snapshot_pid != pid {
            println!(
                "WARNING: snapshot PID differs; addresses are not expected to remain valid across runs"
            );
        }
    }
    println!("Tracked addresses: {considered}");
    println!("Readable now: {readable}");
    println!("Plausible vectors now: {plausible}");
    println!("Changed by at least 0.001: {changed}");
    println!("Tracking result: {}", output.display());
    Ok(())
}

fn rtti_type_name(
    process: &ProcessHandle,
    module_base: usize,
    module_size: usize,
    vtable: usize,
) -> Option<String> {
    let module_end = module_base.checked_add(module_size)?;
    if vtable < module_base + 8 || vtable >= module_end {
        return None;
    }
    let mut pointer_bytes = [0u8; 8];
    if !read_exact(process, vtable - 8, &mut pointer_bytes) {
        return None;
    }
    let locator = read_u64(&pointer_bytes, 0) as usize;
    if locator < module_base || locator.checked_add(24)? > module_end {
        return None;
    }
    let mut locator_bytes = [0u8; 24];
    if !read_exact(process, locator, &mut locator_bytes) || read_u32(&locator_bytes, 0) != 1 {
        return None;
    }
    let type_rva = read_u32(&locator_bytes, 12) as usize;
    let self_rva = read_u32(&locator_bytes, 20) as usize;
    if module_base.checked_add(self_rva)? != locator || type_rva >= module_size {
        return None;
    }
    let name_address = module_base.checked_add(type_rva)?.checked_add(16)?;
    if name_address >= module_end {
        return None;
    }
    let mut name_bytes = [0u8; 256];
    if !read_exact(process, name_address, &mut name_bytes) {
        return None;
    }
    let length = name_bytes.iter().position(|byte| *byte == 0)?;
    let name = std::str::from_utf8(&name_bytes[..length]).ok()?;
    if !name.starts_with(".?A") || !name.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        return None;
    }
    Some(name.replace(',', "_"))
}

fn classify_vector(
    pid: u32,
    input: PathBuf,
    output: PathBuf,
    wanted: [f32; 3],
) -> Result<(), String> {
    let process = open_read_only(pid)?;
    let modules = image_modules(&process);
    if modules.is_empty() {
        return Err(
            "No readable PE image headers were found in the process memory map".to_string(),
        );
    }
    let mut largest_modules = modules.clone();
    largest_modules.sort_by(|left, right| right.1.cmp(&left.1));
    let reader =
        BufReader::new(File::open(&input).map_err(|e| format!("Open {}: {e}", input.display()))?);
    let file = File::create(&output).map_err(|e| format!("Create {}: {e}", output.display()))?;
    let mut writer = BufWriter::new(file);
    writeln!(
        writer,
        "vector_address,object_base,position_offset,vtable,module_base,vtable_rva,type_name"
    )
    .map_err(|e| e.to_string())?;

    let wanted_bits = [
        wanted[0].to_bits(),
        wanted[1].to_bits(),
        wanted[2].to_bits(),
    ];
    let mut matching_vectors = 0usize;
    let mut classified = 0usize;
    for line in reader.lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.starts_with('#') || line.starts_with("address,") || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() != 14 || fields[1] != "true" || fields[2] != "true" {
            continue;
        }
        let current = [
            fields[8].parse::<f32>().map_err(|e| e.to_string())?,
            fields[9].parse::<f32>().map_err(|e| e.to_string())?,
            fields[10].parse::<f32>().map_err(|e| e.to_string())?,
        ];
        if [
            current[0].to_bits(),
            current[1].to_bits(),
            current[2].to_bits(),
        ] != wanted_bits
        {
            continue;
        }
        matching_vectors += 1;
        let vector_address = parse_address(fields[0])?;
        let first_base = vector_address.saturating_sub(0x400) & !7usize;
        let last_base = vector_address & !7usize;
        let mut object_base = first_base;
        while object_base <= last_base {
            let mut pointer_bytes = [0u8; 8];
            if read_exact(&process, object_base, &mut pointer_bytes) {
                let vtable = read_u64(&pointer_bytes, 0) as usize;
                if let Some((module_base, module_size)) = modules
                    .iter()
                    .copied()
                    .find(|(base, size)| vtable >= *base && vtable < base.saturating_add(*size))
                {
                    if let Some(type_name) =
                        rtti_type_name(&process, module_base, module_size, vtable)
                    {
                        writeln!(
                            writer,
                            "0x{vector_address:016x},0x{object_base:016x},{},0x{vtable:016x},0x{module_base:016x},0x{:x},{}",
                            vector_address - object_base,
                            vtable - module_base,
                            type_name
                        )
                        .map_err(|e| e.to_string())?;
                        classified += 1;
                    }
                }
            }
            object_base = match object_base.checked_add(8) {
                Some(next) => next,
                None => break,
            };
        }
    }
    writer.flush().map_err(|e| e.to_string())?;
    println!("Roblox PID: {pid}");
    println!("Readable PE images: {}", modules.len());
    for (base, size) in largest_modules.iter().take(5) {
        println!("PE image: 0x{base:016x} ({size} bytes)");
    }
    println!("Matching vectors: {matching_vectors}");
    println!("Valid enclosing RTTI objects: {classified}");
    println!("Classification: {}", output.display());
    Ok(())
}

fn find_pointer_owners(
    pid: u32,
    output: PathBuf,
    target: usize,
    all_readable: bool,
) -> Result<(), String> {
    let process = open_read_only(pid)?;
    let modules = image_modules(&process);
    if modules.is_empty() {
        return Err(
            "No readable PE image headers were found in the process memory map".to_string(),
        );
    }
    let pattern = target.to_le_bytes();
    let mut hits = Vec::new();
    let mut selected_bytes = 0u128;
    let mut read_bytes = 0u128;
    let mut read_failures = 0usize;
    let mut buffer = vec![0u8; CHUNK_SIZE + 7];

    for region in regions(&process) {
        let selected = region.state == MEM_COMMIT
            && is_readable(region.protect)
            && (all_readable || (region.kind == MEM_PRIVATE && is_writable(region.protect)));
        if !selected {
            continue;
        }
        selected_bytes += region.size as u128;
        let mut offset = 0usize;
        while offset < region.size {
            let wanted_bytes = (region.size - offset).min(buffer.len());
            if wanted_bytes < pattern.len() {
                break;
            }
            let chunk = &mut buffer[..wanted_bytes];
            if !read_exact(&process, region.base + offset, chunk) {
                read_failures += 1;
                offset = offset.saturating_add(CHUNK_SIZE);
                continue;
            }
            read_bytes += wanted_bytes as u128;
            let scan_len = (region.size - offset).min(CHUNK_SIZE);
            let last_start = scan_len.min(wanted_bytes - pattern.len());
            let mut local = 0usize;
            while local <= last_start {
                if chunk[local..local + pattern.len()] == pattern {
                    hits.push(region.base + offset + local);
                }
                local += 8;
            }
            offset = offset.saturating_add(CHUNK_SIZE);
        }
    }

    let file = File::create(&output).map_err(|e| format!("Create {}: {e}", output.display()))?;
    let mut writer = BufWriter::new(file);
    writeln!(
        writer,
        "pointer_address,target,object_base,field_offset,vtable,module_base,vtable_rva,type_name"
    )
    .map_err(|e| e.to_string())?;
    let mut classified = 0usize;
    for pointer_address in &hits {
        let first_base = pointer_address.saturating_sub(0x800) & !7usize;
        let last_base = *pointer_address & !7usize;
        let mut object_base = first_base;
        while object_base <= last_base {
            let mut pointer_bytes = [0u8; 8];
            if read_exact(&process, object_base, &mut pointer_bytes) {
                let vtable = read_u64(&pointer_bytes, 0) as usize;
                if let Some((module_base, module_size)) = modules
                    .iter()
                    .copied()
                    .find(|(base, size)| vtable >= *base && vtable < base.saturating_add(*size))
                {
                    if let Some(type_name) =
                        rtti_type_name(&process, module_base, module_size, vtable)
                    {
                        writeln!(
                            writer,
                            "0x{pointer_address:016x},0x{target:016x},0x{object_base:016x},{},0x{vtable:016x},0x{module_base:016x},0x{:x},{}",
                            pointer_address - object_base,
                            vtable - module_base,
                            type_name
                        )
                        .map_err(|e| e.to_string())?;
                        classified += 1;
                    }
                }
            }
            object_base = match object_base.checked_add(8) {
                Some(next) => next,
                None => break,
            };
        }
    }
    writer.flush().map_err(|e| e.to_string())?;
    println!("Roblox PID: {pid}");
    println!("Target pointer: 0x{target:016x}");
    println!("Selected: {:.2} MiB", selected_bytes as f64 / 1_048_576.0);
    println!("Read: {:.2} MiB", read_bytes as f64 / 1_048_576.0);
    println!("Read failures: {read_failures}");
    println!("Pointer hits: {}", hits.len());
    println!("Valid enclosing RTTI objects: {classified}");
    println!("Owners: {}", output.display());
    Ok(())
}

fn inspect_object_strings(pid: u32, address: usize, byte_count: usize) -> Result<(), String> {
    if byte_count == 0 || byte_count > 4096 {
        return Err("--bytes must be between 1 and 4096".to_string());
    }
    let process = open_read_only(pid)?;
    let mut object = vec![0u8; byte_count];
    if !read_exact(&process, address, &mut object) {
        return Err(format!(
            "Could not read {byte_count} bytes from 0x{address:016x}"
        ));
    }
    println!("Roblox PID: {pid}");
    println!("Object: 0x{address:016x} ({byte_count} bytes)");
    println!("field_offset,pointer,string_offset,string");
    let mut target = [0u8; 256];
    for field_offset in (0..byte_count.saturating_sub(7)).step_by(8) {
        let pointer = read_u64(&object, field_offset) as usize;
        if pointer < 0x1_0000 || pointer >= 0x0000_8000_0000_0000 {
            continue;
        }
        if !read_exact(&process, pointer, &mut target) {
            continue;
        }
        let mut cursor = 0usize;
        while cursor < target.len() {
            while cursor < target.len() && !(0x20..=0x7e).contains(&target[cursor]) {
                cursor += 1;
            }
            let start = cursor;
            while cursor < target.len() && (0x20..=0x7e).contains(&target[cursor]) {
                cursor += 1;
            }
            if cursor.saturating_sub(start) >= 4 {
                let value = String::from_utf8_lossy(&target[start..cursor]).replace(',', "_");
                println!("0x{field_offset:x},0x{pointer:016x},0x{start:x},{value}");
            }
        }
    }
    Ok(())
}

fn inspect_object_pointers(pid: u32, address: usize, byte_count: usize) -> Result<(), String> {
    if byte_count == 0 || byte_count > 4096 {
        return Err("--bytes must be between 1 and 4096".to_string());
    }
    let process = open_read_only(pid)?;
    let modules = image_modules(&process);
    let mut object = vec![0u8; byte_count];
    if !read_exact(&process, address, &mut object) {
        return Err(format!(
            "Could not read {byte_count} bytes from 0x{address:016x}"
        ));
    }
    println!("Roblox PID: {pid}");
    println!("Object: 0x{address:016x} ({byte_count} bytes)");
    println!("field_offset,pointer,target_subobject_offset,vtable_rva,type_name");
    for field_offset in (0..byte_count.saturating_sub(7)).step_by(8) {
        let pointer = read_u64(&object, field_offset) as usize;
        if pointer < 0x1_0000 || pointer >= 0x0000_8000_0000_0000 {
            continue;
        }
        for target_offset in (0..=0x200usize).step_by(8) {
            let mut pointer_bytes = [0u8; 8];
            if !read_exact(
                &process,
                pointer.saturating_add(target_offset),
                &mut pointer_bytes,
            ) {
                continue;
            }
            let vtable = read_u64(&pointer_bytes, 0) as usize;
            if let Some((module_base, module_size)) = modules
                .iter()
                .copied()
                .find(|(base, size)| vtable >= *base && vtable < base.saturating_add(*size))
            {
                if let Some(type_name) = rtti_type_name(&process, module_base, module_size, vtable)
                {
                    println!(
                        "0x{field_offset:x},0x{pointer:016x},0x{target_offset:x},0x{:x},{}",
                        vtable - module_base,
                        type_name
                    );
                }
            }
        }
    }
    Ok(())
}

fn scan_private_writable_targets(
    process: &ProcessHandle,
    targets: &[usize],
) -> (Vec<Vec<usize>>, usize) {
    let mut hits = vec![Vec::new(); targets.len()];
    let mut read_failures = 0usize;
    let mut buffer = vec![0u8; CHUNK_SIZE + 7];
    for region in regions(process) {
        if region.state != MEM_COMMIT
            || region.kind != MEM_PRIVATE
            || !is_readable(region.protect)
            || !is_writable(region.protect)
        {
            continue;
        }
        let mut offset = 0usize;
        while offset < region.size {
            let wanted_bytes = (region.size - offset).min(buffer.len());
            if wanted_bytes < 8 {
                break;
            }
            let chunk = &mut buffer[..wanted_bytes];
            if !read_exact(process, region.base + offset, chunk) {
                read_failures += 1;
                offset = offset.saturating_add(CHUNK_SIZE);
                continue;
            }
            let scan_len = (region.size - offset).min(CHUNK_SIZE);
            let last_start = scan_len.min(wanted_bytes - 8);
            let mut local = 0usize;
            while local <= last_start {
                let value = read_u64(chunk, local) as usize;
                for (target_index, target) in targets.iter().enumerate() {
                    if value == *target {
                        hits[target_index].push(region.base + offset + local);
                    }
                }
                local += 8;
            }
            offset = offset.saturating_add(CHUNK_SIZE);
        }
    }
    (hits, read_failures)
}

fn read_instance_name(process: &ProcessHandle, instance: usize) -> Option<String> {
    let mut pointer_bytes = [0u8; 8];
    if !read_exact(
        process,
        instance.checked_add(INSTANCE_NAME_OFFSET)?,
        &mut pointer_bytes,
    ) {
        return None;
    }
    let storage = read_u64(&pointer_bytes, 0) as usize;
    let mut header = [0u8; 40];
    if storage == 0 || !read_exact(process, storage, &mut header) {
        return None;
    }
    let length = read_u64(&header, 24) as usize;
    let capacity = read_u64(&header, 32) as usize;
    if length == 0 || length > 128 || capacity < length {
        return None;
    }
    let bytes = if capacity <= 15 {
        header[8..8 + length].to_vec()
    } else {
        let data = read_u64(&header, 8) as usize;
        let mut bytes = vec![0u8; length];
        if data == 0 || !read_exact(process, data, &mut bytes) {
            return None;
        }
        bytes
    };
    let name = String::from_utf8(bytes).ok()?;
    if name.chars().any(char::is_control) {
        None
    } else {
        Some(name)
    }
}

fn locate_humanoid_roots(pid: u32) -> Result<(), String> {
    let process = open_read_only(pid)?;
    let modules = image_modules(&process);
    let module = modules.into_iter().find(|(base, size)| {
        [
            (HUMANOID_VTABLE_RVA, ".?AVHumanoid@RBX@@"),
            (PRIMITIVE_VTABLE_RVA, ".?AVPrimitive@RBX@@"),
            (PART_VTABLE_RVA, ".?AVPart@RBX@@"),
            (MODEL_VTABLE_RVA, ".?AVModelInstance@RBX@@"),
            (PLAYER_VTABLE_RVA, ".?AVPlayer@RBX@@"),
        ]
        .iter()
        .all(|(rva, expected)| {
            rtti_type_name(&process, *base, *size, base.saturating_add(*rva)).as_deref()
                == Some(*expected)
        })
    });
    let Some((module_base, _module_size)) = module else {
        return Err(
            "This Roblox build does not match the validated Player/Model/Part/Humanoid/Primitive RTTI layout"
                .to_string(),
        );
    };
    let humanoid_vtable = module_base + HUMANOID_VTABLE_RVA;
    let primitive_vtable = module_base + PRIMITIVE_VTABLE_RVA;
    let part_vtable = module_base + PART_VTABLE_RVA;
    let model_vtable = module_base + MODEL_VTABLE_RVA;
    let player_vtable = module_base + PLAYER_VTABLE_RVA;
    let (object_hits, first_read_failures) =
        scan_private_writable_targets(&process, &[humanoid_vtable, player_vtable]);
    let humanoids = &object_hits[0];
    let players = &object_hits[1];

    let mut roots = Vec::new();
    for humanoid in humanoids.iter().copied() {
        let mut pointer_bytes = [0u8; 8];
        if !read_exact(
            &process,
            humanoid + HUMANOID_ROOT_PRIMITIVE_OFFSET,
            &mut pointer_bytes,
        ) {
            continue;
        }
        let primitive = read_u64(&pointer_bytes, 0) as usize;
        if primitive == 0 || !read_exact(&process, primitive, &mut pointer_bytes) {
            continue;
        }
        if read_u64(&pointer_bytes, 0) as usize != primitive_vtable {
            continue;
        }
        let mut first_bytes = [0u8; 12];
        let mut copy_bytes = [0u8; 12];
        if !read_exact(
            &process,
            primitive + PRIMITIVE_POSITION_OFFSET,
            &mut first_bytes,
        ) || !read_exact(
            &process,
            primitive + PRIMITIVE_POSITION_COPY_OFFSET,
            &mut copy_bytes,
        ) {
            continue;
        }
        if first_bytes != copy_bytes {
            continue;
        }
        let position = [
            read_f32(&first_bytes, 0),
            read_f32(&first_bytes, 4),
            read_f32(&first_bytes, 8),
        ];
        if position_is_plausible(&position) {
            roots.push((humanoid, primitive, position));
        }
    }

    let primitive_targets: Vec<usize> = roots.iter().map(|root| root.1).collect();
    let (primitive_references, second_read_failures) =
        scan_private_writable_targets(&process, &primitive_targets);
    let mut root_names = vec![None; roots.len()];
    for (root_index, references) in primitive_references.iter().enumerate() {
        for pointer_address in references {
            let Some(part) = pointer_address.checked_sub(PART_PRIMITIVE_FIELD_OFFSET) else {
                continue;
            };
            let mut pointer_bytes = [0u8; 8];
            if !read_exact(&process, part + PART_SUBOBJECT_OFFSET, &mut pointer_bytes)
                || read_u64(&pointer_bytes, 0) as usize != part_vtable
            {
                continue;
            }
            if !read_exact(
                &process,
                part + PART_PARENT_MODEL_OFFSET,
                &mut pointer_bytes,
            ) {
                continue;
            }
            let model = read_u64(&pointer_bytes, 0) as usize;
            if model == 0 || !read_exact(&process, model, &mut pointer_bytes) {
                continue;
            }
            if read_u64(&pointer_bytes, 0) as usize != model_vtable {
                continue;
            }
            if let Some(name) = read_instance_name(&process, model) {
                root_names[root_index] = Some(name);
                break;
            }
        }
    }

    let mut player_names: Vec<String> = players
        .iter()
        .filter_map(|player| read_instance_name(&process, *player))
        .collect();
    player_names.sort();
    player_names.dedup();
    let local_roots: Vec<usize> = root_names
        .iter()
        .enumerate()
        .filter_map(|(index, name)| {
            name.as_ref()
                .filter(|candidate| player_names.iter().any(|player| player == *candidate))
                .map(|_| index)
        })
        .collect();

    println!("Roblox PID: {pid}");
    println!("Validated module base: 0x{module_base:016x}");
    println!("Humanoid RTTI hits: {}", humanoids.len());
    println!("Player RTTI hits: {}", players.len());
    println!(
        "Player names: {}",
        if player_names.is_empty() {
            "<none>".to_string()
        } else {
            player_names.join(", ")
        }
    );
    println!(
        "Read failures: {}",
        first_read_failures + second_read_failures
    );
    println!("Validated Humanoid root Primitives: {}", roots.len());
    for (index, (humanoid, primitive, position)) in roots.iter().enumerate() {
        println!(
            "Root {}: model={} humanoid=0x{humanoid:016x} primitive=0x{primitive:016x} X={} Y={} Z={}",
            index + 1,
            root_names[index].as_deref().unwrap_or("<unknown>"),
            position[0],
            position[1],
            position[2]
        );
    }
    if local_roots.len() != 1 {
        return Err(format!(
            "Expected exactly one Humanoid root whose Model name matches a Player name; found {}",
            local_roots.len()
        ));
    }
    let local_index = local_roots[0];
    let local_name = root_names[local_index].as_deref().unwrap();
    let position = roots[local_index].2;
    println!();
    println!("Local player: {local_name}");
    println!("X: {}", position[0]);
    println!("Y: {}", position[1]);
    println!("Z: {}", position[2]);
    Ok(())
}

fn compare_cframes(pid: u32, input: PathBuf, output: PathBuf) -> Result<(), String> {
    let process = open_read_only(pid)?;
    let reader =
        BufReader::new(File::open(&input).map_err(|e| format!("Open {}: {e}", input.display()))?);
    let mut changed = Vec::new();
    let mut baseline_pid = None;
    let mut considered = 0usize;
    let mut unreadable = 0usize;
    let mut no_longer_cframe = 0usize;

    for line in reader.lines() {
        let line = line.map_err(|e| e.to_string())?;
        if let Some(raw) = line.strip_prefix("# pid=") {
            baseline_pid = raw.parse::<u32>().ok();
            continue;
        }
        if line.starts_with('#') || line.starts_with("matrix_address,") || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() != 15 {
            return Err(format!("Malformed snapshot line: {line}"));
        }
        let matrix_address = parse_address(fields[0])?;
        let position_address = parse_address(fields[1])?;
        let layout = fields[2].to_string();
        let baseline = [
            fields[12].parse::<f32>().map_err(|e| e.to_string())?,
            fields[13].parse::<f32>().map_err(|e| e.to_string())?,
            fields[14].parse::<f32>().map_err(|e| e.to_string())?,
        ];
        considered += 1;

        let mut matrix_bytes = [0u8; 36];
        let mut position_bytes = [0u8; 12];
        if !read_exact(&process, matrix_address, &mut matrix_bytes)
            || !read_exact(&process, position_address, &mut position_bytes)
        {
            unreadable += 1;
            continue;
        }
        let mut matrix = [0.0f32; 9];
        let mut current = [0.0f32; 3];
        for (index, value) in matrix.iter_mut().enumerate() {
            *value = read_f32(&matrix_bytes, index * 4);
        }
        for (index, value) in current.iter_mut().enumerate() {
            *value = read_f32(&position_bytes, index * 4);
        }
        if !matrix_is_rotation(&matrix) || !position_is_plausible(&current) {
            no_longer_cframe += 1;
            continue;
        }
        let delta = [
            current[0] - baseline[0],
            current[1] - baseline[1],
            current[2] - baseline[2],
        ];
        let distance = (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt();
        if (0.05..=2_000.0).contains(&distance) {
            changed.push((
                distance,
                position_address,
                matrix_address,
                layout,
                baseline,
                current,
                delta,
            ));
        }
    }
    changed.sort_by(|a, b| a.0.total_cmp(&b.0));

    let file = File::create(&output).map_err(|e| format!("Create {}: {e}", output.display()))?;
    let mut writer = BufWriter::new(file);
    writeln!(writer, "position_address,matrix_address,layout,distance,old_x,old_y,old_z,new_x,new_y,new_z,dx,dy,dz")
        .map_err(|e| e.to_string())?;
    for item in &changed {
        writeln!(
            writer,
            "0x{:016x},0x{:016x},{},{},{},{},{},{},{},{},{},{},{}",
            item.1,
            item.2,
            item.3,
            item.0,
            item.4[0],
            item.4[1],
            item.4[2],
            item.5[0],
            item.5[1],
            item.5[2],
            item.6[0],
            item.6[1],
            item.6[2]
        )
        .map_err(|e| e.to_string())?;
    }
    writer.flush().map_err(|e| e.to_string())?;

    println!("Roblox PID: {pid}");
    if let Some(snapshot_pid) = baseline_pid {
        println!("Snapshot PID: {snapshot_pid}");
        if snapshot_pid != pid {
            println!(
                "WARNING: snapshot PID differs; addresses are not expected to remain valid across runs"
            );
        }
    }
    println!("Baseline candidates: {considered}");
    println!("Unreadable now: {unreadable}");
    println!("No longer CFrame-like: {no_longer_cframe}");
    println!("Changed CFrames (0.05..2000 studs): {}", changed.len());
    println!("Diff: {}", output.display());
    Ok(())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    let Some(command) = args.get(1).map(String::as_str) else {
        usage();
    };
    let pid = requested_pid(&args)?;
    match command {
        "survey" => survey(pid),
        "capture-cframes" => {
            let output = arg_value(&args, "--out")
                .map(PathBuf::from)
                .unwrap_or_else(|| usage());
            capture_cframes(pid, output, args.iter().any(|arg| arg == "--all-readable"))
        }
        "compare-cframes" => {
            let input = arg_value(&args, "--input")
                .map(PathBuf::from)
                .unwrap_or_else(|| usage());
            let output = arg_value(&args, "--out")
                .map(PathBuf::from)
                .unwrap_or_else(|| usage());
            compare_cframes(pid, input, output)
        }
        "find-vector" => {
            let output = arg_value(&args, "--out")
                .map(PathBuf::from)
                .unwrap_or_else(|| usage());
            let wanted = [
                required_f32(&args, "--x")?,
                required_f32(&args, "--y")?,
                required_f32(&args, "--z")?,
            ];
            find_vector(
                pid,
                output,
                wanted,
                args.iter().any(|arg| arg == "--all-readable"),
            )
        }
        "track-vectors" => {
            let input = arg_value(&args, "--input")
                .map(PathBuf::from)
                .unwrap_or_else(|| usage());
            let output = arg_value(&args, "--out")
                .map(PathBuf::from)
                .unwrap_or_else(|| usage());
            track_vectors(pid, input, output)
        }
        "classify-vector" => {
            let input = arg_value(&args, "--input")
                .map(PathBuf::from)
                .unwrap_or_else(|| usage());
            let output = arg_value(&args, "--out")
                .map(PathBuf::from)
                .unwrap_or_else(|| usage());
            let wanted = [
                required_f32(&args, "--x")?,
                required_f32(&args, "--y")?,
                required_f32(&args, "--z")?,
            ];
            classify_vector(pid, input, output, wanted)
        }
        "find-pointer-owners" => {
            let output = arg_value(&args, "--out")
                .map(PathBuf::from)
                .unwrap_or_else(|| usage());
            let target = arg_value(&args, "--target")
                .map(|raw| parse_address(&raw))
                .unwrap_or_else(|| usage())?;
            find_pointer_owners(
                pid,
                output,
                target,
                args.iter().any(|arg| arg == "--all-readable"),
            )
        }
        "inspect-object-strings" => {
            let address = arg_value(&args, "--address")
                .map(|raw| parse_address(&raw))
                .unwrap_or_else(|| usage())?;
            let byte_count = match arg_value(&args, "--bytes") {
                Some(raw) => raw
                    .parse::<usize>()
                    .map_err(|e| format!("Invalid --bytes value {raw}: {e}"))?,
                None => 1024,
            };
            inspect_object_strings(pid, address, byte_count)
        }
        "inspect-object-pointers" => {
            let address = arg_value(&args, "--address")
                .map(|raw| parse_address(&raw))
                .unwrap_or_else(|| usage())?;
            let byte_count = match arg_value(&args, "--bytes") {
                Some(raw) => raw
                    .parse::<usize>()
                    .map_err(|e| format!("Invalid --bytes value {raw}: {e}"))?,
                None => 512,
            };
            inspect_object_pointers(pid, address, byte_count)
        }
        "locate-humanoid-roots" => locate_humanoid_roots(pid),
        _ => usage(),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("ERROR: {error}");
        std::process::exit(1);
    }
}
