use crate::{
    Caller, CallerRef, Config, Engine, ExecutionConfig, ExecutionError, Gas, GasMeteringKind, Linker, Module, ModuleConfig, ProgramBlob,
    Reg, Trap, Val,
};
use core::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Mutex;

use polkavm_common::abi::{VM_ADDR_USER_MEMORY, VM_PAGE_SIZE};
use polkavm_common::elf::FnMetadata;
use polkavm_common::program::asm;
use polkavm_common::program::ExternTy::*;
use polkavm_common::program::Reg::*;
use polkavm_common::writer::ProgramBlobBuilder;

macro_rules! run_tests {
    ($($test_name:ident)+) => {
        if_compiler_is_supported! {
            mod compiler {
                #[cfg(target_os = "linux")]
                mod linux {
                    $(
                        #[test]
                        fn $test_name() {
                            let mut config = crate::Config::default();
                            config.set_backend(Some(crate::BackendKind::Compiler));
                            config.set_sandbox(Some(crate::SandboxKind::Linux));
                            super::super::$test_name(config);
                        }
                    )+
                }

                #[cfg(target_os = "linux")]
                mod linux_tracing {
                    $(
                        #[test]
                        fn $test_name() {
                            let mut config = crate::Config::default();
                            config.set_backend(Some(crate::BackendKind::Compiler));
                            config.set_sandbox(Some(crate::SandboxKind::Linux));
                            config.set_allow_insecure(true);
                            config.set_trace_execution(true);
                            super::super::$test_name(config);
                        }
                    )+
                }

                mod generic {
                    $(
                        #[test]
                        fn $test_name() {
                            let mut config = crate::Config::default();
                            config.set_backend(Some(crate::BackendKind::Compiler));
                            config.set_sandbox(Some(crate::SandboxKind::Generic));
                            config.set_allow_insecure(true);
                            super::super::$test_name(config);
                        }
                    )+
                }

                mod generic_tracing {
                    $(
                        #[test]
                        fn $test_name() {
                            let mut config = crate::Config::default();
                            config.set_backend(Some(crate::BackendKind::Compiler));
                            config.set_sandbox(Some(crate::SandboxKind::Generic));
                            config.set_allow_insecure(true);
                            config.set_trace_execution(true);
                            super::super::$test_name(config);
                        }
                    )+
                }
            }
        }

        mod interpreter {
            $(
                #[test]
                fn $test_name() {
                    let mut config = crate::Config::default();
                    config.set_backend(Some(crate::BackendKind::Interpreter));
                    super::$test_name(config);
                }
            )+
        }
    }
}

fn basic_test_blob() -> ProgramBlob<'static> {
    let mut builder = ProgramBlobBuilder::new();
    builder.set_bss_size(VM_PAGE_SIZE);
    builder.add_export(0, &FnMetadata::new("main", &[I32, I32], Some(I32)));
    builder.add_import(0, &FnMetadata::new("hostcall", &[], Some(I32)));
    builder.set_code(&[
        asm::store_imm_u32(0x12345678, VM_ADDR_USER_MEMORY),
        asm::add(S0, A0, A1),
        asm::ecalli(0),
        asm::add(A0, A0, S0),
        asm::ret(),
    ]);
    ProgramBlob::parse(builder.into_vec()).unwrap()
}

