use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

const CACHE_DOMAIN: &[u8] = b"mach-browser-bindgen-v5\0";
const FUNCTION_IDENTITIES_HEADER: &[u8] = b"mach-function-identities-v2\0";
type FunctionIdentity = [u8; 16];

#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct CacheState {
    fingerprint: String,
    layout_fingerprint: Option<String>,
    data_fingerprint: Option<String>,
    data_len: Option<usize>,
    data_passthrough: bool,
    expected: Vec<String>,
    #[serde(default)]
    code_patch: Option<CodePatchState>,
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct CodePatchState {
    non_code_layout_fingerprint: String,
    fixed_layout_fingerprint: String,
    function_identities_fingerprint: String,
    #[serde(default)]
    function_names_fingerprint: Option<String>,
    functions: Vec<Option<u32>>,
    element_functions: Vec<Option<u32>>,
    types: Vec<Option<u32>>,
    globals: Vec<Option<u32>>,
    memories: Vec<Option<u32>>,
    tables: Vec<Option<u32>>,
    elements: Vec<Option<u32>>,
    data: Vec<Option<u32>>,
    tags: Vec<Option<u32>>,
    emitted_function_imports: u32,
}

struct ModuleInfo {
    layout_fingerprint: String,
    non_code_layout_fingerprint: String,
    fixed_layout_fingerprint: String,
    data_fingerprint: Option<String>,
    data_range: Option<Range<usize>>,
    element_range: Option<Range<usize>>,
    code_range: Option<Range<usize>>,
}

struct RememberedBindgenSource {
    path: PathBuf,
    fingerprint: String,
    bytes: Arc<Vec<u8>>,
}

static REMEMBERED_BINDGEN_SOURCE: OnceLock<Mutex<Option<RememberedBindgenSource>>> =
    OnceLock::new();

fn remembered_bindgen_source(path: &Path, fingerprint: &str) -> Option<Arc<Vec<u8>>> {
    let remembered = REMEMBERED_BINDGEN_SOURCE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()?;
    let remembered = remembered.as_ref()?;
    (remembered.path == path && remembered.fingerprint == fingerprint)
        .then(|| Arc::clone(&remembered.bytes))
}

fn remember_bindgen_source(path: PathBuf, fingerprint: String, bytes: Vec<u8>) {
    if let Ok(mut remembered) = REMEMBERED_BINDGEN_SOURCE
        .get_or_init(|| Mutex::new(None))
        .lock()
    {
        *remembered = Some(RememberedBindgenSource {
            path,
            fingerprint,
            bytes: Arc::new(bytes),
        });
    }
}

pub(super) fn generate(input: &Path, output: &Path, name: &str) -> Result<(), String> {
    let fingerprint_started = Instant::now();
    let linked_bytes = fs::read(input)
        .map_err(|error| format!("cannot read browser module {}: {error}", input.display()))?;
    let name_range = custom_section_range(&linked_bytes, b"name")?;
    let mut hasher = cache_hasher(name);
    if let Some(range) = &name_range {
        hasher.update_rayon(&linked_bytes[..range.start]);
        hasher.update_rayon(&linked_bytes[range.end..]);
    } else {
        hasher.update_rayon(&linked_bytes);
    }
    let fingerprint = hasher.finalize().to_hex().to_string();
    let fingerprint_elapsed = fingerprint_started.elapsed();
    let cached = load_cache(output, name);
    if cached
        .as_ref()
        .is_some_and(|cached| cached.fingerprint == fingerprint && outputs_exist(output, cached))
    {
        if std::env::var_os("MACH_PROFILE_BUILD").is_some() {
            eprintln!(
                "  profile   wasm-bindgen cache hit in {:.0}ms",
                fingerprint_elapsed.as_secs_f64() * 1000.0,
            );
        }
        return Ok(());
    }
    let identity_started = Instant::now();
    let function_names_fingerprint = name_range.as_ref().map(|range| {
        let mut hasher = blake3::Hasher::new();
        hasher.update_rayon(&linked_bytes[range.clone()]);
        hasher.finalize().to_hex().to_string()
    });
    let function_identities = match cached
        .as_ref()
        .and_then(|cached| cached.code_patch.as_ref())
    {
        Some(indices)
            if indices.function_names_fingerprint.as_ref()
                == function_names_fingerprint.as_ref() =>
        {
            match load_function_identities(output, name, &indices.function_identities_fingerprint) {
                Some(identities) => Some(identities),
                None => extract_function_identities(&linked_bytes)?,
            }
        }
        _ => extract_function_identities(&linked_bytes)?,
    };
    let identity_elapsed = identity_started.elapsed();
    // Keep the linked module intact on the patch path. Removing the large name
    // section requires copying the module and is only useful to full bindgen.
    let mut bytes = linked_bytes;
    let layout_started = Instant::now();
    let module_info = module_info(&bytes, name).ok();
    let input_element_functions = element_function_items(&bytes).ok();
    let layout_elapsed = layout_started.elapsed();
    if std::env::var_os("MACH_PROFILE_BUILD").is_some() {
        eprintln!(
            "  profile   wasm identities {:.0}ms / layout {:.0}ms",
            identity_elapsed.as_secs_f64() * 1000.0,
            layout_elapsed.as_secs_f64() * 1000.0,
        );
    }
    let patch_started = Instant::now();
    if let (Some(cached), Some(module_info)) = (&cached, &module_info) {
        let same_layout =
            cached.layout_fingerprint.as_deref() == Some(module_info.layout_fingerprint.as_str());
        let same_data_len = cached.data_len
            == module_info
                .data_range
                .as_ref()
                .map(|range| range.end - range.start);
        if same_layout
            && same_data_len
            && cached.data_passthrough
            && outputs_exist(output, cached)
            && patch_data_output(output, name, cached, &bytes, module_info)?
        {
            let mut state = refreshed_state(cached, fingerprint, module_info);
            if let (Some(code_patch), Some(identities)) =
                (&mut state.code_patch, function_identities.as_deref())
            {
                code_patch.function_identities_fingerprint =
                    function_identities_fingerprint(identities);
                code_patch.function_names_fingerprint = function_names_fingerprint.clone();
            }
            record_cache(output, name, &state);
            record_bindgen_source(input, output, name);
            record_function_identities(output, name, function_identities.as_deref());
            remember_bindgen_source(
                bindgen_source(output, name),
                state.fingerprint.clone(),
                bytes,
            );
            if std::env::var_os("MACH_PROFILE_BUILD").is_some() {
                eprintln!(
                    "  profile   wasm-bindgen section patch in {:.0}ms",
                    patch_started.elapsed().as_secs_f64() * 1000.0,
                );
            }
            return Ok(());
        }
        if cached.data_passthrough && outputs_exist(output, cached) {
            if let Some(code_patch) = patch_code_output(
                output,
                name,
                cached,
                &bytes,
                function_identities.as_deref(),
                function_names_fingerprint.as_deref(),
                module_info,
            )? {
                let mut state = refreshed_state(cached, fingerprint, module_info);
                state.code_patch = Some(code_patch);
                record_cache(output, name, &state);
                record_bindgen_source(input, output, name);
                record_function_identities(output, name, function_identities.as_deref());
                remember_bindgen_source(
                    bindgen_source(output, name),
                    state.fingerprint.clone(),
                    bytes,
                );
                if std::env::var_os("MACH_PROFILE_BUILD").is_some() {
                    eprintln!(
                        "  profile   wasm-bindgen code patch in {:.0}ms",
                        patch_started.elapsed().as_secs_f64() * 1000.0,
                    );
                }
                return Ok(());
            }
        }
    }

    record_bindgen_source(input, output, name);
    bytes = strip_section(bytes, name_range);
    let mut bindgen = wasm_bindgen_cli_support::Bindgen::new();
    bindgen
        .web(true)
        .map_err(|error| format!("cannot configure browser bindings: {error}"))?
        .input_bytes(name, bytes)
        .out_name(name)
        .typescript(false)
        .demangle(false)
        .remove_name_section(true)
        .remove_producers_section(true)
        .omit_default_module_path(false);
    let transform_started = Instant::now();
    let mut generated = bindgen
        .generate_output()
        .map_err(|error| format!("cannot generate browser bindings: {error}"))?;
    drop(bindgen);
    let transform_elapsed = transform_started.elapsed();
    let expected = expected_outputs(&generated, name);
    let emit_started = Instant::now();
    generated
        .emit(output)
        .map_err(|error| format!("cannot emit browser bindings: {error}"))?;
    let data_passthrough = module_info
        .as_ref()
        .is_some_and(|module_info| output_data_matches_input(output, name, module_info));
    let code_patch = module_info.as_ref().and_then(|module_info| {
        let indices = generated.wasm().original_to_emitted_indices()?;
        Some(CodePatchState {
            non_code_layout_fingerprint: module_info.non_code_layout_fingerprint.clone(),
            fixed_layout_fingerprint: module_info.fixed_layout_fingerprint.clone(),
            function_identities_fingerprint: function_identities
                .as_deref()
                .map(function_identities_fingerprint)?,
            function_names_fingerprint: function_names_fingerprint.clone(),
            functions: indices.functions.clone(),
            element_functions: element_function_map(
                input_element_functions.as_deref()?,
                &element_function_items(&fs::read(output.join(format!("{name}_bg.wasm"))).ok()?)
                    .ok()?,
                &indices.functions,
            )?,
            types: indices.types.clone(),
            globals: indices.globals.clone(),
            memories: indices.memories.clone(),
            tables: indices.tables.clone(),
            elements: indices.elements.clone(),
            data: indices.data.clone(),
            tags: indices.tags.clone(),
            emitted_function_imports: indices.emitted_function_imports,
        })
    });
    let state = CacheState {
        fingerprint,
        layout_fingerprint: module_info
            .as_ref()
            .map(|module_info| module_info.layout_fingerprint.clone()),
        data_fingerprint: module_info
            .as_ref()
            .and_then(|module_info| module_info.data_fingerprint.clone()),
        data_len: module_info.as_ref().and_then(|module_info| {
            module_info
                .data_range
                .as_ref()
                .map(|range| range.end - range.start)
        }),
        data_passthrough,
        expected,
        code_patch,
    };
    record_cache(output, name, &state);
    record_function_identities(output, name, function_identities.as_deref());
    if std::env::var_os("MACH_PROFILE_BUILD").is_some() {
        eprintln!(
            "  profile   wasm-bindgen hash {:.0}ms / transform {:.0}ms / emit {:.0}ms",
            fingerprint_elapsed.as_secs_f64() * 1000.0,
            transform_elapsed.as_secs_f64() * 1000.0,
            emit_started.elapsed().as_secs_f64() * 1000.0,
        );
    }
    Ok(())
}

