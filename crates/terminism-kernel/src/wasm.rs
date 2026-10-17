use crate::host::{Cmd, Json, WasmHost};
use crate::logger::Logger;

pub(crate) const FUEL_PER_CALL: u64 = 1_000_000;
pub(crate) const MAX_MEMORY: usize = 64 * 1024 * 1024;
pub(crate) const MAX_STRING: usize = 1024 * 1024;

pub(crate) fn host_state(
    actor: &str,
    log: Logger,
    queue: std::rc::Rc<std::cell::RefCell<std::collections::VecDeque<Cmd>>>,
    data: std::rc::Rc<std::cell::RefCell<crate::host::DataStore>>,
    config: std::rc::Rc<std::cell::RefCell<crate::host::ConfigStore>>,
) -> WasmHost {
    WasmHost {
        actor: actor.into(),
        log,
        queue,
        data,
        config,
        limits: wasmi::StoreLimitsBuilder::new()
            .memory_size(MAX_MEMORY)
            .memories(1)
            .instances(1)
            .trap_on_grow_failure(true)
            .build(),
    }
}

fn engine() -> wasmi::Engine {
    let mut config = wasmi::Config::default();
    config.consume_fuel(true);
    wasmi::Engine::new(&config)
}

fn validate(module: &wasmi::Module) -> Result<(), String> {
    use wasmi::ValType::I32;
    if !matches!(
        module.get_export("memory"),
        Some(wasmi::ExternType::Memory(_))
    ) {
        return Err("guest must export memory".into());
    }
    let signatures: &[(&str, &[wasmi::ValType], &[wasmi::ValType])] = &[
        ("alloc", &[I32], &[I32]),
        ("dealloc", &[I32, I32], &[]),
        ("init", &[], &[]),
        ("on_event", &[I32, I32, I32, I32], &[I32]),
    ];
    for &(name, params, results) in signatures {
        match module.get_export(name) {
            Some(wasmi::ExternType::Func(f)) if f.params() == params && f.results() == results => {}
            _ => return Err(format!("invalid guest export: {name}")),
        }
    }
    Ok(())
}

pub(crate) fn check(bytes: &[u8]) -> Result<(), String> {
    let module = wasmi::Module::new(&engine(), bytes).map_err(|e| e.to_string())?;
    validate(&module)
}

pub(crate) fn instantiate(
    bytes: &[u8],
    state: WasmHost,
) -> Result<(wasmi::Store<WasmHost>, wasmi::Instance), String> {
    let engine = engine();
    let module = wasmi::Module::new(&engine, bytes).map_err(|e| e.to_string())?;
    validate(&module)?;
    let linker = build_linker(&engine).map_err(|e| e.to_string())?;
    let mut store = wasmi::Store::new(&engine, state);
    store.limiter(|state| &mut state.limits);
    store.set_fuel(FUEL_PER_CALL).map_err(|e| e.to_string())?;
    let instance = linker
        .instantiate_and_start(&mut store, &module)
        .map_err(|e| e.to_string())?;
    store.set_fuel(FUEL_PER_CALL).map_err(|e| e.to_string())?;
    instance
        .get_typed_func::<(), ()>(&store, "init")
        .and_then(|f| f.call(&mut store, ()))
        .map_err(|e| e.to_string())?;
    Ok((store, instance))
}

pub(crate) fn invoke(
    store: &mut wasmi::Store<WasmHost>,
    instance: wasmi::Instance,
    event: &str,
    payload: &Json,
) -> Result<(), String> {
    let payload = payload.to_string();
    if event.len() > MAX_STRING || payload.len() > MAX_STRING {
        return Err("wasm string limit exceeded".into());
    }
    store.set_fuel(FUEL_PER_CALL).map_err(|e| e.to_string())?;
    let alloc = instance
        .get_typed_func::<i32, i32>(&*store, "alloc")
        .map_err(|e| e.to_string())?;
    let free = instance
        .get_typed_func::<(i32, i32), ()>(&*store, "dealloc")
        .map_err(|e| e.to_string())?;
    let memory = instance
        .get_memory(&*store, "memory")
        .ok_or("guest must export memory")?;
    let name_ptr = alloc
        .call(&mut *store, event.len() as i32)
        .map_err(|e| e.to_string())?;
    let data_ptr = alloc
        .call(&mut *store, payload.len() as i32)
        .map_err(|e| e.to_string())?;
    let offset = |ptr: i32| usize::try_from(ptr).map_err(|_| "negative wasm pointer".to_string());
    memory
        .write(&mut *store, offset(name_ptr)?, event.as_bytes())
        .map_err(|e| e.to_string())?;
    memory
        .write(&mut *store, offset(data_ptr)?, payload.as_bytes())
        .map_err(|e| e.to_string())?;
    let handler = instance
        .get_typed_func::<(i32, i32, i32, i32), i32>(&*store, "on_event")
        .map_err(|e| e.to_string())?;
    let status = handler
        .call(
            &mut *store,
            (name_ptr, event.len() as i32, data_ptr, payload.len() as i32),
        )
        .map_err(|e| e.to_string())?;
    free.call(&mut *store, (data_ptr, payload.len() as i32))
        .map_err(|e| e.to_string())?;
    free.call(&mut *store, (name_ptr, event.len() as i32))
        .map_err(|e| e.to_string())?;
    if status != 0 {
        return Err(format!("wasm event returned {status}"));
    }
    Ok(())
}