fn caller_and_caller_ref_work(config: Config) {
    let _ = env_logger::try_init();
    let blob = basic_test_blob();
    let engine = Engine::new(&config).unwrap();
    let module = Module::from_blob(&engine, &Default::default(), &blob).unwrap();
    let mut linker = Linker::new(&engine);

    #[derive(Default)]
    struct State {
        illegal_contraband: Rc<RefCell<Option<CallerRef<State>>>>,
    }

    linker
        .func_wrap("hostcall", move |caller: Caller<State>| -> Result<u32, Trap> {
            {
                let value = caller.read_u32(VM_ADDR_USER_MEMORY)?;
                assert_eq!(value, 0x12345678);
            }
            {
                let caller = caller.into_ref();
                let value = caller.read_u32(VM_ADDR_USER_MEMORY)?;
                assert_eq!(value, 0x12345678);

                let illegal_contraband = Rc::clone(&caller.data().illegal_contraband);
                *illegal_contraband.borrow_mut() = Some(caller);
            }

            Ok(100)
        })
        .unwrap();

    let instance_pre = linker.instantiate_pre(&module).unwrap();
    let instance = instance_pre.instantiate().unwrap();
    let mut state = State::default();
    let result = instance
        .get_typed_func::<(u32, u32), u32>("main")
        .unwrap()
        .call(&mut state, (1, 10))
        .unwrap();

    assert_eq!(result, 111);

    let caller = state.illegal_contraband.borrow_mut().take().unwrap();
    let result = std::panic::catch_unwind(core::panic::AssertUnwindSafe(|| caller.get_reg(Reg::A0)));
    assert!(result.is_err());
}

fn caller_split_works(config: Config) {
    let _ = env_logger::try_init();
    let blob = basic_test_blob();
    let engine = Engine::new(&config).unwrap();
    let module = Module::from_blob(&engine, &Default::default(), &blob).unwrap();
    let mut linker = Linker::new(&engine);

    #[derive(Default)]
    struct State {
        value: u32,
    }

    linker
        .func_wrap("hostcall", move |caller: Caller<State>| -> Result<u32, Trap> {
            {
                let value = caller.read_u32(VM_ADDR_USER_MEMORY)?;
                assert_eq!(value, 0x12345678);
            }
            {
                let (caller, state) = caller.split();
                state.value = caller.read_u32(VM_ADDR_USER_MEMORY)?;
            }

            Ok(100)
        })
        .unwrap();

    let instance_pre = linker.instantiate_pre(&module).unwrap();
    let instance = instance_pre.instantiate().unwrap();
    let mut state = State::default();
    let result = instance
        .get_typed_func::<(u32, u32), u32>("main")
        .unwrap()
        .call(&mut state, (1, 10))
        .unwrap();

    assert_eq!(result, 111);
    assert_eq!(state.value, 0x12345678);
}

fn trapping_from_hostcall_handler_works(config: Config) {
    let _ = env_logger::try_init();
    let blob = basic_test_blob();
    let engine = Engine::new(&config).unwrap();
    let module = Module::from_blob(&engine, &Default::default(), &blob).unwrap();
    let mut linker = Linker::new(&engine);

    enum Kind {
        Ok,
        Trap,
    }

    linker
        .func_wrap("hostcall", move |caller: Caller<Kind>| -> Result<u32, Trap> {
            match *caller.data() {
                Kind::Ok => Ok(100),
                Kind::Trap => Err(Trap::default()),
            }
        })
        .unwrap();

    let instance_pre = linker.instantiate_pre(&module).unwrap();
    let instance = instance_pre.instantiate().unwrap();

    let result = instance
        .get_typed_func::<(u32, u32), u32>("main")
        .unwrap()
        .call(&mut Kind::Ok, (1, 10));
    assert!(matches!(result, Ok(111)));

    let result = instance
        .get_typed_func::<(u32, u32), u32>("main")
        .unwrap()
        .call(&mut Kind::Trap, (1, 10));
    assert!(matches!(result, Err(ExecutionError::Trap(..))));

    let result = instance
        .get_func("main")
        .unwrap()
        .call(&mut Kind::Ok, &[Val::from(1), Val::from(10)]);
    assert!(matches!(result, Ok(Some(Val::I32(111)))));

    let result = instance
        .get_func("main")
        .unwrap()
        .call(&mut Kind::Trap, &[Val::from(1), Val::from(10)]);
    assert!(matches!(result, Err(ExecutionError::Trap(..))));
}

fn decompress_zstd(mut bytes: &[u8]) -> Vec<u8> {
    use std::io::Read;
    let mut output = Vec::new();
    ruzstd::streaming_decoder::StreamingDecoder::new(&mut bytes)
        .unwrap()
        .read_to_end(&mut output)
        .unwrap();
    output
}