fn cache_marker(output: &Path, name: &str) -> PathBuf {
    output.join(format!(".{name}.bindgen-cache"))
}

fn cache_hasher(name: &str) -> blake3::Hasher {
    let mut hasher = blake3::Hasher::new();
    hasher.update(CACHE_DOMAIN);
    hasher.update(crate::WASM_BINDGEN_VERSION.as_bytes());
    hasher.update(&[0]);
    hasher.update(name.as_bytes());
    hasher
}

fn strip_section(bytes: Vec<u8>, range: Option<Range<usize>>) -> Vec<u8> {
    let Some(name_range) = range else {
        return bytes;
    };
    let mut stripped = Vec::with_capacity(bytes.len() - name_range.len());
    stripped.extend_from_slice(&bytes[..name_range.start]);
    stripped.extend_from_slice(&bytes[name_range.end..]);
    stripped
}

fn custom_section_range(bytes: &[u8], wanted: &[u8]) -> Result<Option<Range<usize>>, String> {
    if bytes.get(..8) != Some(b"\0asm\x01\0\0\0") {
        return Err("browser module has an invalid WebAssembly header".to_owned());
    }
    let mut cursor = 8;
    while cursor < bytes.len() {
        let section_start = cursor;
        let id = bytes[cursor];
        cursor += 1;
        let size = read_uleb_u32(bytes, &mut cursor)? as usize;
        let section_end = cursor
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| "browser module has a truncated section".to_owned())?;
        if id == 0 {
            let name_len = read_uleb_u32(bytes, &mut cursor)? as usize;
            let name_end = cursor
                .checked_add(name_len)
                .filter(|end| *end <= section_end)
                .ok_or_else(|| "browser module has a truncated custom section name".to_owned())?;
            if &bytes[cursor..name_end] == wanted {
                return Ok(Some(section_start..section_end));
            }
        }
        cursor = section_end;
    }
    Ok(None)
}

fn extract_function_identities(bytes: &[u8]) -> Result<Option<Vec<FunctionIdentity>>, String> {
    let Some(range) = custom_section_range(bytes, b"name")? else {
        return Ok(None);
    };
    let mut cursor = range.start + 1;
    let payload_len = read_uleb_u32(bytes, &mut cursor)? as usize;
    if cursor.checked_add(payload_len) != Some(range.end) {
        return Err("browser name section has an invalid length".to_owned());
    }
    let section_name_len = read_uleb_u32(bytes, &mut cursor)? as usize;
    cursor = cursor
        .checked_add(section_name_len)
        .filter(|end| *end <= range.end)
        .ok_or_else(|| "browser name section is truncated".to_owned())?;

    let mut function_names = Vec::<Option<&[u8]>>::new();
    while cursor < range.end {
        let subsection_id = bytes[cursor];
        cursor += 1;
        let subsection_len = read_uleb_u32(bytes, &mut cursor)? as usize;
        let subsection_end = cursor
            .checked_add(subsection_len)
            .filter(|end| *end <= range.end)
            .ok_or_else(|| "browser name subsection is truncated".to_owned())?;
        if subsection_id == 1 {
            let count = read_uleb_u32(bytes, &mut cursor)? as usize;
            for _ in 0..count {
                let index = read_uleb_u32(bytes, &mut cursor)? as usize;
                let name_len = read_uleb_u32(bytes, &mut cursor)? as usize;
                let name_end = cursor
                    .checked_add(name_len)
                    .filter(|end| *end <= subsection_end)
                    .ok_or_else(|| "browser function name is truncated".to_owned())?;
                if function_names.len() <= index {
                    function_names.resize(index + 1, None);
                }
                function_names[index] = Some(&bytes[cursor..name_end]);
                cursor = name_end;
            }
            if cursor != subsection_end {
                return Err("browser function names contain trailing bytes".to_owned());
            }
        }
        cursor = subsection_end;
    }
    if function_names.is_empty() || function_names.iter().any(Option::is_none) {
        return Ok(None);
    }
    let function_names = function_names
        .into_iter()
        .map(Option::unwrap)
        .collect::<Vec<_>>();
    let mut identities = vec![[0_u8; 16]; function_names.len()];
    let workers = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(function_names.len().max(1));
    let chunk_len = function_names.len().div_ceil(workers);
    std::thread::scope(|scope| {
        for (names, output) in function_names
            .chunks(chunk_len)
            .zip(identities.chunks_mut(chunk_len))
        {
            scope.spawn(move || {
                for (name, identity) in names.iter().zip(output) {
                    let mut hasher = blake3::Hasher::new();
                    hasher.update(FUNCTION_IDENTITIES_HEADER);
                    hasher.update(name);
                    identity.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
                }
            });
        }
    });

    let mut occurrences = std::collections::HashMap::new();
    for identity in &mut identities {
        let base = *identity;
        let ordinal = occurrences.entry(base).or_insert(0_u32);
        if *ordinal != 0 {
            let mut hasher = blake3::Hasher::new();
            hasher.update(FUNCTION_IDENTITIES_HEADER);
            hasher.update(&base);
            hasher.update(&ordinal.to_le_bytes());
            identity.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
        }
        *ordinal += 1;
    }
    Ok(Some(identities))
}

fn function_identities_fingerprint(identities: &[FunctionIdentity]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(FUNCTION_IDENTITIES_HEADER);
    for identity in identities {
        hasher.update(identity);
    }
    hasher.finalize().to_hex().to_string()
}

fn load_cache(output: &Path, name: &str) -> Option<CacheState> {
    serde_json::from_str(&fs::read_to_string(cache_marker(output, name)).ok()?).ok()
}

fn outputs_exist(output: &Path, state: &CacheState) -> bool {
    !state.expected.is_empty()
        && state.expected.iter().all(|relative| {
            let path = Path::new(relative);
            path.components()
                .all(|component| matches!(component, Component::Normal(_)))
                && output.join(path).is_file()
        })
}

fn module_info(bytes: &[u8], name: &str) -> Result<ModuleInfo, String> {
    if bytes.get(..8) != Some(b"\0asm\x01\0\0\0") {
        return Err("browser module has an invalid WebAssembly header".to_owned());
    }
    let mut layout = cache_hasher(name);
    let mut non_code_layout = cache_hasher(name);
    let mut fixed_layout = cache_hasher(name);
    let mut data_range = None;
    let mut element_range = None;
    let mut code_range = None;
    let mut cursor = 8;
    while cursor < bytes.len() {
        let section_start = cursor;
        let id = bytes[cursor];
        cursor += 1;
        let size = read_uleb_u32(bytes, &mut cursor)? as usize;
        let payload_start = cursor;
        let section_end = payload_start
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| "browser module has a truncated section".to_owned())?;
        let custom_name = if id == 0 {
            let mut name_cursor = payload_start;
            let name_len = read_uleb_u32(bytes, &mut name_cursor)? as usize;
            let name_end = name_cursor
                .checked_add(name_len)
                .filter(|end| *end <= section_end)
                .ok_or_else(|| "browser module has a truncated custom section name".to_owned())?;
            Some(&bytes[name_cursor..name_end])
        } else {
            None
        };
        let ignored_custom = custom_name
            .is_some_and(|custom_name| custom_name == b"name" || custom_name == b"producers");
        if id == 11 {
            if data_range.is_some() {
                return Err("browser module contains more than one data section".to_owned());
            }
            data_range = Some(section_start..section_end);
        } else if !ignored_custom {
            if id == 9 {
                if element_range.is_some() {
                    return Err("browser module contains more than one element section".to_owned());
                }
                element_range = Some(section_start..section_end);
            }
            layout.update_rayon(&bytes[section_start..section_end]);
            if id == 10 {
                if code_range.is_some() {
                    return Err("browser module contains more than one code section".to_owned());
                }
                code_range = Some(section_start..section_end);
            } else {
                non_code_layout.update_rayon(&bytes[section_start..section_end]);
            }
            if !matches!(id, 3 | 9 | 10) {
                fixed_layout.update_rayon(&bytes[section_start..section_end]);
            }
        }
        cursor = section_end;
    }
    let data_fingerprint = data_range
        .as_ref()
        .map(|range| blake3::hash(&bytes[range.clone()]).to_hex().to_string());
    Ok(ModuleInfo {
        layout_fingerprint: layout.finalize().to_hex().to_string(),
        non_code_layout_fingerprint: non_code_layout.finalize().to_hex().to_string(),
        fixed_layout_fingerprint: fixed_layout.finalize().to_hex().to_string(),
        data_fingerprint,
        data_range,
        element_range,
        code_range,
    })
}