fn read_str(
    caller: &wasmi::Caller<'_, WasmHost>,
    ptr: i32,
    len: i32,
) -> Result<String, wasmi::Error> {
    let ptr = usize::try_from(ptr).map_err(|_| wasmi::Error::new("negative wasm pointer"))?;
    let len = usize::try_from(len).map_err(|_| wasmi::Error::new("negative wasm length"))?;
    if len > MAX_STRING {
        return Err(wasmi::Error::new("wasm string limit exceeded"));
    }
    let end = ptr
        .checked_add(len)
        .ok_or_else(|| wasmi::Error::new("wasm range overflow"))?;
    let memory = caller
        .get_export("memory")
        .and_then(wasmi::Extern::into_memory)
        .ok_or_else(|| wasmi::Error::new("guest must export memory"))?;
    let bytes = memory
        .data(caller)
        .get(ptr..end)
        .ok_or_else(|| wasmi::Error::new("wasm range outside memory"))?;
    std::str::from_utf8(bytes)
        .map(str::to_string)
        .map_err(|e| wasmi::Error::new(e.to_string()))
}

fn request(caller: &wasmi::Caller<'_, WasmHost>, ptr: i32, len: i32) -> Result<Json, wasmi::Error> {
    serde_json::from_str(&read_str(caller, ptr, len)?).map_err(|e| wasmi::Error::new(e.to_string()))
}

fn field<'a>(value: &'a Json, key: &str) -> Result<&'a str, wasmi::Error> {
    value
        .get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| wasmi::Error::new(format!("missing string field: {key}")))
}

fn write_str(caller: &mut wasmi::Caller<'_, WasmHost>, text: &str) -> Result<i64, wasmi::Error> {
    if text.len() > MAX_STRING {
        return Err(wasmi::Error::new("wasm string limit exceeded"));
    }
    let alloc = caller
        .get_export("alloc")
        .and_then(wasmi::Extern::into_func)
        .ok_or_else(|| wasmi::Error::new("guest must export alloc"))?
        .typed::<i32, i32>(&*caller)?;
    let memory = caller
        .get_export("memory")
        .and_then(wasmi::Extern::into_memory)
        .ok_or_else(|| wasmi::Error::new("guest must export memory"))?;
    let ptr = alloc.call(&mut *caller, text.len() as i32)?;
    let offset = usize::try_from(ptr).map_err(|_| wasmi::Error::new("negative wasm pointer"))?;
    memory
        .write(&mut *caller, offset, text.as_bytes())
        .map_err(|e| wasmi::Error::new(e.to_string()))?;
    Ok(((ptr as i64) << 32) | text.len() as i64)
}