static BLOB_MAP: Mutex<Option<HashMap<&'static [u8], ProgramBlob>>> = Mutex::new(None);

fn get_blob(elf: &'static [u8]) -> ProgramBlob {
    let mut blob_map = match BLOB_MAP.lock() {
        Ok(blob_map) => blob_map,
        Err(error) => error.into_inner(),
    };

    let blob_map = blob_map.get_or_insert_with(HashMap::new);
    blob_map
        .entry(elf)
        .or_insert_with(|| {
            // This is slow, so cache it.
            let elf = decompress_zstd(elf);
            let blob = polkavm_linker::program_from_elf(Default::default(), &elf).unwrap();
            blob.into_owned()
        })
        .clone()
}

fn doom(config: Config, elf: &'static [u8]) {
    if config.backend() == Some(crate::BackendKind::Interpreter) || config.trace_execution() {
        // The interpreter is currently too slow to run doom.
        return;
    }

    if cfg!(debug_assertions) {
        // The linker is currently very slow in debug mode.
        return;
    }

    const DOOM_WAD: &[u8] = include_bytes!("../../../examples/doom/roms/doom1.wad");

    let _ = env_logger::try_init();
    let blob = get_blob(elf);
    let engine = Engine::new(&config).unwrap();
    let module = Module::from_blob(&engine, &Default::default(), &blob).unwrap();
    let mut linker = Linker::new(&engine);

    struct State {
        frame: Vec<u8>,
        frame_width: u32,
        frame_height: u32,
    }

    linker
        .func_wrap(
            "ext_output_video",
            |caller: Caller<State>, address: u32, width: u32, height: u32| -> Result<(), Trap> {
                let (caller, state) = caller.split();
                let length = width * height * 4;
                state.frame.clear();
                state.frame.reserve(length as usize);
                caller.read_memory_into_slice(address, &mut state.frame.spare_capacity_mut()[..length as usize])?;
                // SAFETY: We've successfully read this many bytes into this Vec.
                unsafe {
                    state.frame.set_len(length as usize);
                }
                state.frame_width = width;
                state.frame_height = height;
                Ok(())
            },
        )
        .unwrap();

    linker
        .func_wrap(
            "ext_output_audio",
            |_caller: Caller<State>, _address: u32, _samples: u32| -> Result<(), Trap> { Ok(()) },
        )
        .unwrap();

    linker
        .func_wrap("ext_rom_size", |_caller: Caller<State>| -> u32 { DOOM_WAD.len() as u32 })
        .unwrap();

    linker
        .func_wrap(
            "ext_rom_read",
            |mut caller: Caller<State>, pointer: u32, offset: u32, length: u32| -> Result<(), Trap> {
                let chunk = DOOM_WAD
                    .get(offset as usize..offset as usize + length as usize)
                    .ok_or_else(Trap::default)?;

                caller.write_memory(pointer, chunk)
            },
        )
        .unwrap();

    linker
        .func_wrap(
            "ext_stdout",
            |_caller: Caller<State>, _buffer: u32, length: u32| -> Result<i32, Trap> { Ok(length as i32) },
        )
        .unwrap();

    let instance_pre = linker.instantiate_pre(&module).unwrap();
    let instance = instance_pre.instantiate().unwrap();
    let ext_initialize = instance.get_typed_func::<(), ()>("ext_initialize").unwrap();
    let ext_tick = instance.get_typed_func::<(), ()>("ext_tick").unwrap();

    let mut state = State {
        frame: Vec::new(),
        frame_width: 0,
        frame_height: 0,
    };

    ext_initialize.call(&mut state, ()).unwrap();
    for nth_frame in 0..=10440 {
        ext_tick.call(&mut state, ()).unwrap();

        let expected_frame_raw = match nth_frame {
            120 => decompress_zstd(include_bytes!("../../../test-data/doom_00120.tga.zst")),
            1320 => decompress_zstd(include_bytes!("../../../test-data/doom_01320.tga.zst")),
            9000 => decompress_zstd(include_bytes!("../../../test-data/doom_09000.tga.zst")),
            10440 => decompress_zstd(include_bytes!("../../../test-data/doom_10440.tga.zst")),
            _ => continue,
        };

        for pixel in state.frame.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            pixel[3] = 0xff;
        }

        let expected_frame = image::load_from_memory_with_format(&expected_frame_raw, image::ImageFormat::Tga)
            .unwrap()
            .to_rgba8();

        if state.frame != *expected_frame.as_raw() {
            panic!("frame {nth_frame:05} doesn't match!");
        }
    }

    // Generate frames to pick:
    // for nth_frame in 0..20000 {
    //     ext_tick.call(&mut state, ()).unwrap();
    //     if nth_frame % 120 == 0 {
    //         for pixel in state.frame.chunks_exact_mut(4) {
    //             pixel.swap(0, 2);
    //             pixel[3] = 0xff;
    //         }
    //         let filename = format!("/tmp/doom-frames/doom_{:05}.tga", nth_frame);
    //         image::save_buffer(filename, &state.frame, state.frame_width, state.frame_height, image::ColorType::Rgba8).unwrap();
    //     }
    // }
}