fn read_uleb_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, String> {
    let mut value = 0u32;
    for shift in (0..35).step_by(7) {
        let byte = *bytes
            .get(*cursor)
            .ok_or_else(|| "browser module has a truncated section size".to_owned())?;
        *cursor += 1;
        let payload = u32::from(byte & 0x7f);
        if shift == 28 && payload > 0x0f {
            return Err("browser module has an invalid section size".to_owned());
        }
        value |= payload << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("browser module has an invalid section size".to_owned())
}

fn refreshed_state(
    cached: &CacheState,
    fingerprint: String,
    module_info: &ModuleInfo,
) -> CacheState {
    CacheState {
        fingerprint,
        layout_fingerprint: Some(module_info.layout_fingerprint.clone()),
        data_fingerprint: module_info.data_fingerprint.clone(),
        data_len: module_info
            .data_range
            .as_ref()
            .map(|range| range.end - range.start),
        data_passthrough: true,
        expected: cached.expected.clone(),
        code_patch: cached.code_patch.clone(),
    }
}

fn bindgen_source(output: &Path, name: &str) -> PathBuf {
    output.join(format!(".{name}.bindgen-source.wasm"))
}

fn bindgen_function_identities(output: &Path, name: &str) -> PathBuf {
    output.join(format!(".{name}.bindgen-function-identities"))
}

fn encode_function_identities(identities: &[FunctionIdentity]) -> Option<Vec<u8>> {
    let count = u32::try_from(identities.len()).ok()?;
    let mut bytes =
        Vec::with_capacity(FUNCTION_IDENTITIES_HEADER.len() + 4 + identities.len() * 16);
    bytes.extend_from_slice(FUNCTION_IDENTITIES_HEADER);
    bytes.extend_from_slice(&count.to_le_bytes());
    for identity in identities {
        bytes.extend_from_slice(identity);
    }
    Some(bytes)
}

fn record_function_identities(output: &Path, name: &str, identities: Option<&[FunctionIdentity]>) {
    let Some(bytes) = identities.and_then(encode_function_identities) else {
        return;
    };
    let path = bindgen_function_identities(output, name);
    let candidate = path.with_file_name(format!(
        ".{name}.bindgen-function-identities.next-{}",
        std::process::id()
    ));
    if fs::write(&candidate, bytes).is_ok()
        && crate::project::activate_validated_file(&candidate, &path).is_err()
    {
        let _ = fs::remove_file(candidate);
    }
}

fn load_function_identities(
    output: &Path,
    name: &str,
    expected_fingerprint: &str,
) -> Option<Vec<FunctionIdentity>> {
    let bytes = fs::read(bindgen_function_identities(output, name)).ok()?;
    let payload = bytes.strip_prefix(FUNCTION_IDENTITIES_HEADER)?;
    let count = u32::from_le_bytes(payload.get(..4)?.try_into().ok()?) as usize;
    let raw = payload.get(4..)?;
    if raw.len() != count.checked_mul(16)? {
        return None;
    }
    let identities = raw
        .chunks_exact(16)
        .map(|chunk| chunk.try_into().ok())
        .collect::<Option<Vec<FunctionIdentity>>>()?;
    (function_identities_fingerprint(&identities) == expected_fingerprint).then_some(identities)
}

fn record_bindgen_source(input: &Path, output: &Path, name: &str) {
    let started = Instant::now();
    let path = bindgen_source(output, name);
    let candidate = path.with_file_name(format!(
        ".{name}.bindgen-source.next-{}",
        std::process::id()
    ));
    let _ = fs::remove_file(&candidate);
    if clone_or_copy_file(input, &candidate).is_ok()
        && crate::project::activate_validated_file(&candidate, &path).is_err()
    {
        let _ = fs::remove_file(candidate);
    }
    if std::env::var_os("MACH_PROFILE_BUILD").is_some() {
        eprintln!(
            "  profile   bindgen source snapshot in {:.0}ms",
            started.elapsed().as_secs_f64() * 1000.0,
        );
    }
}

#[cfg(target_os = "macos")]
fn clone_or_copy_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source_c = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let destination_c = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    if unsafe { libc::clonefile(source_c.as_ptr(), destination_c.as_ptr(), 0) } == 0 {
        return Ok(());
    }
    let _ = fs::remove_file(destination);
    fs::copy(source, destination)?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn clone_or_copy_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::copy(source, destination)?;
    Ok(())
}

fn clone_and_patch_file(
    source: &Path,
    destination: &Path,
    patches: &[(Range<usize>, &[u8])],
) -> std::io::Result<()> {
    let _ = fs::remove_file(destination);
    clone_or_copy_file(source, destination)?;
    let mut file = fs::OpenOptions::new().write(true).open(destination)?;
    for (range, bytes) in patches {
        file.seek(SeekFrom::Start(range.start as u64))?;
        file.write_all(bytes)?;
    }
    file.flush()
}

#[derive(Clone)]
struct CodeBody {
    entry: Range<usize>,
    body: Range<usize>,
}

fn code_bodies(
    bytes: &[u8],
    range: &Range<usize>,
) -> Result<(Range<usize>, Vec<CodeBody>), String> {
    let mut cursor = range
        .start
        .checked_add(1)
        .filter(|cursor| *cursor <= range.end)
        .ok_or_else(|| "browser module has an invalid code section".to_owned())?;
    let payload_len = read_uleb_u32(bytes, &mut cursor)? as usize;
    let payload_start = cursor;
    if payload_start.checked_add(payload_len) != Some(range.end) {
        return Err("browser module has an invalid code section length".to_owned());
    }
    let count = read_uleb_u32(bytes, &mut cursor)? as usize;
    let entries_start = cursor;
    let mut bodies = Vec::with_capacity(count);
    for _ in 0..count {
        let entry_start = cursor;
        let body_len = read_uleb_u32(bytes, &mut cursor)? as usize;
        let body_start = cursor;
        let body_end = body_start
            .checked_add(body_len)
            .filter(|end| *end <= range.end)
            .ok_or_else(|| "browser module has a truncated function body".to_owned())?;
        bodies.push(CodeBody {
            entry: entry_start..body_end,
            body: body_start..body_end,
        });
        cursor = body_end;
    }
    if cursor != range.end {
        return Err("browser module has trailing code section bytes".to_owned());
    }
    Ok((payload_start..entries_start, bodies))
}

fn source_fingerprint(bytes: &[u8], name: &str) -> String {
    let mut hasher = cache_hasher(name);
    if let Ok(Some(range)) = custom_section_range(bytes, b"name") {
        hasher.update_rayon(&bytes[..range.start]);
        hasher.update_rayon(&bytes[range.end..]);
    } else {
        hasher.update_rayon(bytes);
    }
    hasher.finalize().to_hex().to_string()
}

fn patchable_operator_pair(old: &wasmparser::Operator<'_>, new: &wasmparser::Operator<'_>) -> bool {
    matches!(
        (old, new),
        (
            wasmparser::Operator::I32Const { .. },
            wasmparser::Operator::I32Const { .. }
        ) | (
            wasmparser::Operator::I64Const { .. },
            wasmparser::Operator::I64Const { .. }
        ) | (
            wasmparser::Operator::F32Const { .. },
            wasmparser::Operator::F32Const { .. }
        ) | (
            wasmparser::Operator::F64Const { .. },
            wasmparser::Operator::F64Const { .. }
        ) | (
            wasmparser::Operator::V128Const { .. },
            wasmparser::Operator::V128Const { .. }
        ) | (
            wasmparser::Operator::LocalGet { .. },
            wasmparser::Operator::LocalGet { .. }
        ) | (
            wasmparser::Operator::LocalSet { .. },
            wasmparser::Operator::LocalSet { .. }
        ) | (
            wasmparser::Operator::LocalTee { .. },
            wasmparser::Operator::LocalTee { .. }
        )
    ) || memory_operand_shape(old).is_some_and(|old| Some(old) == memory_operand_shape(new))
}

#[allow(unused_mut, unused_assignments)]
fn memory_operand_shape(operator: &wasmparser::Operator<'_>) -> Option<String> {
    macro_rules! record {
        ($shape:ident, $has_memarg:ident, memarg, $value:ident) => {{
            $has_memarg = true;
            $shape.push_str(&format!(
                ":memarg:{:?}:{:?}:{}",
                $value.align, $value.max_align, $value.memory
            ));
        }};
        ($shape:ident, $has_memarg:ident, $field:ident, $value:ident) => {
            $shape.push_str(&format!(":{}:{:?}", stringify!($field), $value));
        };
    }
    macro_rules! match_operator {
        ($( @$proposal:ident $op:ident $({ $($field:ident: $field_type:ty),* })? => $visit:ident ($($ann:tt)*))*) => {
            match operator {
                $(
                    wasmparser::Operator::$op $( { $($field),* } )? => {
                        let mut shape = stringify!($op).to_owned();
                        let mut has_memarg = false;
                        $($(
                            record!(shape, has_memarg, $field, $field);
                        )*)?
                        has_memarg.then_some(shape)
                    }
                )*
                _ => unreachable!("wasmparser returned an unknown operator"),
            }
        };
    }
    wasmparser::for_each_operator!(match_operator)
}

fn push_reference(
    references: &mut Vec<String>,
    operator: &str,
    kind: &str,
    value: impl std::fmt::Debug,
) {
    references.push(format!("{operator}:{kind}:{value:?}"));
}

fn push_block_type_reference(
    references: &mut Vec<String>,
    operator: &str,
    block_type: &wasmparser::BlockType,
) {
    match block_type {
        wasmparser::BlockType::FuncType(index) => {
            push_reference(references, operator, "type", index)
        }
        wasmparser::BlockType::Type(ty) => push_val_type_reference(references, operator, ty),
        wasmparser::BlockType::Empty => {}
    }
}