fn build_linker(engine: &wasmi::Engine) -> Result<wasmi::Linker<WasmHost>, wasmi::Error> {
    let mut linker = wasmi::Linker::new(engine);
    linker.func_wrap(
        "env",
        "log",
        |caller: wasmi::Caller<'_, WasmHost>, ptr: i32, len: i32| -> Result<(), wasmi::Error> {
            caller.data().log.log(&read_str(&caller, ptr, len)?);
            Ok(())
        },
    )?;
    linker.func_wrap(
        "env",
        "publish",
        |caller: wasmi::Caller<'_, WasmHost>, ptr: i32, len: i32| -> Result<(), wasmi::Error> {
            let value = request(&caller, ptr, len)?;
            let name = field(&value, "name")?;
            if crate::manifest::RESERVED_PREFIXES
                .iter()
                .any(|p| name.starts_with(p))
            {
                caller
                    .data()
                    .log
                    .log(&format!("reserved event prefix dropped: {name}"));
                return Ok(());
            }
            caller.data().queue.borrow_mut().push_back(Cmd::Publish {
                name: name.into(),
                payload: value.get("payload").cloned().unwrap_or(Json::Null),
            });
            Ok(())
        },
    )?;
    linker.func_wrap(
        "env",
        "data_set",
        |caller: wasmi::Caller<'_, WasmHost>, ptr: i32, len: i32| -> Result<(), wasmi::Error> {
            let value = request(&caller, ptr, len)?;
            let key = field(&value, "key")?;
            let data = value
                .get("value")
                .cloned()
                .ok_or_else(|| wasmi::Error::new("missing value"))?;
            caller
                .data()
                .data
                .borrow_mut()
                .0
                .entry(caller.data().actor.clone())
                .or_default()
                .insert(key.into(), data);
            Ok(())
        },
    )?;
    linker.func_wrap(
        "env",
        "data_get",
        |mut caller: wasmi::Caller<'_, WasmHost>,
         ptr: i32,
         len: i32|
         -> Result<i64, wasmi::Error> {
            let value = request(&caller, ptr, len)?;
            let key = field(&value, "key")?;
            let data = caller
                .data()
                .data
                .borrow()
                .0
                .get(&caller.data().actor)
                .and_then(|r| r.get(key))
                .cloned()
                .unwrap_or(Json::Null);
            write_str(&mut caller, &data.to_string())
        },
    )?;
    linker.func_wrap(
        "env",
        "config_get",
        |mut caller: wasmi::Caller<'_, WasmHost>,
         ptr: i32,
         len: i32|
         -> Result<i64, wasmi::Error> {
            let value = request(&caller, ptr, len)?;
            let data = caller
                .data()
                .config
                .borrow()
                .get(field(&value, "section")?, field(&value, "key")?)
                .cloned()
                .unwrap_or(Json::Null);
            write_str(&mut caller, &data.to_string())
        },
    )?;
    linker.func_wrap(
        "env",
        "config_set",
        |caller: wasmi::Caller<'_, WasmHost>, ptr: i32, len: i32| -> Result<(), wasmi::Error> {
            let value = request(&caller, ptr, len)?;
            let data = value
                .get("value")
                .cloned()
                .ok_or_else(|| wasmi::Error::new("missing value"))?;
            caller.data().config.borrow_mut().set(
                field(&value, "section")?,
                field(&value, "key")?,
                data,
            );
            Ok(())
        },
    )?;
    Ok(linker)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(init: &str, pages: usize) -> String {
        format!(
            r#"(module
          (import "env" "log" (func $log (param i32 i32)))
          (memory (export "memory") {pages})
          (func (export "alloc") (param i32) (result i32) (i32.const 1024))
          (func (export "dealloc") (param i32 i32))
          (func (export "init") {init})
          (func (export "on_event") (param i32 i32 i32 i32) (result i32) (i32.const 0)))"#
        )
    }

    fn state() -> WasmHost {
        host_state(
            "probe",
            Logger::new("probe", Default::default()),
            Default::default(),
            std::rc::Rc::new(std::cell::RefCell::new(crate::host::DataStore(
                Default::default(),
            ))),
            std::rc::Rc::new(std::cell::RefCell::new(
                crate::host::ConfigStore::load(std::path::Path::new("unused-config.toml")).0,
            )),
        )
    }

    #[test]
    fn import_rejects_large_lengths_before_copying() {
        let text = module("(call $log (i32.const 0) (i32.const 2147483647))", 1);
        let error = instantiate(text.as_bytes(), state()).err().unwrap();
        assert!(error.contains("string limit"), "{error}");
    }

    #[test]
    fn linear_memory_is_limited_at_creation_and_growth() {
        assert!(instantiate(module("", 1025).as_bytes(), state()).is_err());
        assert!(
            instantiate(
                module("(drop (memory.grow (i32.const 1024)))", 1).as_bytes(),
                state()
            )
            .is_err()
        );
    }

    #[test]
    fn incomplete_abi_is_rejected_before_init() {
        assert!(
            check(b"(module (memory (export \"memory\") 1))")
                .unwrap_err()
                .contains("alloc")
        );
    }
}