fn doom_o3_dwarf5(config: Config) {
    doom(config, include_bytes!("../../../test-data/doom_O3_dwarf5.elf.zst"));
}

fn doom_o1_dwarf5(config: Config) {
    doom(config, include_bytes!("../../../test-data/doom_O1_dwarf5.elf.zst"));
}

fn doom_o3_dwarf2(config: Config) {
    doom(config, include_bytes!("../../../test-data/doom_O3_dwarf2.elf.zst"));
}

fn pinky(config: Config) {
    if config.backend() == Some(crate::BackendKind::Interpreter) || config.trace_execution() {
        // The interpreter is currently too slow to run this.
        return;
    }

    let _ = env_logger::try_init();
    let blob = get_blob(include_bytes!("../../../test-data/bench-pinky.elf.zst"));

    let engine = Engine::new(&config).unwrap();
    let module = Module::from_blob(&engine, &Default::default(), &blob).unwrap();
    let linker = Linker::new(&engine);
    let instance_pre = linker.instantiate_pre(&module).unwrap();
    let instance = instance_pre.instantiate().unwrap();
    let ext_initialize = instance.get_typed_func::<(), ()>("initialize").unwrap();
    let ext_run = instance.get_typed_func::<(), ()>("run").unwrap();
    let ext_get_framebuffer = instance.get_typed_func::<(), u32>("get_framebuffer").unwrap();

    ext_initialize.call(&mut (), ()).unwrap();
    for _ in 0..256 {
        ext_run.call(&mut (), ()).unwrap();
    }

    let address = ext_get_framebuffer.call(&mut (), ()).unwrap();
    let framebuffer = instance.read_memory_into_new_vec(address, 256 * 240 * 4).unwrap();

    let expected_frame_raw = decompress_zstd(include_bytes!("../../../test-data/pinky_00256.tga.zst"));
    let expected_frame = image::load_from_memory_with_format(&expected_frame_raw, image::ImageFormat::Tga)
        .unwrap()
        .to_rgba8();

    if framebuffer != *expected_frame.as_raw() {
        panic!("frames doesn't match!");
    }
}

struct TestInstance {
    instance: crate::Instance<()>,
}

impl TestInstance {
    fn new(config: &Config) -> Self {
        let _ = env_logger::try_init();
        let blob = get_blob(include_bytes!("../../../test-data/test-blob.elf.zst"));

        let engine = Engine::new(config).unwrap();
        let module = Module::from_blob(&engine, &Default::default(), &blob).unwrap();
        let mut linker = Linker::new(&engine);
        linker
            .func_wrap("multiply_by_2", |_caller: Caller<()>, value: u32| -> Result<u32, Trap> {
                Ok(value * 2)
            })
            .unwrap();

        let instance_pre = linker.instantiate_pre(&module).unwrap();
        let instance = instance_pre.instantiate().unwrap();

        TestInstance { instance }
    }