fn push_val_type_reference(references: &mut Vec<String>, operator: &str, ty: &wasmparser::ValType) {
    if matches!(ty, wasmparser::ValType::Ref(_)) {
        push_reference(references, operator, "reference-type", ty);
    }
}

fn operator_references(operator: &wasmparser::Operator<'_>, references: &mut Vec<String>) {
    macro_rules! record {
        ($op:ident, function_index, $value:ident) => {
            push_reference(references, stringify!($op), "function", $value)
        };
        ($op:ident, global_index, $value:ident) => {
            push_reference(references, stringify!($op), "global", $value)
        };
        ($op:ident, tag_index, $value:ident) => {
            push_reference(references, stringify!($op), "tag", $value)
        };
        ($op:ident, table, $value:ident) => {
            push_reference(references, stringify!($op), "table", $value)
        };
        ($op:ident, table_index, $value:ident) => {
            push_reference(references, stringify!($op), "table", $value)
        };
        ($op:ident, dst_table, $value:ident) => {
            push_reference(references, stringify!($op), "table", $value)
        };
        ($op:ident, src_table, $value:ident) => {
            push_reference(references, stringify!($op), "table", $value)
        };
        ($op:ident, type_index, $value:ident) => {
            push_reference(references, stringify!($op), "type", $value)
        };
        ($op:ident, array_type_index, $value:ident) => {
            push_reference(references, stringify!($op), "type", $value)
        };
        ($op:ident, array_type_index_dst, $value:ident) => {
            push_reference(references, stringify!($op), "type", $value)
        };
        ($op:ident, array_type_index_src, $value:ident) => {
            push_reference(references, stringify!($op), "type", $value)
        };
        ($op:ident, struct_type_index, $value:ident) => {
            push_reference(references, stringify!($op), "type", $value)
        };
        ($op:ident, argument_index, $value:ident) => {
            push_reference(references, stringify!($op), "type", $value)
        };
        ($op:ident, result_index, $value:ident) => {
            push_reference(references, stringify!($op), "type", $value)
        };
        ($op:ident, cont_type_index, $value:ident) => {
            push_reference(references, stringify!($op), "type", $value)
        };
        ($op:ident, mem, $value:ident) => {
            push_reference(references, stringify!($op), "memory", $value)
        };
        ($op:ident, src_mem, $value:ident) => {
            push_reference(references, stringify!($op), "memory", $value)
        };
        ($op:ident, dst_mem, $value:ident) => {
            push_reference(references, stringify!($op), "memory", $value)
        };
        ($op:ident, memarg, $value:ident) => {
            push_reference(references, stringify!($op), "memory", $value.memory)
        };
        ($op:ident, data_index, $value:ident) => {
            push_reference(references, stringify!($op), "data", $value)
        };
        ($op:ident, array_data_index, $value:ident) => {
            push_reference(references, stringify!($op), "data", $value)
        };
        ($op:ident, elem_index, $value:ident) => {
            push_reference(references, stringify!($op), "element", $value)
        };
        ($op:ident, array_elem_index, $value:ident) => {
            push_reference(references, stringify!($op), "element", $value)
        };
        ($op:ident, blockty, $value:ident) => {
            push_block_type_reference(references, stringify!($op), $value)
        };
        ($op:ident, ty, $value:ident) => {
            push_val_type_reference(references, stringify!($op), $value)
        };
        ($op:ident, tys, $value:ident) => {
            for ty in $value.iter() {
                push_val_type_reference(references, stringify!($op), ty);
            }
        };
        ($op:ident, hty, $value:ident) => {
            push_reference(references, stringify!($op), "heap-type", $value)
        };
        ($op:ident, from_ref_type, $value:ident) => {
            push_reference(references, stringify!($op), "reference-type", $value)
        };
        ($op:ident, to_ref_type, $value:ident) => {
            push_reference(references, stringify!($op), "reference-type", $value)
        };
        ($op:ident, try_table, $value:ident) => {
            push_reference(references, stringify!($op), "try-table", $value)
        };
        ($op:ident, resume_table, $value:ident) => {
            push_reference(references, stringify!($op), "resume-table", $value)
        };
        ($op:ident, $field:ident, $value:ident) => {};
    }
    macro_rules! match_operator {
        ($( @$proposal:ident $op:ident $({ $($field:ident: $field_type:ty),* })? => $visit:ident ($($ann:tt)*))*) => {
            match operator {
                $(
                    wasmparser::Operator::$op $( { $($field),* } )? => {
                        $(
                            $(
                                let _ = $field;
                                record!($op, $field, $field);
                            )*
                        )?
                    }
                )*
                _ => unreachable!("wasmparser returned an unknown operator"),
            }
        };
    }
    wasmparser::for_each_operator!(match_operator);
}

#[derive(Debug, PartialEq, Eq)]
struct BodyShape {
    // these can affect gc, index rewriting, or wasm-bindgen transforms.
    references: Vec<String>,
    // local values and local control flow may change. everything else must match.
    fixed_operators: Vec<String>,
}

fn body_shape(body: &[u8]) -> Result<BodyShape, String> {
    let body = wasmparser::FunctionBody::new(wasmparser::BinaryReader::new(body, 0));
    let mut references = Vec::new();
    let locals = body
        .get_locals_reader()
        .map_err(|error| format!("cannot read function locals: {error}"))?;
    for local in locals {
        let (_, ty) = local.map_err(|error| format!("cannot read function local: {error}"))?;
        push_val_type_reference(&mut references, "local", &ty);
    }
    let mut fixed_operators = Vec::new();
    let mut reader = body
        .get_operators_reader()
        .map_err(|error| format!("cannot read function instructions: {error}"))?;
    while !reader.eof() {
        let operator = reader
            .read()
            .map_err(|error| format!("cannot read function instruction: {error}"))?;
        operator_references(&operator, &mut references);
        if !safe_value_operator(&operator) && !local_control_operator(&operator) {
            fixed_operators.push(format!("{operator:?}"));
        }
    }
    Ok(BodyShape {
        references,
        fixed_operators,
    })
}

fn body_has_only_operand_changes(old: &[u8], new: &[u8]) -> Result<bool, String> {
    let old_body = wasmparser::FunctionBody::new(wasmparser::BinaryReader::new(old, 0));
    let new_body = wasmparser::FunctionBody::new(wasmparser::BinaryReader::new(new, 0));
    let mut old_reader = old_body
        .get_operators_reader()
        .map_err(|error| format!("cannot read cached function body: {error}"))?;
    let mut new_reader = new_body
        .get_operators_reader()
        .map_err(|error| format!("cannot read current function body: {error}"))?;
    let old_ops_start = old_reader.original_position();
    let new_ops_start = new_reader.original_position();
    if old.get(..old_ops_start) != new.get(..new_ops_start) {
        return Ok(false);
    }
    let mut changed = false;
    loop {
        if old_reader.eof() || new_reader.eof() {
            return Ok(changed && old_reader.eof() && new_reader.eof());
        }
        let (old_op, old_start) = old_reader
            .read_with_offset()
            .map_err(|error| format!("cannot read cached function instruction: {error}"))?;
        let old_end = old_reader.original_position();
        let (new_op, new_start) = new_reader
            .read_with_offset()
            .map_err(|error| format!("cannot read current function instruction: {error}"))?;
        let new_end = new_reader.original_position();
        if old[old_start..old_end] == new[new_start..new_end] {
            continue;
        }
        if !patchable_operator_pair(&old_op, &new_op) {
            return Ok(false);
        }
        changed = true;
    }
}

fn body_has_only_patchable_changes(old: &[u8], new: &[u8]) -> Result<bool, String> {
    if body_has_only_operand_changes(old, new)? {
        return Ok(true);
    }
    if old == new || body_shape(old)? != body_shape(new)? {
        return Ok(false);
    }
    Ok(true)
}

fn operator_proposal(operator: &wasmparser::Operator<'_>) -> &'static str {
    macro_rules! match_operator {
        ($( @$proposal:ident $op:ident $({ $($field:ident: $field_type:ty),* })? => $visit:ident ($($ann:tt)*))*) => {
            match operator {
                $(wasmparser::Operator::$op { .. } => stringify!($proposal),)*
                _ => unreachable!("wasmparser returned an unknown operator"),
            }
        };
    }
    wasmparser::for_each_operator!(match_operator)
}

fn safe_value_operator(operator: &wasmparser::Operator<'_>) -> bool {
    if matches!(
        operator,
        wasmparser::Operator::Unreachable
            | wasmparser::Operator::Nop
            | wasmparser::Operator::Block { .. }
            | wasmparser::Operator::Loop { .. }
            | wasmparser::Operator::If { .. }
            | wasmparser::Operator::Else
            | wasmparser::Operator::End
            | wasmparser::Operator::Br { .. }
            | wasmparser::Operator::BrIf { .. }
            | wasmparser::Operator::BrTable { .. }
            | wasmparser::Operator::Return
    ) {
        return false;
    }
    let mut references = Vec::new();
    operator_references(operator, &mut references);
    if !references.is_empty() {
        return false;
    }
    matches!(
        operator_proposal(operator),
        "mvp"
            | "sign_extension"
            | "saturating_float_to_int"
            | "simd"
            | "relaxed_simd"
            | "wide_arithmetic"
    )
}

fn local_control_operator(operator: &wasmparser::Operator<'_>) -> bool {
    matches!(
        operator,
        wasmparser::Operator::Unreachable
            | wasmparser::Operator::Nop
            | wasmparser::Operator::Block { .. }
            | wasmparser::Operator::Loop { .. }
            | wasmparser::Operator::If { .. }
            | wasmparser::Operator::Else
            | wasmparser::Operator::End
            | wasmparser::Operator::Br { .. }
            | wasmparser::Operator::BrIf { .. }
            | wasmparser::Operator::BrTable { .. }
            | wasmparser::Operator::Return
    )
}

struct BodyReencoder<'a> {
    indices: &'a CodePatchState,
    functions: &'a [Option<u32>],
}