    pub fn call<FnArgs, FnResult>(&self, name: &str, args: FnArgs) -> Result<FnResult, crate::ExecutionError<crate::Error>>
    where
        FnArgs: crate::api::FuncArgs,
        FnResult: crate::api::FuncResult,
    {
        let function = self
            .instance
            .get_typed_func::<FnArgs, FnResult>(name)
            .expect("function doesn't exist");
        function.call(&mut (), args)
    }
}

fn test_blob_basic_test(config: Config) {
    let i = TestInstance::new(&config);
    assert_eq!(i.call::<(), u32>("push_one_to_global_vec", ()).unwrap(), 1);
    assert_eq!(i.call::<(), u32>("push_one_to_global_vec", ()).unwrap(), 2);
    assert_eq!(i.call::<(), u32>("push_one_to_global_vec", ()).unwrap(), 3);
}

fn test_blob_atomic_fetch_add(config: Config) {
    let i = TestInstance::new(&config);
    assert_eq!(i.call::<(u32,), u32>("atomic_fetch_add", (1,)).unwrap(), 0);
    assert_eq!(i.call::<(u32,), u32>("atomic_fetch_add", (1,)).unwrap(), 1);
    assert_eq!(i.call::<(u32,), u32>("atomic_fetch_add", (1,)).unwrap(), 2);
    assert_eq!(i.call::<(u32,), u32>("atomic_fetch_add", (0,)).unwrap(), 3);
    assert_eq!(i.call::<(u32,), u32>("atomic_fetch_add", (0,)).unwrap(), 3);
    assert_eq!(i.call::<(u32,), u32>("atomic_fetch_add", (2,)).unwrap(), 3);
    assert_eq!(i.call::<(u32,), u32>("atomic_fetch_add", (0,)).unwrap(), 5);
}

fn test_blob_atomic_fetch_swap(config: Config) {
    let i = TestInstance::new(&config);
    assert_eq!(i.call::<(u32,), u32>("atomic_fetch_swap", (10,)).unwrap(), 0);
    assert_eq!(i.call::<(u32,), u32>("atomic_fetch_swap", (100,)).unwrap(), 10);
    assert_eq!(i.call::<(u32,), u32>("atomic_fetch_swap", (1000,)).unwrap(), 100);
}

fn test_blob_atomic_fetch_minmax(config: Config) {
    use core::cmp::{max, min};

    fn maxu(a: i32, b: i32) -> i32 {
        max(a as u32, b as u32) as i32
    }

    fn minu(a: i32, b: i32) -> i32 {
        min(a as u32, b as u32) as i32
    }

    #[allow(clippy::type_complexity)]
    let list: [(&str, fn(i32, i32) -> i32); 4] = [
        ("atomic_fetch_max_signed", max),
        ("atomic_fetch_min_signed", min),
        ("atomic_fetch_max_unsigned", maxu),
        ("atomic_fetch_min_unsigned", minu),
    ];

    let i = TestInstance::new(&config);
    for (name, cb) in list {
        for a in [-10, 0, 10] {
            for b in [-10, 0, 10] {
                let new_value = cb(a, b);
                i.call::<(i32,), ()>("set_atomic_global", (a,)).unwrap();
                assert_eq!(i.call::<(i32,), i32>(name, (b,)).unwrap(), a);
                assert_eq!(i.call::<(), i32>("get_atomic_global", ()).unwrap(), new_value);
            }
        }
    }
}

fn test_blob_hostcall(config: Config) {
    let i = TestInstance::new(&config);
    assert_eq!(i.call::<(u32,), u32>("test_multiply_by_6", (10,)).unwrap(), 60);
}

fn basic_gas_metering(config: Config, gas_metering_kind: GasMeteringKind) {
    let _ = env_logger::try_init();

    let mut builder = ProgramBlobBuilder::new();
    builder.add_export(0, &FnMetadata::new("main", &[], Some(I32)));
    builder.set_code(&[asm::add_imm(A0, A0, 666), asm::ret()]);

    let blob = ProgramBlob::parse(builder.into_vec()).unwrap();
    let engine = Engine::new(&config).unwrap();
    let mut module_config = ModuleConfig::default();
    module_config.set_gas_metering(Some(gas_metering_kind));

    let module = Module::from_blob(&engine, &module_config, &blob).unwrap();
    let linker = Linker::new(&engine);
    let instance_pre = linker.instantiate_pre(&module).unwrap();
    let instance = instance_pre.instantiate().unwrap();

    {
        let mut config = ExecutionConfig::default();
        config.set_gas(Gas::new(2).unwrap());

        let result = instance.get_typed_func::<(), i32>("main").unwrap().call_ex(&mut (), (), config);
        assert!(matches!(result, Ok(666)), "unexpected result: {result:?}");
        assert_eq!(instance.gas_remaining().unwrap(), Gas::new(0).unwrap());
    }

    {
        let mut config = ExecutionConfig::default();
        config.set_gas(Gas::new(1).unwrap());

        let result = instance.get_typed_func::<(), i32>("main").unwrap().call_ex(&mut (), (), config);
        assert!(matches!(result, Err(ExecutionError::OutOfGas)), "unexpected result: {result:?}");
        assert_eq!(instance.gas_remaining().unwrap(), Gas::new(0).unwrap());
    }

    {
        let mut config = ExecutionConfig::default();
        config.set_gas(Gas::new(4).unwrap());

        let result = instance.get_typed_func::<(), i32>("main").unwrap().call_ex(&mut (), (), config);
        assert!(matches!(result, Ok(666)), "unexpected result: {result:?}");
        assert_eq!(instance.gas_remaining().unwrap(), Gas::new(2).unwrap());

        let result = instance.get_typed_func::<(), i32>("main").unwrap().call(&mut (), ());
        assert!(matches!(result, Ok(666)), "unexpected result: {result:?}");
        assert_eq!(instance.gas_remaining().unwrap(), Gas::new(0).unwrap());

        let result = instance.get_typed_func::<(), i32>("main").unwrap().call(&mut (), ());
        assert_eq!(instance.gas_remaining().unwrap(), Gas::new(0).unwrap());
        assert!(matches!(result, Err(ExecutionError::OutOfGas)), "unexpected result: {result:?}");
    }

    {
        core::mem::drop(instance);
        let instance = instance_pre.instantiate().unwrap();
        assert_eq!(instance.gas_remaining().unwrap(), Gas::new(0).unwrap());

        let result = instance.get_typed_func::<(), i32>("main").unwrap().call(&mut (), ());
        assert!(matches!(result, Err(ExecutionError::OutOfGas)), "unexpected result: {result:?}");
        assert_eq!(instance.gas_remaining().unwrap(), Gas::new(0).unwrap());
    }
}

fn basic_gas_metering_sync(config: Config) {
    basic_gas_metering(config, GasMeteringKind::Sync);
}

fn basic_gas_metering_async(config: Config) {
    basic_gas_metering(config, GasMeteringKind::Async);
}