impl BodyReencoder<'_> {
    fn mapped(
        map: &[Option<u32>],
        index: u32,
        kind: &'static str,
    ) -> Result<u32, wasm_encoder::reencode::Error<&'static str>> {
        map.get(index as usize)
            .copied()
            .flatten()
            .ok_or(wasm_encoder::reencode::Error::UserError(kind))
    }
}

impl wasm_encoder::reencode::Reencode for BodyReencoder<'_> {
    type Error = &'static str;

    fn data_index(
        &mut self,
        index: u32,
    ) -> Result<u32, wasm_encoder::reencode::Error<Self::Error>> {
        Self::mapped(&self.indices.data, index, "removed data index")
    }

    fn element_index(
        &mut self,
        index: u32,
    ) -> Result<u32, wasm_encoder::reencode::Error<Self::Error>> {
        Self::mapped(&self.indices.elements, index, "removed element index")
    }

    fn function_index(
        &mut self,
        index: u32,
    ) -> Result<u32, wasm_encoder::reencode::Error<Self::Error>> {
        Self::mapped(self.functions, index, "removed function index")
    }

    fn global_index(
        &mut self,
        index: u32,
    ) -> Result<u32, wasm_encoder::reencode::Error<Self::Error>> {
        Self::mapped(&self.indices.globals, index, "removed global index")
    }

    fn memory_index(
        &mut self,
        index: u32,
    ) -> Result<u32, wasm_encoder::reencode::Error<Self::Error>> {
        Self::mapped(&self.indices.memories, index, "removed memory index")
    }

    fn table_index(
        &mut self,
        index: u32,
    ) -> Result<u32, wasm_encoder::reencode::Error<Self::Error>> {
        Self::mapped(&self.indices.tables, index, "removed table index")
    }

    fn tag_index(&mut self, index: u32) -> Result<u32, wasm_encoder::reencode::Error<Self::Error>> {
        Self::mapped(&self.indices.tags, index, "removed tag index")
    }

    fn type_index(
        &mut self,
        index: u32,
    ) -> Result<u32, wasm_encoder::reencode::Error<Self::Error>> {
        Self::mapped(&self.indices.types, index, "removed type index")
    }
}

fn local_function_param_counts(bytes: &[u8]) -> Result<Vec<usize>, String> {
    let mut type_params = Vec::new();
    let mut function_types = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        match payload.map_err(|error| format!("cannot parse browser module types: {error}"))? {
            wasmparser::Payload::TypeSection(types) => {
                for ty in types.into_iter_err_on_gc_types() {
                    type_params.push(
                        ty.map_err(|error| format!("cannot parse browser function type: {error}"))?
                            .params()
                            .len(),
                    );
                }
            }
            wasmparser::Payload::FunctionSection(functions) => {
                for ty in functions {
                    let ty = ty.map_err(|error| {
                        format!("cannot parse browser function section: {error}")
                    })? as usize;
                    function_types.push(
                        *type_params
                            .get(ty)
                            .ok_or_else(|| "browser function has an invalid type".to_owned())?,
                    );
                }
            }
            _ => {}
        }
    }
    Ok(function_types)
}

fn local_function_type_indices(bytes: &[u8]) -> Result<Vec<u32>, String> {
    let mut function_types = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        if let wasmparser::Payload::FunctionSection(functions) =
            payload.map_err(|error| format!("cannot parse browser function types: {error}"))?
        {
            for ty in functions {
                function_types.push(
                    ty.map_err(|error| format!("cannot parse browser function type: {error}"))?,
                );
            }
        }
    }
    Ok(function_types)
}

struct FunctionRemap {
    indices: CodePatchState,
    new_to_old_locals: Vec<usize>,
    unmatched_new_locals: std::collections::BTreeSet<usize>,
}

fn remap_functions(
    cached: &CodePatchState,
    old_identities: &[FunctionIdentity],
    new_identities: &[FunctionIdentity],
    old_types: &[u32],
    new_types: &[u32],
) -> Option<FunctionRemap> {
    if old_types.len() != new_types.len()
        || cached.functions.len() != old_identities.len()
        || cached.element_functions.len() != old_identities.len()
        || old_identities.len() != new_identities.len()
    {
        return None;
    }
    let imports = old_identities.len().checked_sub(old_types.len())?;
    if new_identities.len().checked_sub(new_types.len())? != imports
        || old_identities[..imports] != new_identities[..imports]
    {
        return None;
    }
    if old_identities == new_identities && old_types == new_types {
        return Some(FunctionRemap {
            indices: cached.clone(),
            new_to_old_locals: (0..new_types.len()).collect(),
            unmatched_new_locals: std::collections::BTreeSet::new(),
        });
    }
    let mut functions = vec![None; new_identities.len()];
    let mut element_functions = vec![None; new_identities.len()];
    functions[..imports].copy_from_slice(&cached.functions[..imports]);
    element_functions[..imports].copy_from_slice(&cached.element_functions[..imports]);
    let mut old_by_identity = std::collections::HashMap::new();
    for (local, identity) in old_identities[imports..].iter().enumerate() {
        if old_by_identity.insert(*identity, local).is_some() {
            return None;
        }
    }
    let mut new_to_old_locals = vec![usize::MAX; new_types.len()];
    let mut used_old = std::collections::BTreeSet::new();
    let mut unmatched_new = Vec::new();
    for (new_local, identity) in new_identities[imports..].iter().enumerate() {
        if let Some(&old_local) = old_by_identity.get(identity) {
            if !used_old.insert(old_local) || old_types[old_local] != new_types[new_local] {
                return None;
            }
            new_to_old_locals[new_local] = old_local;
            functions[imports + new_local] = cached.functions[imports + old_local];
            element_functions[imports + new_local] = cached.element_functions[imports + old_local];
        } else {
            unmatched_new.push(new_local);
        }
    }
    let unmatched_old = (0..old_types.len())
        .filter(|local| !used_old.contains(local))
        .collect::<Vec<_>>();
    if unmatched_old.len() != unmatched_new.len() || unmatched_old.len() > 64 {
        return None;
    }
    let mut old_by_type = std::collections::HashMap::<u32, Vec<usize>>::new();
    let mut new_by_type = std::collections::HashMap::<u32, Vec<usize>>::new();
    for old_local in unmatched_old {
        old_by_type
            .entry(old_types[old_local])
            .or_default()
            .push(old_local);
    }
    for &new_local in &unmatched_new {
        new_by_type
            .entry(new_types[new_local])
            .or_default()
            .push(new_local);
    }
    if old_by_type
        .keys()
        .collect::<std::collections::BTreeSet<_>>()
        != new_by_type
            .keys()
            .collect::<std::collections::BTreeSet<_>>()
    {
        return None;
    }
    for (ty, old_locals) in old_by_type {
        let new_locals = new_by_type.remove(&ty)?;
        if old_locals.len() != 1 || new_locals.len() != 1 {
            return None;
        }
        let old_local = old_locals[0];
        let new_local = new_locals[0];
        let emitted = cached.functions[imports + old_local]?;
        new_to_old_locals[new_local] = old_local;
        functions[imports + new_local] = Some(emitted);
        element_functions[imports + new_local] = cached.element_functions[imports + old_local];
    }
    let unmatched_new_locals = unmatched_new.into_iter().collect();
    let mut indices = cached.clone();
    indices.functions = functions;
    indices.element_functions = element_functions;
    Some(FunctionRemap {
        indices,
        new_to_old_locals,
        unmatched_new_locals,
    })
}

fn element_function_items(bytes: &[u8]) -> Result<Vec<u32>, String> {
    let mut functions = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|error| format!("cannot parse element section: {error}"))?;
        let wasmparser::Payload::ElementSection(elements) = payload else {
            continue;
        };
        for element in elements {
            let element =
                element.map_err(|error| format!("cannot parse element segment: {error}"))?;
            match element.items {
                wasmparser::ElementItems::Functions(items) => {
                    for function in items {
                        functions.push(
                            function.map_err(|error| {
                                format!("cannot parse element function: {error}")
                            })?,
                        );
                    }
                }
                wasmparser::ElementItems::Expressions(_, expressions) => {
                    for expression in expressions {
                        let expression = expression
                            .map_err(|error| format!("cannot parse element expression: {error}"))?;
                        let mut reader = expression.get_operators_reader();
                        while !reader.eof() {
                            if let wasmparser::Operator::RefFunc { function_index } =
                                reader.read().map_err(|error| {
                                    format!("cannot parse element expression: {error}")
                                })?
                            {
                                functions.push(function_index);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(functions)
}

fn element_function_map(
    input: &[u32],
    output: &[u32],
    functions: &[Option<u32>],
) -> Option<Vec<Option<u32>>> {
    if input.len() != output.len() {
        return None;
    }
    let mut mapped = functions.to_vec();
    for (&original, &emitted) in input.iter().zip(output) {
        let slot = mapped.get_mut(original as usize)?;
        match *slot {
            Some(existing) if existing != emitted => return None,
            Some(_) => {}
            None => *slot = Some(emitted),
        }
    }
    Some(mapped)
}

fn reencode_element_section(
    bytes: &[u8],
    indices: &CodePatchState,
) -> Result<Option<Vec<u8>>, String> {
    use wasm_encoder::reencode::Reencode;

    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|error| format!("cannot parse element section: {error}"))?;
        let wasmparser::Payload::ElementSection(elements) = payload else {
            continue;
        };
        let mut section = wasm_encoder::ElementSection::new();
        BodyReencoder {
            indices,
            functions: &indices.element_functions,
        }
        .parse_element_section(&mut section, elements)
        .map_err(|error| format!("cannot re-encode element section: {error}"))?;
        let mut module = wasm_encoder::Module::new();
        module.section(&section);
        return Ok(Some(module.finish()[8..].to_vec()));
    }
    Ok(None)
}

fn reencode_body(
    body: &[u8],
    param_count: usize,
    indices: &CodePatchState,
) -> Result<Vec<u8>, String> {
    use wasm_encoder::reencode::Reencode;

    let body = wasmparser::FunctionBody::new(wasmparser::BinaryReader::new(body, 0));
    let mut reencoder = BodyReencoder {
        indices,
        functions: &indices.functions,
    };
    let mut local_types = Vec::new();
    let locals = body
        .get_locals_reader()
        .map_err(|error| format!("cannot read function locals: {error}"))?;
    for local in locals {
        let (count, ty) = local.map_err(|error| format!("cannot read function local: {error}"))?;
        for _ in 0..count {
            local_types.push(ty);
        }
    }
    let mut used = std::collections::BTreeSet::new();
    let mut reader = body
        .get_operators_reader()
        .map_err(|error| format!("cannot read function instructions: {error}"))?;
    while !reader.eof() {
        match reader
            .read()
            .map_err(|error| format!("cannot read function instruction: {error}"))?
        {
            wasmparser::Operator::LocalGet { local_index }
            | wasmparser::Operator::LocalSet { local_index }
            | wasmparser::Operator::LocalTee { local_index } => {
                used.insert(local_index as usize);
            }
            _ => {}
        }
    }
    let mut grouped = std::collections::BTreeMap::new();
    for original in used.iter().copied().filter(|index| *index >= param_count) {
        let ty = *local_types
            .get(original - param_count)
            .ok_or_else(|| "function refers to an invalid local".to_owned())?;
        grouped.entry(ty).or_insert_with(Vec::new).push(original);
    }
    let mut local_map = (0..param_count as u32).collect::<Vec<_>>();
    local_map.resize(param_count + local_types.len(), u32::MAX);
    let mut emitted_locals = Vec::new();
    let mut next = param_count as u32;
    for (ty, originals) in grouped {
        emitted_locals.push((
            originals.len() as u32,
            reencoder
                .val_type(ty)
                .map_err(|error| format!("cannot re-encode function local type: {error}"))?,
        ));
        for original in originals {
            local_map[original] = next;
            next += 1;
        }
    }
    let mut function = wasm_encoder::Function::new(emitted_locals);
    let mut reader = body
        .get_operators_reader()
        .map_err(|error| format!("cannot read function instructions: {error}"))?;
    while !reader.eof() {
        let op = reader
            .read()
            .map_err(|error| format!("cannot read function instruction: {error}"))?;
        let instruction = match op {
            wasmparser::Operator::LocalGet { local_index } => wasm_encoder::Instruction::LocalGet(
                *local_map
                    .get(local_index as usize)
                    .filter(|index| **index != u32::MAX)
                    .ok_or_else(|| "function refers to an unused local".to_owned())?,
            ),
            wasmparser::Operator::LocalSet { local_index } => wasm_encoder::Instruction::LocalSet(
                *local_map
                    .get(local_index as usize)
                    .filter(|index| **index != u32::MAX)
                    .ok_or_else(|| "function refers to an unused local".to_owned())?,
            ),
            wasmparser::Operator::LocalTee { local_index } => wasm_encoder::Instruction::LocalTee(
                *local_map
                    .get(local_index as usize)
                    .filter(|index| **index != u32::MAX)
                    .ok_or_else(|| "function refers to an unused local".to_owned())?,
            ),
            op => wasm_encoder::reencode::utils::instruction(&mut reencoder, op)
                .map_err(|error| format!("cannot re-encode function instruction: {error}"))?,
        };
        function.instruction(&instruction);
    }
    Ok(function.into_raw_body())
}

fn encode_uleb_u32(mut value: u32, bytes: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        bytes.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn rebuilt_code_section(
    bytes: &[u8],
    range: &Range<usize>,
    prefix: &Range<usize>,
    bodies: &[CodeBody],
    replacements: &std::collections::BTreeMap<usize, Vec<u8>>,
) -> Result<Vec<u8>, String> {
    let mut payload = bytes[prefix.clone()].to_vec();
    for (index, body) in bodies.iter().enumerate() {
        if let Some(replacement) = replacements.get(&index) {
            let len = u32::try_from(replacement.len())
                .map_err(|_| "patched function body is too large".to_owned())?;
            encode_uleb_u32(len, &mut payload);
            payload.extend_from_slice(replacement);
        } else {
            payload.extend_from_slice(&bytes[body.entry.clone()]);
        }
    }
    let payload_len =
        u32::try_from(payload.len()).map_err(|_| "patched code section is too large".to_owned())?;
    let mut section = Vec::with_capacity(payload.len() + 6);
    section.push(10);
    encode_uleb_u32(payload_len, &mut section);
    section.extend_from_slice(&payload);
    if range.start >= range.end {
        return Err("browser module has an invalid code section range".to_owned());
    }
    Ok(section)
}

fn replace_sections(bytes: &[u8], mut replacements: Vec<(Range<usize>, Vec<u8>)>) -> Vec<u8> {
    replacements.sort_by_key(|(range, _)| range.start);
    let extra = replacements
        .iter()
        .map(|(range, replacement)| replacement.len().saturating_sub(range.len()))
        .sum::<usize>();
    let mut patched = Vec::with_capacity(bytes.len() + extra);
    let mut cursor = 0;
    for (range, replacement) in replacements {
        patched.extend_from_slice(&bytes[cursor..range.start]);
        patched.extend_from_slice(&replacement);
        cursor = range.end;
    }
    patched.extend_from_slice(&bytes[cursor..]);
    patched
}

fn validate_patched_functions(
    bytes: &[u8],
    changed: &std::collections::BTreeSet<usize>,
) -> Result<(), String> {
    let mut validator = wasmparser::Validator::new();
    let mut local_index = 0usize;
    let mut validated = 0usize;
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        let payload =
            payload.map_err(|error| format!("patched browser module is invalid: {error}"))?;
        if let wasmparser::ValidPayload::Func(function, body) = validator
            .payload(&payload)
            .map_err(|error| format!("patched browser module is invalid: {error}"))?
        {
            if changed.contains(&local_index) {
                function
                    .into_validator(Default::default())
                    .validate(&body)
                    .map_err(|error| format!("patched browser function is invalid: {error}"))?;
                validated += 1;
            }
            local_index += 1;
        }
    }
    if validated != changed.len() {
        return Err("patched browser module did not contain every changed function".to_owned());
    }
    Ok(())
}

fn patch_code_output(
    output: &Path,
    name: &str,
    cached: &CacheState,
    input: &[u8],
    input_identities: Option<&[FunctionIdentity]>,
    input_names_fingerprint: Option<&str>,
    input_info: &ModuleInfo,
) -> Result<Option<CodePatchState>, String> {
    macro_rules! miss {
        ($reason:literal) => {{
            if std::env::var_os("MACH_PROFILE_BUILD").is_some() {
                eprintln!("  profile   wasm-bindgen code patch miss: {}", $reason);
            }
            return Ok(None);
        }};
    }
    let Some(indices) = cached.code_patch.as_ref() else {
        miss!("no index map");
    };
    let Some(input_identities) = input_identities else {
        miss!("linked module has no function identities");
    };
    let Some(old_identities) =
        load_function_identities(output, name, &indices.function_identities_fingerprint)
    else {
        miss!("cached function identities are missing");
    };
    let Some(input_code_range) = input_info.code_range.as_ref() else {
        miss!("input has no code section");
    };
    let source_started = Instant::now();
    let source_path = bindgen_source(output, name);
    let old_input = if let Some(bytes) =
        remembered_bindgen_source(&source_path, &cached.fingerprint)
    {
        bytes
    } else {
        match fs::read(&source_path) {
            Ok(bytes) if source_fingerprint(&bytes, name) == cached.fingerprint => Arc::new(bytes),
            _ => miss!("cached source does not match marker"),
        }
    };
    let old_info = match module_info(&old_input, name) {
        Ok(info) if info.non_code_layout_fingerprint == indices.non_code_layout_fingerprint => info,
        _ => miss!("cached source layout does not match index map"),
    };
    let Some(old_code_range) = old_info.code_range.as_ref() else {
        miss!("cached source has no code section");
    };
    let source_elapsed = source_started.elapsed();
    let remap_started = Instant::now();
    let (_, old_bodies) = code_bodies(&old_input, old_code_range)?;
    let (_, input_bodies) = code_bodies(input, input_code_range)?;
    if old_bodies.len() != input_bodies.len()
        || cached.data_len
            != input_info
                .data_range
                .as_ref()
                .map(|range| range.end - range.start)
    {
        miss!("function or data layout changed");
    }
    let old_types = local_function_type_indices(&old_input)?;
    let input_types = local_function_type_indices(input)?;
    let Some(mut remap) = remap_functions(
        indices,
        &old_identities,
        input_identities,
        &old_types,
        &input_types,
    ) else {
        miss!("function identities could not be mapped");
    };
    let remap_elapsed = remap_started.elapsed();
    let module_layout_remapped =
        indices.non_code_layout_fingerprint != input_info.non_code_layout_fingerprint;
    let element_patch = if module_layout_remapped {
        if indices.fixed_layout_fingerprint != input_info.fixed_layout_fingerprint {
            miss!("fixed module layout changed");
        }
        if old_info.fixed_layout_fingerprint != indices.fixed_layout_fingerprint {
            miss!("cached fixed module layout changed");
        }
        Some((
            reencode_element_section(&old_input, indices)?,
            reencode_element_section(input, &remap.indices)?,
        ))
    } else {
        None
    };
    let changed = remap
        .new_to_old_locals
        .iter()
        .copied()
        .enumerate()
        .filter_map(|(new_local, old_local)| {
            (old_local == usize::MAX
                || old_input[old_bodies[old_local].body.clone()]
                    != input[input_bodies[new_local].body.clone()])
            .then_some(new_local)
        })
        .collect::<Vec<_>>();
    if changed.len() > 256 {
        miss!("changed function count is outside patch limit");
    }

    let output_started = Instant::now();
    let wasm_path = output.join(format!("{name}_bg.wasm"));
    let output_bytes = fs::read(&wasm_path).map_err(|error| {
        format!(
            "cannot read cached browser module {}: {error}",
            wasm_path.display()
        )
    })?;
    let output_info = module_info(&output_bytes, name)?;
    let element_replacement = match element_patch {
        Some((Some(old), Some(new))) => {
            let Some(output_element_range) = output_info.element_range.as_ref() else {
                miss!("cached output has no element section");
            };
            if output_bytes[output_element_range.clone()] != old {
                miss!("old element section does not prove direct passthrough");
            }
            Some((output_element_range.clone(), new))
        }
        Some((None, None)) | None => None,
        Some(_) => miss!("element section shape changed"),
    };
    let Some(output_code_range) = output_info.code_range.as_ref() else {
        miss!("cached output has no code section");
    };
    let (output_prefix, output_bodies) = code_bodies(&output_bytes, output_code_range)?;
    let output_elapsed = output_started.elapsed();
    let rewrite_started = Instant::now();
    let raw_function_imports = remap.indices.functions.len() - input_bodies.len();
    let param_counts = local_function_param_counts(input)?;
    if param_counts.len() != input_bodies.len() {
        miss!("function types do not match code bodies");
    }
    let mut replacements = std::collections::BTreeMap::new();
    for new_local_index in changed {
        let old_local_index = remap.new_to_old_locals[new_local_index];
        let old = &old_input[old_bodies[old_local_index].body.clone()];
        let new = &input[input_bodies[new_local_index].body.clone()];
        let original_function = raw_function_imports + new_local_index;
        let Some(Some(emitted_function)) = remap.indices.functions.get(original_function) else {
            miss!("changed function was removed by wasm-bindgen");
        };
        let Some(output_local_index) = emitted_function
            .checked_sub(indices.emitted_function_imports)
            .map(|index| index as usize)
        else {
            miss!("changed function maps to an import");
        };
        let Some(output_body) = output_bodies.get(output_local_index) else {
            miss!("changed function is outside cached output");
        };
        let old_reencoded = reencode_body(old, param_counts[new_local_index], indices)?;
        if old_reencoded != output_bytes[output_body.body.clone()] {
            miss!("old body does not prove direct passthrough");
        }
        let new_reencoded = reencode_body(new, param_counts[new_local_index], &remap.indices)?;
        if new_reencoded == old_reencoded {
            continue;
        }
        if !module_layout_remapped
            && !remap.unmatched_new_locals.contains(&new_local_index)
            && !body_has_only_patchable_changes(&old_reencoded, &new_reencoded)?
        {
            miss!("function changed a module-facing operation");
        }
        replacements.insert(output_local_index, new_reencoded);
    }

    let changed_output_functions = replacements.keys().copied().collect();
    if !module_layout_remapped
        && element_replacement.is_none()
        && cached.data_fingerprint == input_info.data_fingerprint
    {
        let direct_patches = replacements
            .iter()
            .map(|(index, bytes)| (output_bodies[*index].body.clone(), bytes.as_slice()))
            .collect::<Vec<_>>();
        if direct_patches
            .iter()
            .all(|(range, bytes)| range.len() == bytes.len())
        {
            if !direct_patches.is_empty() {
                let candidate = wasm_path
                    .with_file_name(format!(".{name}_bg.wasm.next-{}", std::process::id()));
                clone_and_patch_file(&wasm_path, &candidate, &direct_patches).map_err(|error| {
                    format!(
                        "cannot patch browser module {}: {error}",
                        candidate.display()
                    )
                })?;
                if let Err(error) = crate::project::activate_validated_file(&candidate, &wasm_path)
                {
                    let _ = fs::remove_file(candidate);
                    return Err(error);
                }
            }
            remap.indices.non_code_layout_fingerprint =
                input_info.non_code_layout_fingerprint.clone();
            remap.indices.fixed_layout_fingerprint = input_info.fixed_layout_fingerprint.clone();
            remap.indices.function_identities_fingerprint =
                function_identities_fingerprint(input_identities);
            remap.indices.function_names_fingerprint = input_names_fingerprint.map(str::to_owned);
            if std::env::var_os("MACH_PROFILE_BUILD").is_some() {
                eprintln!(
                    "  profile   code patch source {:.0}ms / remap {:.0}ms / output {:.0}ms / rewrite {:.0}ms",
                    source_elapsed.as_secs_f64() * 1000.0,
                    remap_elapsed.as_secs_f64() * 1000.0,
                    output_elapsed.as_secs_f64() * 1000.0,
                    rewrite_started.elapsed().as_secs_f64() * 1000.0,
                );
            }
            return Ok(Some(remap.indices));
        }
    }

    let code = rebuilt_code_section(
        &output_bytes,
        output_code_range,
        &output_prefix,
        &output_bodies,
        &replacements,
    )?;
    let mut section_replacements = vec![(output_code_range.clone(), code)];
    if let Some(element_replacement) = element_replacement {
        section_replacements.push(element_replacement);
    }
    if cached.data_fingerprint != input_info.data_fingerprint {
        let (Some(input_data), Some(output_data), Some(expected_old_data)) = (
            input_info.data_range.as_ref(),
            output_info.data_range.as_ref(),
            cached.data_fingerprint.as_ref(),
        ) else {
            miss!("data section shape changed");
        };
        if input_data.len() != output_data.len()
            || output_info.data_fingerprint.as_ref() != Some(expected_old_data)
        {
            miss!("cached output data does not match marker");
        }
        section_replacements.push((output_data.clone(), input[input_data.clone()].to_vec()));
    }
    let patched = replace_sections(&output_bytes, section_replacements);
    validate_patched_functions(&patched, &changed_output_functions)?;
    let candidate =
        wasm_path.with_file_name(format!(".{name}_bg.wasm.next-{}", std::process::id()));
    fs::write(&candidate, patched).map_err(|error| {
        format!(
            "cannot write patched browser module {}: {error}",
            candidate.display()
        )
    })?;
    if let Err(error) = crate::project::activate_validated_file(&candidate, &wasm_path) {
        let _ = fs::remove_file(candidate);
        return Err(error);
    }
    remap.indices.non_code_layout_fingerprint = input_info.non_code_layout_fingerprint.clone();
    remap.indices.fixed_layout_fingerprint = input_info.fixed_layout_fingerprint.clone();
    remap.indices.function_identities_fingerprint =
        function_identities_fingerprint(input_identities);
    remap.indices.function_names_fingerprint = input_names_fingerprint.map(str::to_owned);
    Ok(Some(remap.indices))
}

fn output_data_matches_input(output: &Path, name: &str, input_info: &ModuleInfo) -> bool {
    let Ok(output_bytes) = fs::read(output.join(format!("{name}_bg.wasm"))) else {
        return false;
    };
    let Ok(output_info) = module_info(&output_bytes, name) else {
        return false;
    };
    match (&input_info.data_range, &output_info.data_range) {
        (None, None) => true,
        (Some(input_range), Some(output_range)) => {
            input_range.end - input_range.start == output_range.end - output_range.start
                && input_info.data_fingerprint == output_info.data_fingerprint
        }
        _ => false,
    }
}

fn patch_data_output(
    output: &Path,
    name: &str,
    cached: &CacheState,
    input: &[u8],
    input_info: &ModuleInfo,
) -> Result<bool, String> {
    if cached.data_fingerprint == input_info.data_fingerprint {
        return Ok(true);
    }
    let (Some(input_range), Some(expected_old_data)) =
        (&input_info.data_range, &cached.data_fingerprint)
    else {
        return Ok(false);
    };
    let wasm_path = output.join(format!("{name}_bg.wasm"));
    let mut output_bytes = fs::read(&wasm_path).map_err(|error| {
        format!(
            "cannot read cached browser module {}: {error}",
            wasm_path.display()
        )
    })?;
    let output_info = module_info(&output_bytes, name)?;
    let Some(output_range) = output_info.data_range else {
        return Ok(false);
    };
    if output_range.end - output_range.start != input_range.end - input_range.start
        || output_info.data_fingerprint.as_ref() != Some(expected_old_data)
    {
        return Ok(false);
    }
    output_bytes[output_range].copy_from_slice(&input[input_range.clone()]);
    let candidate =
        wasm_path.with_file_name(format!(".{name}_bg.wasm.next-{}", std::process::id()));
    fs::write(&candidate, output_bytes).map_err(|error| {
        format!(
            "cannot write patched browser module {}: {error}",
            candidate.display()
        )
    })?;
    if let Err(error) = crate::project::activate_validated_file(&candidate, &wasm_path) {
        let _ = fs::remove_file(candidate);
        return Err(error);
    }
    Ok(true)
}

fn expected_outputs(generated: &wasm_bindgen_cli_support::Output, name: &str) -> Vec<String> {
    let mut expected = vec![format!("{name}.js"), format!("{name}_bg.wasm")];
    for (identifier, snippets) in generated.snippets() {
        for index in 0..snippets.len() {
            expected.push(format!("snippets/{identifier}/inline{index}.js"));
        }
    }
    for path in generated.local_modules().keys() {
        expected.push(format!("snippets/{path}"));
    }
    expected.sort();
    expected.dedup();
    expected
}

fn record_cache(output: &Path, name: &str, state: &CacheState) {
    if let Ok(marker) = serde_json::to_vec(state) {
        let _ = fs::write(cache_marker(output, name), marker);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mach-bindgen-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn cache_state() -> CacheState {
        CacheState {
            fingerprint: "input-hash".to_owned(),
            layout_fingerprint: Some("layout-hash".to_owned()),
            data_fingerprint: Some("data-hash".to_owned()),
            data_len: Some(8),
            data_passthrough: true,
            expected: vec![
                "mach_webgpu.js".to_owned(),
                "mach_webgpu_bg.wasm".to_owned(),
                "snippets/generated/inline0.js".to_owned(),
            ],
            code_patch: None,
        }
    }

    fn empty_code_patch() -> CodePatchState {
        CodePatchState {
            non_code_layout_fingerprint: "layout".to_owned(),
            fixed_layout_fingerprint: "fixed-layout".to_owned(),
            function_identities_fingerprint: "identities".to_owned(),
            function_names_fingerprint: None,
            functions: Vec::new(),
            element_functions: Vec::new(),
            types: Vec::new(),
            globals: Vec::new(),
            memories: Vec::new(),
            tables: Vec::new(),
            elements: Vec::new(),
            data: Vec::new(),
            tags: Vec::new(),
            emitted_function_imports: 0,
        }
    }

    fn push_uleb(mut value: usize, bytes: &mut Vec<u8>) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            bytes.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    fn test_module(data: &[u8], name_payload: &[u8]) -> Vec<u8> {
        let mut module = b"\0asm\x01\0\0\0".to_vec();
        module.push(1);
        module.push(1);
        module.push(0);
        module.push(11);
        push_uleb(data.len(), &mut module);
        module.extend_from_slice(data);
        module.push(0);
        push_uleb(name_payload.len() + 5, &mut module);
        module.push(4);
        module.extend_from_slice(b"name");
        module.extend_from_slice(name_payload);
        module
    }

    fn named_module(names: &[&str]) -> Vec<u8> {
        let mut map = Vec::new();
        push_uleb(names.len(), &mut map);
        for (index, name) in names.iter().enumerate() {
            push_uleb(index, &mut map);
            push_uleb(name.len(), &mut map);
            map.extend_from_slice(name.as_bytes());
        }
        let mut names_payload = vec![1];
        push_uleb(map.len(), &mut names_payload);
        names_payload.extend_from_slice(&map);
        let mut module = b"\0asm\x01\0\0\0".to_vec();
        module.push(0);
        push_uleb(names_payload.len() + 5, &mut module);
        module.push(4);
        module.extend_from_slice(b"name");
        module.extend_from_slice(&names_payload);
        module
    }

    #[test]
    fn cache_requires_the_fingerprint_and_every_output() {
        let root = test_root("cache-test");
        fs::create_dir_all(root.join("snippets/generated")).unwrap();
        fs::write(root.join("mach_webgpu.js"), b"js").unwrap();
        fs::write(root.join("mach_webgpu_bg.wasm"), b"wasm").unwrap();
        fs::write(root.join("snippets/generated/inline0.js"), b"snippet").unwrap();
        let state = cache_state();
        record_cache(&root, "mach_webgpu", &state);

        let loaded = load_cache(&root, "mach_webgpu").unwrap();
        assert_eq!(loaded.fingerprint, "input-hash");
        assert!(outputs_exist(&root, &loaded));
        fs::remove_file(root.join("snippets/generated/inline0.js")).unwrap();
        assert!(!outputs_exist(&root, &loaded));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_rejects_paths_outside_the_output_directory() {
        let root = test_root("cache-path-test");
        fs::create_dir_all(&root).unwrap();
        let mut state = cache_state();
        state.expected = vec!["../outside".to_owned()];
        assert!(!outputs_exist(&root, &state));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn function_identities_follow_names_across_linker_reordering() {
        let first = extract_function_identities(&named_module(&["alpha", "beta", "alpha"]))
            .unwrap()
            .unwrap();
        let reordered = extract_function_identities(&named_module(&["beta", "alpha", "alpha"]))
            .unwrap()
            .unwrap();

        assert_eq!(first[0], reordered[1]);
        assert_eq!(first[1], reordered[0]);
        assert_eq!(first[2], reordered[2]);
        assert_ne!(first[0], first[2]);
    }

    #[test]
    fn element_function_map_records_synthetic_replacements() {
        let mapped =
            element_function_map(&[0, 1, 2], &[10, 11, 12], &[Some(10), None, Some(12)]).unwrap();

        assert_eq!(mapped, vec![Some(10), Some(11), Some(12)]);
        assert!(element_function_map(&[0], &[11], &[Some(10)]).is_none());
    }

    #[test]
    fn layout_ignores_data_and_names_but_not_code() {
        let first = test_module(b"old-data", b"old-name");
        let second = test_module(b"new-data", b"new-name");
        let mut changed_type = second.clone();
        changed_type[10] = 1;

        let first = module_info(&first, "game").unwrap();
        let second = module_info(&second, "game").unwrap();
        let changed_type = module_info(&changed_type, "game").unwrap();
        assert_eq!(first.layout_fingerprint, second.layout_fingerprint);
        assert_ne!(first.data_fingerprint, second.data_fingerprint);
        assert_ne!(first.layout_fingerprint, changed_type.layout_fingerprint);
    }

    #[test]
    fn data_patch_keeps_the_rest_of_the_processed_module() {
        let root = test_root("data-patch-test");
        fs::create_dir_all(&root).unwrap();
        let old = test_module(b"old-data", b"processed-name");
        let current = test_module(b"new-data", b"source-name");
        let old_info = module_info(&old, "mach_webgpu").unwrap();
        let current_info = module_info(&current, "mach_webgpu").unwrap();
        let wasm = root.join("mach_webgpu_bg.wasm");
        fs::write(&wasm, &old).unwrap();
        let state = CacheState {
            fingerprint: "old".to_owned(),
            layout_fingerprint: Some(current_info.layout_fingerprint.clone()),
            data_fingerprint: old_info.data_fingerprint,
            data_len: old_info
                .data_range
                .as_ref()
                .map(|range| range.end - range.start),
            data_passthrough: true,
            expected: vec!["mach_webgpu_bg.wasm".to_owned()],
            code_patch: None,
        };

        assert!(patch_data_output(&root, "mach_webgpu", &state, &current, &current_info,).unwrap());
        let patched = fs::read(wasm).unwrap();
        let patched_info = module_info(&patched, "mach_webgpu").unwrap();
        assert_eq!(
            &patched[patched_info.data_range.unwrap()],
            &current[current_info.data_range.unwrap()]
        );
        assert!(patched.ends_with(b"processed-name"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn code_patch_accepts_numeric_and_local_operand_changes() {
        let old = [0, 0x41, 1, 0x1a, 0x0b];
        let numeric = [0, 0x41, 2, 0x1a, 0x0b];
        let structural = [0, 0x42, 2, 0x1a, 0x0b];
        let old_local = [1, 2, 0x7f, 0x20, 0, 0x1a, 0x0b];
        let new_local = [1, 2, 0x7f, 0x20, 1, 0x1a, 0x0b];
        let add = [0, 0x41, 1, 0x41, 2, 0x6a, 0x1a, 0x0b];
        let multiply = [0, 0x41, 1, 0x41, 2, 0x6c, 0x1a, 0x0b];
        let call_zero = [0, 0x10, 0, 0x0b];
        let call_one = [0, 0x10, 1, 0x0b];
        let extra_operator = [0, 0x41, 1, 0x1a, 0x01, 0x0b];
        let extra_value_operators = [0, 0x41, 1, 0x41, 2, 0x6a, 0x1a, 0x0b];
        let nop = [0, 0x01, 0x0b];
        let drop = [0, 0x1a, 0x0b];
        let unreachable = [0, 0x00, 0x0b];
        let return_ = [0, 0x0f, 0x0b];
        let plain = [0, 0x41, 0, 0x1a, 0x0b];
        let local_block = [0, 0x02, 0x40, 0x41, 0, 0x1a, 0x0b, 0x0b];
        let load_offset_zero = [0, 0x41, 0, 0x28, 2, 0, 0x1a, 0x0b];
        let load_offset_one = [0, 0x41, 0, 0x28, 2, 1, 0x1a, 0x0b];

        assert!(body_has_only_patchable_changes(&old, &numeric).unwrap());
        assert!(body_has_only_patchable_changes(&old_local, &new_local).unwrap());
        assert!(body_has_only_patchable_changes(&add, &multiply).unwrap());
        assert!(body_has_only_patchable_changes(&old, &extra_value_operators).unwrap());
        assert!(body_has_only_patchable_changes(&old, &structural).unwrap());
        assert!(!body_has_only_patchable_changes(&call_zero, &call_one).unwrap());
        assert!(body_has_only_patchable_changes(&old, &extra_operator).unwrap());
        assert!(body_has_only_patchable_changes(&nop, &drop).unwrap());
        assert!(body_has_only_patchable_changes(&unreachable, &return_).unwrap());
        assert!(body_has_only_patchable_changes(&plain, &local_block).unwrap());
        assert!(body_has_only_patchable_changes(&load_offset_zero, &load_offset_one).unwrap());
    }

    #[test]
    fn body_reencoder_compacts_unused_locals_like_walrus() {
        let raw = [1, 1, 0x7f, 0x41, 1, 0x1a, 0x0b];
        let expected = [0, 0x41, 1, 0x1a, 0x0b];

        assert_eq!(
            reencode_body(&raw, 0, &empty_code_patch()).unwrap(),
            expected
        );
    }
}