fn consume_gas_in_host_function(config: Config, gas_metering_kind: GasMeteringKind) {
    let _ = env_logger::try_init();

    let mut builder = ProgramBlobBuilder::new();
    builder.add_export(0, &FnMetadata::new("main", &[], Some(I32)));
    builder.add_import(0, &FnMetadata::new("hostfn", &[], Some(I32)));
    builder.set_code(&[asm::ecalli(0), asm::ret()]);

    let blob = ProgramBlob::parse(builder.into_vec()).unwrap();
    let engine = Engine::new(&config).unwrap();
    let mut module_config = ModuleConfig::default();
    module_config.set_gas_metering(Some(gas_metering_kind));

    let module = Module::from_blob(&engine, &module_config, &blob).unwrap();
    let mut linker = Linker::new(&engine);
    linker
        .func_wrap("hostfn", |mut caller: Caller<u64>| -> u32 {
            assert_eq!(caller.gas_remaining().unwrap().get(), 1);
            caller.consume_gas(*caller.data());
            666
        })
        .unwrap();

    let instance_pre = linker.instantiate_pre(&module).unwrap();
    let instance = instance_pre.instantiate().unwrap();

    {
        let mut config = ExecutionConfig::default();
        config.set_gas(Gas::new(3).unwrap());

        let result = instance.get_typed_func::<(), i32>("main").unwrap().call_ex(&mut 0, (), config);
        assert!(matches!(result, Ok(666)), "unexpected result: {result:?}");
        assert_eq!(instance.gas_remaining().unwrap(), Gas::new(1).unwrap());
    }

    {
        let mut config = ExecutionConfig::default();
        config.set_gas(Gas::new(3).unwrap());

        let result = instance.get_typed_func::<(), i32>("main").unwrap().call_ex(&mut 1, (), config);
        assert!(matches!(result, Ok(666)), "unexpected result: {result:?}");
        assert_eq!(instance.gas_remaining().unwrap(), Gas::new(0).unwrap());
    }

    {
        let mut config = ExecutionConfig::default();
        config.set_gas(Gas::new(3).unwrap());

        let result = instance.get_typed_func::<(), i32>("main").unwrap().call_ex(&mut 2, (), config);
        assert_eq!(instance.gas_remaining().unwrap(), Gas::new(0).unwrap());
        assert!(matches!(result, Err(ExecutionError::OutOfGas)), "unexpected result: {result:?}");
    }
}

fn consume_gas_in_host_function_sync(config: Config) {
    consume_gas_in_host_function(config, GasMeteringKind::Sync);
}

fn consume_gas_in_host_function_async(config: Config) {
    consume_gas_in_host_function(config, GasMeteringKind::Async);
}

run_tests! {
    caller_and_caller_ref_work
    caller_split_works
    trapping_from_hostcall_handler_works
    doom_o3_dwarf5
    doom_o1_dwarf5
    doom_o3_dwarf2
    pinky

    test_blob_basic_test
    test_blob_atomic_fetch_add
    test_blob_atomic_fetch_swap
    test_blob_atomic_fetch_minmax
    test_blob_hostcall

    basic_gas_metering_sync
    basic_gas_metering_async
    consume_gas_in_host_function_sync
    consume_gas_in_host_function_async
}

// Source: https://users.rust-lang.org/t/a-macro-to-assert-that-a-type-does-not-implement-trait-bounds/31179
macro_rules! assert_not_impl {
    ($x:ty, $($t:path),+ $(,)*) => {
        const _: fn() -> () = || {
            struct Check<T: ?Sized>(T);
            trait AmbiguousIfImpl<A> { fn some_item() { } }

            impl<T: ?Sized> AmbiguousIfImpl<()> for Check<T> { }
            impl<T: ?Sized $(+ $t)*> AmbiguousIfImpl<u8> for Check<T> { }

            <Check::<$x> as AmbiguousIfImpl<_>>::some_item()
        };
    };
}

macro_rules! assert_impl {
    ($x:ty, $($t:path),+ $(,)*) => {
        const _: fn() -> () = || {
            struct Check where $x: $($t),+;
        };
    };
}

macro_rules! assert_send_sync {
    ($($x: ty,)+) => {
        $(
            assert_impl!($x, Send);
            assert_impl!($x, Sync);
        )+
    }
}

assert_send_sync! {
    crate::Config,
    crate::Engine,
    crate::Error,
    crate::ExecutionConfig,
    crate::Func<()>,
    crate::Gas,
    crate::Instance<()>,
    crate::InstancePre<()>,
    crate::Linker<()>,
    crate::Module,
    crate::ModuleConfig,
    crate::ProgramBlob<'static>,
    crate::Trap,
    crate::TypedFunc<(), (), ()>,
}

assert_not_impl!(crate::Caller<'static, ()>, Send);
assert_not_impl!(crate::Caller<'static, ()>, Sync);
assert_not_impl!(crate::CallerRef<()>, Send);
assert_not_impl!(crate::CallerRef<()>, Sync);
